//! Durable, TTL-backed presence store on top of Redis.
//!
//! The in-process [`Hub`](crate::hub::Hub) is the authority on *who is
//! connected* and routes live messages. This store is the authority on *what
//! status each user has*: it mirrors presence into Redis so a status survives
//! a process restart and, crucially, expires on its own if the owning server
//! dies without running the clean-disconnect path. A per-connection heartbeat
//! re-arms the TTL while the user stays online; a crash stops the heartbeat,
//! and the stale `online` key fades to offline once the TTL lapses.
//!
//! Every operation is best-effort. Redis being unreachable must never break
//! the chat path, so errors are logged and swallowed. A [`Presence::disabled`]
//! store turns every call into a no-op — used by deployments that run without
//! Redis and by tests that don't exercise persistence.

use std::time::Duration;

use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use tracing::warn;
use uuid::Uuid;

use concord_shared::protocol::UserPresence;
use concord_shared::types::UserStatus;

const DEFAULT_TTL: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct Presence {
    /// `None` => the store is disabled and every call is a no-op. Cloning a
    /// `ConnectionManager` is cheap: it shares one multiplexed connection.
    conn: Option<ConnectionManager>,
    ttl: Duration,
}

fn key(user_id: Uuid) -> String {
    format!("presence:{user_id}")
}

impl Presence {
    /// A disabled store. Every method returns immediately without touching
    /// Redis.
    pub fn disabled() -> Self {
        Self { conn: None, ttl: DEFAULT_TTL }
    }

    /// Open a Redis connection manager and wrap it. The manager reconnects
    /// transparently, so a transient Redis blip degrades to best-effort writes
    /// rather than a hard failure.
    pub async fn connect(url: &str, ttl: Duration) -> redis::RedisResult<Self> {
        let client = redis::Client::open(url)?;
        let conn = ConnectionManager::new(client).await?;
        Ok(Self { conn: Some(conn), ttl })
    }

    pub fn is_enabled(&self) -> bool {
        self.conn.is_some()
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Persist `status` for `user_id`. Offline is represented by the *absence*
    /// of a key, so an offline status deletes the entry rather than storing it;
    /// any other status is written with a fresh TTL.
    pub async fn set(&self, user_id: Uuid, status: UserStatus) {
        let Some(mut conn) = self.conn.clone() else {
            return;
        };
        let res: redis::RedisResult<()> = if status == UserStatus::Offline {
            conn.del(key(user_id)).await
        } else {
            conn.set_ex(key(user_id), status.to_string(), self.ttl.as_secs())
                .await
        };
        if let Err(e) = res {
            warn!(user_id = %user_id, error = %e, "failed to write presence to redis");
        }
    }

    /// Remove a user's presence entry. Used on the clean-disconnect path.
    pub async fn clear(&self, user_id: Uuid) {
        self.set(user_id, UserStatus::Offline).await;
    }

    /// Re-arm the TTL on a user's existing entry without changing its value.
    /// `EXPIRE` on a missing key is a harmless no-op, so a user who manually
    /// went offline (key deleted) is not resurrected by the heartbeat.
    pub async fn refresh(&self, user_id: Uuid) {
        let Some(mut conn) = self.conn.clone() else {
            return;
        };
        let res: redis::RedisResult<bool> =
            conn.expire(key(user_id), self.ttl.as_secs() as i64).await;
        if let Err(e) = res {
            warn!(user_id = %user_id, error = %e, "failed to refresh presence ttl");
        }
    }

    /// Read the current status of each user in `user_ids`. Users with no entry
    /// (offline) are omitted, so the returned vector lists only non-offline
    /// peers. Order is not significant.
    pub async fn get_many(&self, user_ids: &[Uuid]) -> Vec<UserPresence> {
        if user_ids.is_empty() {
            return Vec::new();
        }
        let Some(mut conn) = self.conn.clone() else {
            return Vec::new();
        };
        let keys: Vec<String> = user_ids.iter().copied().map(key).collect();
        let values: Vec<Option<String>> = match conn.mget(&keys).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "failed to read presence from redis");
                return Vec::new();
            }
        };
        user_ids
            .iter()
            .zip(values)
            .filter_map(|(&user_id, value)| {
                let status: UserStatus = value?.parse().ok()?;
                if status == UserStatus::Offline {
                    return None;
                }
                Some(UserPresence { user_id, status })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Redis-backed tests need a reachable server; skip cleanly when `REDIS_URL`
    /// is absent so the default `cargo test` run stays green without infra.
    fn redis_url() -> Option<String> {
        std::env::var("REDIS_URL").ok().filter(|s| !s.is_empty())
    }

    #[tokio::test]
    async fn disabled_store_is_noop() {
        let p = Presence::disabled();
        let uid = Uuid::new_v4();
        // None of these should touch Redis or panic.
        p.set(uid, UserStatus::Online).await;
        p.refresh(uid).await;
        p.clear(uid).await;
        assert!(!p.is_enabled());
        assert!(p.get_many(&[uid]).await.is_empty());
    }

    #[tokio::test]
    async fn set_get_clear_roundtrip() {
        let Some(url) = redis_url() else {
            eprintln!("skipping set_get_clear_roundtrip: REDIS_URL not set");
            return;
        };
        let p = Presence::connect(&url, Duration::from_secs(60)).await.unwrap();
        let uid = Uuid::new_v4();

        p.set(uid, UserStatus::Online).await;
        let got = p.get_many(&[uid]).await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].user_id, uid);
        assert_eq!(got[0].status, UserStatus::Online);

        // Overwriting updates the stored value.
        p.set(uid, UserStatus::Dnd).await;
        assert_eq!(p.get_many(&[uid]).await[0].status, UserStatus::Dnd);

        // Offline deletes the entry, so it drops out of the result.
        p.clear(uid).await;
        assert!(p.get_many(&[uid]).await.is_empty());
    }

    #[tokio::test]
    async fn entry_expires_after_ttl() {
        let Some(url) = redis_url() else {
            eprintln!("skipping entry_expires_after_ttl: REDIS_URL not set");
            return;
        };
        let p = Presence::connect(&url, Duration::from_secs(1)).await.unwrap();
        let uid = Uuid::new_v4();

        p.set(uid, UserStatus::Online).await;
        assert_eq!(p.get_many(&[uid]).await.len(), 1);

        // Without a heartbeat re-arming it, the entry should expire on its own —
        // this is the crash-recovery property.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            p.get_many(&[uid]).await.is_empty(),
            "presence entry should have expired after its ttl"
        );
    }
}
