//! Typing indicators (issue #19).
//!
//! Tracks a short-lived "user X is typing in channel C" session per
//! `(user, channel)` pair and fans `TypingStarted` / `TypingStopped` out to the
//! channel's subscribers, excluding the user who triggered it.
//!
//! A session lives for [`TYPING_TTL`]; clients keep it alive by re-sending
//! `StartTyping` while the user keeps typing. A background sweeper expires
//! stale sessions and emits `TypingStopped` for them.
//!
//! When a Redis connection is configured every event round-trips through a
//! pub/sub channel so all server instances fan it out to their own locally
//! connected subscribers. Without Redis the fan-out happens in-process only.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use futures_util::StreamExt;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tracing::{error, warn};
use uuid::Uuid;

use concord_shared::protocol::ServerMsg;

use crate::hub::Hub;

/// How long a typing session stays live without a refreshing `StartTyping`.
pub const TYPING_TTL: Duration = Duration::from_secs(5);

/// How often the sweeper scans for expired typing sessions.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// Redis pub/sub channel carrying cross-instance typing events.
const PUBSUB_CHANNEL: &str = "concord:typing";

/// One typing transition, as serialized over Redis pub/sub.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct TypingEvent {
    channel_id: Uuid,
    user_id: Uuid,
    started: bool,
}

impl TypingEvent {
    fn into_server_msg(self) -> ServerMsg {
        let TypingEvent { channel_id, user_id, started } = self;
        if started {
            ServerMsg::TypingStarted { channel_id, user_id }
        } else {
            ServerMsg::TypingStopped { channel_id, user_id }
        }
    }
}

/// Cross-instance transport for typing events. Falls back to in-process
/// fan-out when Redis is not configured.
enum Transport {
    Local,
    Redis(redis::aio::ConnectionManager),
}

pub struct Typing {
    hub: Arc<Hub>,
    /// `(user, channel)` -> deadline at which the session auto-expires.
    states: DashMap<(Uuid, Uuid), Instant>,
    ttl: Duration,
    transport: Transport,
}

impl Typing {
    pub fn new(hub: Arc<Hub>, ttl: Duration, redis: Option<redis::aio::ConnectionManager>) -> Self {
        Self {
            hub,
            states: DashMap::new(),
            ttl,
            transport: match redis {
                Some(conn) => Transport::Redis(conn),
                None => Transport::Local,
            },
        }
    }

    /// Record a `StartTyping`. Always refreshes the expiry, but only broadcasts
    /// `TypingStarted` when this opens a fresh session — so the periodic
    /// re-sends a typing client makes don't re-announce on every keystroke.
    pub async fn start(&self, user_id: Uuid, channel_id: Uuid) {
        let now = Instant::now();
        let prev = self.states.insert((user_id, channel_id), now + self.ttl);
        let fresh = match prev {
            None => true,
            Some(deadline) => deadline <= now,
        };
        if fresh {
            self.publish(TypingEvent { channel_id, user_id, started: true }).await;
        }
    }

    /// Record an explicit `StopTyping`; broadcasts `TypingStopped` only if the
    /// user actually had a live session. Intentionally does no membership
    /// check: it only ever clears the caller's own state, and skipping the
    /// check avoids leaving a stuck indicator if the user lost access while
    /// typing.
    pub async fn stop(&self, user_id: Uuid, channel_id: Uuid) {
        if self.states.remove(&(user_id, channel_id)).is_some() {
            self.publish(TypingEvent { channel_id, user_id, started: false }).await;
        }
    }

    /// Expire sessions whose deadline has passed, emitting `TypingStopped` for
    /// each. Run on a fixed interval by the sweeper task.
    async fn sweep(&self) {
        let now = Instant::now();
        let expired: Vec<(Uuid, Uuid)> = self
            .states
            .iter()
            .filter(|entry| *entry.value() <= now)
            .map(|entry| *entry.key())
            .collect();

        for key in expired {
            // `remove_if` rechecks the deadline atomically: a `StartTyping` may
            // have refreshed this session between the scan above and now, in
            // which case it stays alive and we skip the stop.
            if self.states.remove_if(&key, |_, deadline| *deadline <= now).is_some() {
                let (user_id, channel_id) = key;
                self.publish(TypingEvent { channel_id, user_id, started: false }).await;
            }
        }
    }

