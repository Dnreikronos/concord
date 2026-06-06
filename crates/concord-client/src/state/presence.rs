//! Live presence (online status) of the users the client can see.
//!
//! Populated from `ServerMsg::PresenceSnapshot` once on each (re)connect and
//! kept current by `ServerMsg::UserStatusChanged`. Like the other state pieces
//! it names no GPUI types, so it unit-tests in the default build; the root view
//! folds the WebSocket events into it and views read it by observing the entity.
//!
//! The snapshot lists only non-offline peers, so the map only ever holds users
//! who are present — an absent user is taken to be offline.

use std::collections::HashMap;

use uuid::Uuid;

use concord_shared::protocol::UserPresence;
use concord_shared::types::UserStatus;

/// Each known user's current status, keyed by user id.
#[derive(Default)]
pub struct PresenceState {
    statuses: HashMap<Uuid, UserStatus>,
}

impl PresenceState {
    /// Create empty state — every user reads as offline until a snapshot lands.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current status of `user_id`, defaulting to offline when unknown:
    /// the snapshot omits offline peers, so an absent user is offline.
    pub fn status_for(&self, user_id: Uuid) -> UserStatus {
        self.statuses.get(&user_id).copied().unwrap_or_default()
    }

    /// Replace all tracked presence with a fresh snapshot. Any offline entry is
    /// dropped so the map stays a set of present peers.
    pub fn set_snapshot(&mut self, users: Vec<UserPresence>) {
        self.statuses = users
            .into_iter()
            .filter(|p| p.status != UserStatus::Offline)
            .map(|p| (p.user_id, p.status))
            .collect();
    }

    /// Record one user's new status. Going offline drops the entry, so the map
    /// keeps holding only present peers.
    pub fn set_status(&mut self, user_id: Uuid, status: UserStatus) {
        if status == UserStatus::Offline {
            self.statuses.remove(&user_id);
        } else {
            self.statuses.insert(user_id, status);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(n: u8) -> Uuid {
        let mut bytes = [0u8; 16];
        bytes[15] = n;
        Uuid::from_bytes(bytes)
    }

    #[test]
    fn unknown_users_are_offline() {
        let p = PresenceState::new();
        assert_eq!(p.status_for(user(1)), UserStatus::Offline);
    }

    #[test]
    fn snapshot_replaces_and_drops_offline() {
        let mut p = PresenceState::new();
        p.set_status(user(9), UserStatus::Online);
        p.set_snapshot(vec![
            UserPresence { user_id: user(1), status: UserStatus::Online },
            UserPresence { user_id: user(2), status: UserStatus::Idle },
            // An offline entry in a snapshot is ignored.
            UserPresence { user_id: user(3), status: UserStatus::Offline },
        ]);
        assert_eq!(p.status_for(user(1)), UserStatus::Online);
        assert_eq!(p.status_for(user(2)), UserStatus::Idle);
        assert_eq!(p.status_for(user(3)), UserStatus::Offline);
        // The pre-snapshot status is gone — a snapshot is authoritative.
        assert_eq!(p.status_for(user(9)), UserStatus::Offline);
    }

    #[test]
    fn set_status_updates_then_clears_on_offline() {
        let mut p = PresenceState::new();
        p.set_status(user(1), UserStatus::Dnd);
        assert_eq!(p.status_for(user(1)), UserStatus::Dnd);
        p.set_status(user(1), UserStatus::Online);
        assert_eq!(p.status_for(user(1)), UserStatus::Online);
        // Going offline forgets the user entirely.
        p.set_status(user(1), UserStatus::Offline);
        assert_eq!(p.status_for(user(1)), UserStatus::Offline);
    }
}
