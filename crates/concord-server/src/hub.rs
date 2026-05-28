use std::collections::HashSet;

use dashmap::DashMap;
use tokio::sync::mpsc;
use uuid::Uuid;

use concord_shared::protocol::ServerMsg;

pub struct Hub {
    connections: DashMap<Uuid, mpsc::UnboundedSender<ServerMsg>>,
    channels: DashMap<Uuid, HashSet<Uuid>>,
}

impl Hub {
    pub fn new() -> Self {
        Self {
            connections: DashMap::new(),
            channels: DashMap::new(),
        }
    }

    pub fn register(
        &self,
        user_id: Uuid,
    ) -> mpsc::UnboundedReceiver<ServerMsg> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.connections.insert(user_id, tx);
        rx
    }

    pub fn unregister(&self, user_id: Uuid) {
        self.connections.remove(&user_id);
        self.channels.iter_mut().for_each(|mut entry| {
            entry.value_mut().remove(&user_id);
        });
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
        }
    }

    pub fn broadcast_to_channel(&self, channel_id: Uuid, msg: &ServerMsg) {
        let Some(subs) = self.channels.get(&channel_id) else {
            return;
        };
        for uid in subs.value() {
            self.send_to_user(*uid, msg);
        }
    }

    pub fn send_to_user(&self, user_id: Uuid, msg: &ServerMsg) {
        if let Some(tx) = self.connections.get(&user_id) {
            let _ = tx.send(msg.clone());
        }
    }
}
