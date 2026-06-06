//! The user's friends and pending friend requests.
//!
//! Loaded over REST (`GET /api/friends` and `/api/friends/requests`) on login,
//! then kept current by the live friend events the root view folds in
//! ([`ServerMsg::FriendRequestReceived`](concord_shared::protocol::ServerMsg)
//! and friends). Presence is stored per friend and refreshed from
//! `UserStatusChanged`, so the friends list shows online/offline even for
//! friends who share no server with the caller.
//!
//! Mirrors [`crate::state::dms`] in shape: a plain data + logic struct that
//! names no GPUI types, so it unit-tests in the default build.

use uuid::Uuid;

use concord_shared::types::{Friend, FriendRequest, FriendRequests, UserStatus};

/// The friends list, the incoming/outgoing request lists, and load status.
#[derive(Default)]
pub struct FriendsState {
    friends: Vec<Friend>,
    incoming: Vec<FriendRequest>,
    outgoing: Vec<FriendRequest>,
    loading: bool,
    loaded: bool,
}

impl FriendsState {
    /// Create empty state with nothing loaded.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// Accepted friends, alphabetical by username.
    pub fn friends(&self) -> &[Friend] {
        &self.friends
    }

    /// Pending requests addressed to the caller.
    pub fn incoming(&self) -> &[FriendRequest] {
        &self.incoming
    }

    /// Pending requests the caller has sent.
    pub fn outgoing(&self) -> &[FriendRequest] {
        &self.outgoing
    }

    /// How many incoming requests await a response — the sidebar badge count.
    pub fn incoming_count(&self) -> usize {
        self.incoming.len()
    }

    /// Replace the friends list, marking it loaded.
    pub fn set_friends(&mut self, friends: Vec<Friend>) {
        self.friends = friends;
        self.loaded = true;
        self.loading = false;
        self.sort_friends();
    }

    /// Replace both request lists.
    pub fn set_requests(&mut self, requests: FriendRequests) {
        self.incoming = requests.incoming;
        self.outgoing = requests.outgoing;
    }

    /// Drop a friend by user id (they unfriended us, or we unfriended them).
    pub fn remove_friend(&mut self, user_id: Uuid) {
        self.friends.retain(|f| f.user.id != user_id);
    }

    /// Drop a pending request by its friendship-row id, from either list.
    pub fn remove_request(&mut self, request_id: Uuid) {
        self.incoming.retain(|r| r.id != request_id);
        self.outgoing.retain(|r| r.id != request_id);
    }

    /// Update a friend's presence in place (from a live `UserStatusChanged`).
    /// No-op for a user who isn't a friend.
    pub fn set_friend_status(&mut self, user_id: Uuid, status: UserStatus) {
        if let Some(friend) = self.friends.iter_mut().find(|f| f.user.id == user_id) {
            friend.status = status;
        }
    }

    fn sort_friends(&mut self) {
        self.friends.sort_by(|a, b| {
            a.user
                .username
                .to_lowercase()
                .cmp(&b.user.username.to_lowercase())
                .then_with(|| a.user.id.cmp(&b.user.id))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use concord_shared::types::{FriendRequestDirection, UserSummary};

    fn summary(name: &str) -> UserSummary {
        UserSummary { id: Uuid::new_v4(), username: name.into(), avatar_url: None }
    }

    fn friend(name: &str, status: UserStatus) -> Friend {
        Friend { user: summary(name), status, since: Utc::now() }
    }

    #[test]
    fn set_friends_sorts_case_insensitively_and_marks_loaded() {
        let mut s = FriendsState::new();
        s.set_friends(vec![
            friend("Charlie", UserStatus::Online),
            friend("alice", UserStatus::Offline),
            friend("Bob", UserStatus::Idle),
        ]);
        let order: Vec<String> = s.friends().iter().map(|f| f.user.username.clone()).collect();
        assert_eq!(order, vec!["alice", "Bob", "Charlie"]);
        assert!(s.is_loaded());
        assert!(!s.is_loading());
    }

    #[test]
    fn remove_request_drops_from_either_list() {
        let mut s = FriendsState::new();
        let inc = FriendRequest {
            id: Uuid::new_v4(),
            user: summary("ed"),
            direction: FriendRequestDirection::Incoming,
            created_at: Utc::now(),
        };
        let out = FriendRequest {
            id: Uuid::new_v4(),
            user: summary("fay"),
            direction: FriendRequestDirection::Outgoing,
            created_at: Utc::now(),
        };
        let (inc_id, out_id) = (inc.id, out.id);
        s.set_requests(FriendRequests { incoming: vec![inc], outgoing: vec![out] });

        s.remove_request(inc_id);
        assert!(s.incoming().is_empty());
        assert_eq!(s.outgoing().len(), 1);
        s.remove_request(out_id);
        assert!(s.outgoing().is_empty());
    }

    #[test]
    fn set_friend_status_updates_only_a_friend() {
        let mut s = FriendsState::new();
        let f = friend("ivy", UserStatus::Offline);
        let id = f.user.id;
        s.set_friends(vec![f]);
        s.set_friend_status(id, UserStatus::Online);
        assert_eq!(s.friends()[0].status, UserStatus::Online);
        // Unknown user: no panic, no change.
        s.set_friend_status(Uuid::new_v4(), UserStatus::Dnd);
        assert_eq!(s.friends()[0].status, UserStatus::Online);
    }
}
