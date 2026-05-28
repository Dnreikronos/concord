use std::collections::HashSet;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::mpsc;
use uuid::Uuid;

use concord_shared::protocol::ServerMsg;

const TYPING_COOLDOWN: Duration = Duration::from_secs(5);

pub struct Hub {
    senders: DashMap<Uuid, mpsc::UnboundedSender<ServerMsg>>,
    user_conns: DashMap<Uuid, HashSet<Uuid>>,
    channels: DashMap<Uuid, HashSet<Uuid>>,
    typing_cooldowns: DashMap<(Uuid, Uuid), Instant>,
}

impl Hub {
    pub fn new() -> Self {
        Self {
            senders: DashMap::new(),
            user_conns: DashMap::new(),
            channels: DashMap::new(),
            typing_cooldowns: DashMap::new(),
        }
    }

    /// Register a new connection for `user_id`. The returned `bool` is `true`
    /// when this is the user's *first* live connection — the offline-to-online
    /// transition that presence broadcasts hang off of. Additional connections
    /// (other devices/tabs) return `false`.
    pub fn register(
        &self,
        user_id: Uuid,
    ) -> (Uuid, mpsc::UnboundedReceiver<ServerMsg>, bool) {
        let conn_id = Uuid::new_v4();
        let (tx, rx) = mpsc::unbounded_channel();
        self.senders.insert(conn_id, tx);
        let mut conns = self.user_conns.entry(user_id).or_default();
        let is_first = conns.is_empty();
        conns.insert(conn_id);
        (conn_id, rx, is_first)
    }

    /// Drop a connection. Returns `true` when it was the user's *last* live
    /// connection — the online-to-offline transition — so the caller can clear
    /// persisted presence and broadcast the user going offline.
    pub fn unregister(&self, user_id: Uuid, conn_id: Uuid) -> bool {
        self.senders.remove(&conn_id);
        let removed_last = self
            .user_conns
            .remove_if_mut(&user_id, |_, conns| {
                conns.remove(&conn_id);
                conns.is_empty()
            })
            .is_some();
        if removed_last {
            self.channels.iter_mut().for_each(|mut entry| {
                entry.value_mut().remove(&user_id);
            });
            self.typing_cooldowns.retain(|&(uid, _), _| uid != user_id);
        }
        removed_last
    }

    pub fn subscribe(&self, user_id: Uuid, channel_id: Uuid) {
        self.channels
            .entry(channel_id)
            .or_default()
            .insert(user_id);
    }

    pub fn unsubscribe(&self, user_id: Uuid, channel_id: Uuid) {
        if let Some(mut subs) = self.channels.get_mut(&channel_id) {
            subs.remove(&user_id);
            let empty = subs.is_empty();
            drop(subs);
            if empty {
                self.channels.remove(&channel_id);
            }
        }
        self.typing_cooldowns.remove(&(user_id, channel_id));
    }

    pub fn broadcast_to_channel(&self, channel_id: Uuid, msg: &ServerMsg) {
        let Some(subs) = self.channels.get(&channel_id) else {
            return;
        };
        let user_ids: Vec<Uuid> = subs.value().iter().copied().collect();
        drop(subs);
        for uid in user_ids {
            self.send_to_user(uid, msg);
        }
    }

    pub fn send_to_user(&self, user_id: Uuid, msg: &ServerMsg) {
        if let Some(conn_ids) = self.user_conns.get(&user_id) {
            for conn_id in conn_ids.value() {
                if let Some(tx) = self.senders.get(conn_id) {
                    let _ = tx.send(msg.clone());
                }
            }
        }
    }

    pub fn check_typing_cooldown(&self, user_id: Uuid, channel_id: Uuid) -> bool {
        let now = Instant::now();
        let key = (user_id, channel_id);
        if let Some(last) = self.typing_cooldowns.get(&key) {
            if now.duration_since(*last) < TYPING_COOLDOWN {
                return false;
            }
        }
        self.typing_cooldowns.insert(key, now);
        true
    }
}