    /// Publish a typing event. With Redis the event round-trips through pub/sub
    /// so every instance (including this one, via its subscriber) fans it out;
    /// without Redis it is fanned out in-process immediately.
    async fn publish(&self, event: TypingEvent) {
        match &self.transport {
            Transport::Local => self.fan_out(event),
            Transport::Redis(conn) => {
                let payload = match serde_json::to_string(&event) {
                    Ok(p) => p,
                    Err(e) => {
                        error!(error = ?e, "failed to serialize typing event");
                        return;
                    }
                };
                let mut conn = conn.clone();
                if let Err(e) = conn.publish::<_, _, ()>(PUBSUB_CHANNEL, payload).await {
                    // Don't drop the indicator on a Redis hiccup: fall back to a
                    // local fan-out so same-instance subscribers still update.
                    warn!(error = ?e, "redis publish failed; falling back to local fan-out");
                    self.fan_out(event);
                }
            }
        }
    }

    /// Deliver a typing event to locally connected channel subscribers, never
    /// echoing it back to the user who triggered it.
    fn fan_out(&self, event: TypingEvent) {
        self.hub.broadcast_to_channel_except(
            event.channel_id,
            Some(event.user_id),
            &event.into_server_msg(),
        );
    }

    /// Spawn the background task that expires stale typing sessions.
    pub fn spawn_sweeper(self: Arc<Self>, interval: Duration) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            loop {
                tick.tick().await;
                self.sweep().await;
            }
        })
    }
}

/// Spawn the Redis pub/sub subscriber that fans cross-instance typing events
/// out to this instance's locally connected clients. Reconnects on error.
pub fn spawn_subscriber(client: redis::Client, typing: Arc<Typing>) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(e) = run_subscriber(&client, &typing).await {
                error!(error = ?e, "typing pub/sub subscriber failed; reconnecting");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
}

