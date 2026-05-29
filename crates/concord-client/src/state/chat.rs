//! Messages and typing indicators for the channel the user is currently viewing.
//!
//! Only the active channel's history is held; switching channels clears it and
//! a fresh page is loaded. The server returns history newest-first and
//! cursor-paginated (`before` = the oldest id already held), so the loaders
//! reverse each page into oldest→newest order for top-to-bottom rendering.
//!
//! Typing indicators are tracked as a set of user ids for the active channel.
//! Per the protocol, a `TypingStarted` should be self-expired by the view a few
//! seconds later rather than relying solely on `TypingStopped`; this state just
//! holds the set and leaves the timer to the view.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use concord_shared::types::MessageWithAuthor;

/// History and typing state for the active channel.
#[derive(Default)]
pub struct ChatState {
    active_channel: Option<Uuid>,
    /// Loaded messages, oldest first.
    messages: Vec<MessageWithAuthor>,
    /// Whether older messages may exist before the oldest loaded one.
    has_more: bool,
    /// True while a history page is being fetched.
    loading: bool,
    /// Users currently typing in the active channel.
    typing: HashSet<Uuid>,
}

impl ChatState {
    /// Create empty state with no active channel.
    pub fn new() -> Self {
        Self::default()
    }

    /// The channel currently being viewed, if any.
    pub fn active_channel(&self) -> Option<Uuid> {
        self.active_channel
    }

    /// Switch to `channel_id`, clearing history and typing state when it
    /// actually changes (re-opening the same channel is a no-op).
    pub fn open_channel(&mut self, channel_id: Uuid) {
        if self.active_channel == Some(channel_id) {
            return;
        }
        self.active_channel = Some(channel_id);
        self.messages.clear();
        self.typing.clear();
        self.has_more = false;
        self.loading = false;
    }

    /// The loaded messages, oldest first.
    pub fn messages(&self) -> &[MessageWithAuthor] {
        &self.messages
    }

    /// Whether older messages may still be fetched.
    pub fn has_more(&self) -> bool {
        self.has_more
    }

