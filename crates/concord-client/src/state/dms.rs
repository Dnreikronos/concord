//! Direct-message conversations the user can see, and which one is open.
//!
//! The conversation list is loaded over REST (`GET /api/dms`) the first time the
//! user opens the DM view; the server returns it newest-activity-first and we
//! keep it in that order. Live `NewDirectMessage`s fold in through
//! [`DmsState::apply_new_message`], which refreshes a conversation's preview,
//! bumps it back to the top, and flags it unread unless it is the one on screen.
//!
//! Mirrors [`crate::state::servers`] in shape: a plain data + logic struct that
//! names no GPUI types, so it unit-tests in the default build.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use concord_shared::types::{DmConversation, DmLastMessage, MessageAuthor};

/// The DM conversation list, the open conversation, and load status.
#[derive(Default)]
pub struct DmsState {
    conversations: Vec<DmConversation>,
    /// The DM channel currently open in the chat pane, if any.
    active: Option<Uuid>,
    /// True while the conversation list is being fetched.
    loading: bool,
    /// Whether the list has loaded at least once, so the view can tell a
    /// genuinely empty inbox from one that simply hasn't loaded yet.
    loaded: bool,
}

impl DmsState {
    /// Create empty state with nothing loaded.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a conversation-list fetch is in flight.
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Mark a list fetch as in flight or finished.
    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
    }

    /// Whether the list has been fetched at least once.
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// The DM conversations, ordered newest-activity-first.
    pub fn conversations(&self) -> &[DmConversation] {
        &self.conversations
    }

    /// Replace the conversation list, marking it loaded and re-sorting it
    /// newest-activity-first (the server already returns it so, but a defensive
    /// sort keeps it aligned with the live-update ordering).
    pub fn set_conversations(&mut self, conversations: Vec<DmConversation>) {
        self.conversations = conversations;
        self.loaded = true;
        self.loading = false;
        self.sort();
    }

    /// The DM channel currently open, if any.
    pub fn active(&self) -> Option<Uuid> {
        self.active
    }

    /// Open `dm_channel_id` and clear its unread flag (opening a DM reads it).
    pub fn set_active(&mut self, dm_channel_id: Uuid) {
        self.active = Some(dm_channel_id);
        self.mark_read(dm_channel_id);
    }

    /// The conversation record for `dm_channel_id`, if it's in the list.
    pub fn conversation(&self, dm_channel_id: Uuid) -> Option<&DmConversation> {
        self.conversations.iter().find(|c| c.id == dm_channel_id)
    }

    /// Clear a conversation's unread flag locally. The server is told
    /// separately via `POST /api/dms/{id}/read`; this keeps the dot from
    /// lingering until the next fetch.
    pub fn mark_read(&mut self, dm_channel_id: Uuid) {
        if let Some(conv) = self.conversations.iter_mut().find(|c| c.id == dm_channel_id) {
            conv.unread = false;
        }
    }

    /// Fold a freshly delivered DM message into the list: refresh the matching
    /// conversation's preview, re-sort it to the top, and flag it unread unless
    /// it is the open conversation or the message is the caller's own. A message
    /// for a conversation not yet in the list is ignored — it surfaces on the
    /// next fetch.
    pub fn apply_new_message(
        &mut self,
        dm_channel_id: Uuid,
        message_id: Uuid,
        author: Option<MessageAuthor>,
        content: String,
        created_at: DateTime<Utc>,
        from_me: bool,
    ) {
        let is_open = self.active == Some(dm_channel_id);
        let Some(conv) = self.conversations.iter_mut().find(|c| c.id == dm_channel_id) else {
            return;
        };
        conv.last_message = Some(DmLastMessage {
            id: message_id,
            author,
            content,
            created_at,
        });
        if !from_me && !is_open {
            conv.unread = true;
        }
        self.sort();
    }

    /// Order conversations newest-activity-first: by last-message time (a
    /// conversation with no messages sorts last), then by creation time and id —
    /// the same ordering the server's `GET /api/dms` produces, so live updates
    /// keep the list in the shape a fresh fetch would.
    fn sort(&mut self) {
        self.conversations.sort_by(|a, b| {
            let a_at = a.last_message.as_ref().map(|m| m.created_at);
            let b_at = b.last_message.as_ref().map(|m| m.created_at);
            // `None < Some`, so comparing `b` against `a` puts conversations with
            // a message ahead of empty ones and newer messages ahead of older.
            b_at.cmp(&a_at)
                .then_with(|| b.created_at.cmp(&a.created_at))
                .then_with(|| b.id.cmp(&a.id))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concord_shared::types::DmParticipant;

    fn conv(seconds: Option<i64>, unread: bool) -> DmConversation {
        let id = Uuid::new_v4();
        let created_at = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let last_message = seconds.map(|s| DmLastMessage {
            id: Uuid::new_v4(),
            author: None,
            content: "hi".into(),
            created_at: DateTime::<Utc>::from_timestamp(1_700_000_000 + s, 0).unwrap(),
        });
        DmConversation {
            id,
            name: None,
            is_group: false,
            owner_id: None,
            created_at,
            member_count: 2,
            participants: vec![DmParticipant {
                user_id: Uuid::new_v4(),
                username: "alice".into(),
                avatar_url: None,
            }],
            last_message,
            unread,
        }
    }

    #[test]
    fn set_conversations_sorts_newest_first_with_empty_last() {
        let mut s = DmsState::new();
        let older = conv(Some(10), false);
        let newer = conv(Some(100), false);
        let empty = conv(None, false);
        let (older_id, newer_id, empty_id) = (older.id, newer.id, empty.id);
        s.set_conversations(vec![older, empty, newer]);
        let order: Vec<Uuid> = s.conversations().iter().map(|c| c.id).collect();
        assert_eq!(order, vec![newer_id, older_id, empty_id]);
        assert!(s.is_loaded());
        assert!(!s.is_loading());
    }

    #[test]
    fn set_active_marks_the_conversation_read() {
        let mut s = DmsState::new();
        let c = conv(Some(10), true);
        let id = c.id;
        s.set_conversations(vec![c]);
        assert!(s.conversation(id).unwrap().unread);
        s.set_active(id);
        assert_eq!(s.active(), Some(id));
        assert!(!s.conversation(id).unwrap().unread);
    }

    #[test]
    fn apply_new_message_bumps_to_top_and_flags_unread() {
        let mut s = DmsState::new();
        let target = conv(Some(10), false);
        let other = conv(Some(50), false);
        let (target_id, other_id) = (target.id, other.id);
        s.set_conversations(vec![target, other]);
        // `other` is newer, so it leads initially.
        assert_eq!(s.conversations().first().map(|c| c.id), Some(other_id));

        let at = DateTime::<Utc>::from_timestamp(1_700_001_000, 0).unwrap();
        s.apply_new_message(target_id, Uuid::new_v4(), None, "ping".into(), at, false);

        let lead = s.conversations().first().unwrap();
        assert_eq!(lead.id, target_id);
        assert_eq!(lead.last_message.as_ref().unwrap().content, "ping");
        assert!(lead.unread);
    }

    #[test]
    fn apply_new_message_does_not_flag_open_or_own() {
        let mut s = DmsState::new();
        let open = conv(Some(10), false);
        let mine = conv(Some(20), false);
        let (open_id, mine_id) = (open.id, mine.id);
        s.set_conversations(vec![open, mine]);
        s.set_active(open_id);

        let at = DateTime::<Utc>::from_timestamp(1_700_002_000, 0).unwrap();
        // A message in the open conversation stays read.
        s.apply_new_message(open_id, Uuid::new_v4(), None, "a".into(), at, false);
        assert!(!s.conversation(open_id).unwrap().unread);
        // The caller's own message never flags unread.
        s.apply_new_message(mine_id, Uuid::new_v4(), None, "b".into(), at, true);
        assert!(!s.conversation(mine_id).unwrap().unread);
    }

    #[test]
    fn apply_new_message_ignores_unknown_conversation() {
        let mut s = DmsState::new();
        s.set_conversations(vec![conv(Some(10), false)]);
        let at = DateTime::<Utc>::from_timestamp(1_700_003_000, 0).unwrap();
        // No panic, no change for a channel we don't hold.
        s.apply_new_message(Uuid::new_v4(), Uuid::new_v4(), None, "x".into(), at, false);
        assert_eq!(s.conversations().len(), 1);
    }
}