async fn run_subscriber(client: &redis::Client, typing: &Arc<Typing>) -> redis::RedisResult<()> {
    let mut pubsub = client.get_async_pubsub().await?;
    pubsub.subscribe(PUBSUB_CHANNEL).await?;
    let mut stream = pubsub.on_message();
    while let Some(msg) = stream.next().await {
        let payload: String = match msg.get_payload() {
            Ok(p) => p,
            Err(e) => {
                warn!(error = ?e, "typing event with non-string payload");
                continue;
            }
        };
        match serde_json::from_str::<TypingEvent>(&payload) {
            Ok(event) => typing.fan_out(event),
            Err(e) => warn!(error = ?e, "invalid typing event payload"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::sync::mpsc::UnboundedReceiver;

    /// In-process `Typing` with a short TTL so expiry is testable quickly.
    fn setup(ttl: Duration) -> (Arc<Hub>, Arc<Typing>) {
        let hub = Arc::new(Hub::new());
        let typing = Arc::new(Typing::new(Arc::clone(&hub), ttl, None));
        (hub, typing)
    }

    /// Register a user, subscribe them to `channel`, return their event receiver.
    fn join(hub: &Hub, user: Uuid, channel: Uuid) -> UnboundedReceiver<ServerMsg> {
        let (_conn, rx) = hub.register(user);
        hub.subscribe(user, channel);
        rx
    }

    #[tokio::test]
    async fn start_broadcasts_to_others_excluding_sender() {
        let (hub, typing) = setup(TYPING_TTL);
        let channel = Uuid::new_v4();
        let alice = Uuid::new_v4();
        let bob = Uuid::new_v4();
        let mut alice_rx = join(&hub, alice, channel);
        let mut bob_rx = join(&hub, bob, channel);

        typing.start(alice, channel).await;

        match bob_rx.try_recv() {
            Ok(ServerMsg::TypingStarted { channel_id, user_id }) => {
                assert_eq!(channel_id, channel);
                assert_eq!(user_id, alice);
            }
            other => panic!("bob expected TypingStarted, got {other:?}"),
        }
        assert!(
            alice_rx.try_recv().is_err(),
            "sender must not receive her own typing indicator"
        );
    }

    #[tokio::test]
    async fn repeated_start_within_ttl_is_deduped() {
        let (hub, typing) = setup(TYPING_TTL);
        let channel = Uuid::new_v4();
        let alice = Uuid::new_v4();
        let bob = Uuid::new_v4();
        let _alice_rx = join(&hub, alice, channel);
        let mut bob_rx = join(&hub, bob, channel);

        typing.start(alice, channel).await;
        assert!(matches!(bob_rx.try_recv(), Ok(ServerMsg::TypingStarted { .. })));

        // Re-send while still typing: refreshes the deadline, no new broadcast.
        typing.start(alice, channel).await;
        assert!(bob_rx.try_recv().is_err(), "re-send must not re-announce");
    }

    #[tokio::test]
    async fn stop_broadcasts_typing_stopped() {
        let (hub, typing) = setup(TYPING_TTL);
        let channel = Uuid::new_v4();
        let alice = Uuid::new_v4();
        let bob = Uuid::new_v4();
        let _alice_rx = join(&hub, alice, channel);
        let mut bob_rx = join(&hub, bob, channel);

        typing.start(alice, channel).await;
        let _ = bob_rx.try_recv();

        typing.stop(alice, channel).await;
        match bob_rx.try_recv() {
            Ok(ServerMsg::TypingStopped { channel_id, user_id }) => {
                assert_eq!(channel_id, channel);
                assert_eq!(user_id, alice);
            }
            other => panic!("bob expected TypingStopped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stop_without_active_session_is_silent() {
        let (hub, typing) = setup(TYPING_TTL);
        let channel = Uuid::new_v4();
        let alice = Uuid::new_v4();
        let bob = Uuid::new_v4();
        let mut bob_rx = join(&hub, bob, channel);
        let _alice_rx = join(&hub, alice, channel);

        typing.stop(alice, channel).await;
        assert!(bob_rx.try_recv().is_err(), "stop with no live session is a no-op");
    }

    #[tokio::test]
    async fn sweep_expires_stale_session() {
        let (hub, typing) = setup(Duration::from_millis(50));
        let channel = Uuid::new_v4();
        let alice = Uuid::new_v4();
        let bob = Uuid::new_v4();
        let _alice_rx = join(&hub, alice, channel);
        let mut bob_rx = join(&hub, bob, channel);

        typing.start(alice, channel).await;
        let _ = bob_rx.try_recv();

        tokio::time::sleep(Duration::from_millis(80)).await;
        typing.sweep().await;

        match bob_rx.try_recv() {
            Ok(ServerMsg::TypingStopped { user_id, .. }) => assert_eq!(user_id, alice),
            other => panic!("expected TypingStopped on expiry, got {other:?}"),
        }
        // Idempotent: a second sweep has nothing left to expire.
        typing.sweep().await;
        assert!(bob_rx.try_recv().is_err());
    }

    /// Exercises the real Redis publish -> subscribe -> fan-out round-trip.
    /// Skipped unless `REDIS_URL` is set, since it needs a live Redis.
    #[tokio::test]
    async fn redis_round_trip_fans_out_to_subscribers() {
        let Ok(url) = std::env::var("REDIS_URL") else {
            eprintln!("skipping redis_round_trip_fans_out_to_subscribers: REDIS_URL unset");
            return;
        };

        let client = redis::Client::open(url).expect("valid REDIS_URL");
        let manager = redis::aio::ConnectionManager::new(client.clone())
            .await
            .expect("redis connection");

        let hub = Arc::new(Hub::new());
        let typing = Arc::new(Typing::new(Arc::clone(&hub), TYPING_TTL, Some(manager)));
        spawn_subscriber(client, Arc::clone(&typing));

        let channel = Uuid::new_v4();
        let typist = Uuid::new_v4();
        let viewer = Uuid::new_v4();
        let mut viewer_rx = join(&hub, viewer, channel);

        // Wait for the subscriber's SUBSCRIBE to land; Redis drops messages
        // published while a channel has no subscribers.
        tokio::time::sleep(Duration::from_millis(300)).await;

        typing.start(typist, channel).await;

        let got = tokio::time::timeout(Duration::from_secs(2), viewer_rx.recv())
            .await
            .expect("timed out waiting for cross-instance typing event");
        match got {
            Some(ServerMsg::TypingStarted { channel_id, user_id }) => {
                assert_eq!(channel_id, channel);
                assert_eq!(user_id, typist);
            }
            other => panic!("expected TypingStarted via redis, got {other:?}"),
        }
    }
}