    /// Whether a history fetch is in flight.
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Mark a history fetch as in flight or finished.
    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
    }

    /// The cursor for fetching the next older page: the oldest loaded id.
    pub fn oldest_cursor(&self) -> Option<Uuid> {
        self.messages.first().map(|m| m.id)
    }

    /// Replace history with a fresh first page for `channel_id`. The page is
    /// newest-first (as the server returns it) and is stored oldest-first.
    /// Ignored if `channel_id` is not the active channel — a late response for
    /// a channel the user already left must not clobber the current view.
    pub fn set_history(
        &mut self,
        channel_id: Uuid,
        mut newest_first: Vec<MessageWithAuthor>,
        has_more: bool,
    ) {
        if self.active_channel != Some(channel_id) {
            return;
        }
        newest_first.reverse();
        self.messages = newest_first;
        self.has_more = has_more;
        self.loading = false;
    }

    /// Prepend an older page (newest-first) ahead of the current history.
    pub fn prepend_older(
        &mut self,
        channel_id: Uuid,
        mut newest_first: Vec<MessageWithAuthor>,
        has_more: bool,
    ) {
        if self.active_channel != Some(channel_id) {
            return;
        }
        newest_first.reverse();
        newest_first.append(&mut self.messages);
        self.messages = newest_first;
        self.has_more = has_more;
        self.loading = false;
    }

    /// Append a live message to the active channel, de-duplicating by id. The
    /// caller resolves the author and timestamp, since the wire `NewMessage`
    /// carries neither the author profile nor `created_at`.
    pub fn push_message(&mut self, message: MessageWithAuthor) {
        if self.active_channel != Some(message.channel_id) {
            return;
        }
        if self.messages.iter().any(|m| m.id == message.id) {
            return;
        }
        self.messages.push(message);
    }

    /// Apply an edit to a loaded message, if present.
    pub fn edit_message(&mut self, message_id: Uuid, content: String, edited_at: DateTime<Utc>) {
        if let Some(m) = self.messages.iter_mut().find(|m| m.id == message_id) {
            m.content = content;
            m.edited_at = Some(edited_at);
        }
    }

    /// Remove a loaded message, if present.
    pub fn delete_message(&mut self, message_id: Uuid) {
        self.messages.retain(|m| m.id != message_id);
    }

    /// Whether any messages are loaded.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Record that `user_id` started typing in the active channel.
    pub fn start_typing(&mut self, user_id: Uuid) {
        self.typing.insert(user_id);
    }

    /// Record that `user_id` stopped typing.
    pub fn stop_typing(&mut self, user_id: Uuid) {
        self.typing.remove(&user_id);
    }

    /// The users currently shown as typing.
    pub fn typing_users(&self) -> impl Iterator<Item = Uuid> + '_ {
        self.typing.iter().copied()
    }

    /// How many users are currently typing.
    pub fn typing_count(&self) -> usize {
        self.typing.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concord_shared::types::MessageAuthor;

    fn msg(channel_id: Uuid, n: u8, content: &str) -> MessageWithAuthor {
        // A deterministic id whose ordering matches `n`, so reversal is visible.
        let mut bytes = [0u8; 16];
        bytes[15] = n;
        MessageWithAuthor {
            id: Uuid::from_bytes(bytes),
            channel_id,
            author: Some(MessageAuthor {
                id: Uuid::nil(),
                username: "alice".into(),
                avatar_url: None,
            }),
            content: content.into(),
            edited_at: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn open_channel_resets_and_is_idempotent() {
        let mut chat = ChatState::new();
        let ch = Uuid::new_v4();
        chat.open_channel(ch);
        chat.set_history(ch, vec![msg(ch, 1, "hi")], false);
        assert_eq!(chat.messages().len(), 1);
        // Re-opening the same channel keeps history.
        chat.open_channel(ch);
        assert_eq!(chat.messages().len(), 1);
        // Switching channels clears it.
        chat.open_channel(Uuid::new_v4());
        assert!(chat.is_empty());
    }

    #[test]
    fn set_history_stores_oldest_first() {
        let mut chat = ChatState::new();
        let ch = Uuid::new_v4();
        chat.open_channel(ch);
        // Server order: newest first (3, 2, 1).
        chat.set_history(ch, vec![msg(ch, 3, "c"), msg(ch, 2, "b"), msg(ch, 1, "a")], true);
        let order: Vec<_> = chat.messages().iter().map(|m| m.content.as_str()).collect();
        assert_eq!(order, vec!["a", "b", "c"]);
        assert!(chat.has_more());
        // Oldest cursor is the first (oldest) message.
        assert_eq!(chat.oldest_cursor(), Some(chat.messages()[0].id));
    }

    #[test]
    fn prepend_older_goes_in_front() {
        let mut chat = ChatState::new();
        let ch = Uuid::new_v4();
        chat.open_channel(ch);
        chat.set_history(ch, vec![msg(ch, 4, "d"), msg(ch, 3, "c")], true);
        chat.prepend_older(ch, vec![msg(ch, 2, "b"), msg(ch, 1, "a")], false);
        let order: Vec<_> = chat.messages().iter().map(|m| m.content.as_str()).collect();
        assert_eq!(order, vec!["a", "b", "c", "d"]);
        assert!(!chat.has_more());
    }

    #[test]
    fn stale_history_for_other_channel_is_ignored() {
        let mut chat = ChatState::new();
        let ch = Uuid::new_v4();
        chat.open_channel(ch);
        chat.set_history(Uuid::new_v4(), vec![msg(ch, 1, "a")], false);
        assert!(chat.is_empty());
    }

    #[test]
    fn push_dedupes_and_respects_active_channel() {
        let mut chat = ChatState::new();
        let ch = Uuid::new_v4();
        chat.open_channel(ch);
        let m = msg(ch, 1, "a");
        chat.push_message(m.clone());
        chat.push_message(m);
        assert_eq!(chat.messages().len(), 1);
        // A message for another channel is dropped.
        chat.push_message(msg(Uuid::new_v4(), 2, "other"));
        assert_eq!(chat.messages().len(), 1);
    }

    #[test]
    fn edit_and_delete_target_by_id() {
        let mut chat = ChatState::new();
        let ch = Uuid::new_v4();
        chat.open_channel(ch);
        let m = msg(ch, 1, "a");
        let id = m.id;
        chat.push_message(m);
        chat.edit_message(id, "edited".into(), Utc::now());
        assert_eq!(chat.messages()[0].content, "edited");
        assert!(chat.messages()[0].edited_at.is_some());
        chat.delete_message(id);
        assert!(chat.is_empty());
    }

    #[test]
    fn typing_set_tracks_users() {
        let mut chat = ChatState::new();
        let u = Uuid::new_v4();
        chat.start_typing(u);
        chat.start_typing(u);
        assert_eq!(chat.typing_count(), 1);
        chat.stop_typing(u);
        assert_eq!(chat.typing_count(), 0);
    }
}
