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
    /// Ids of optimistically-shown messages still awaiting the server's echo.
    /// Each is a client-generated id that [`Self::confirm_optimistic`] swaps for
    /// the server's once the matching `NewMessage` arrives.
    pending: HashSet<Uuid>,
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
        self.pending.clear();
        self.has_more = false;
        self.loading = false;
    }

    /// Clear the active channel and its history. Used when switching to a
    /// server that has no text channel to show, so the pane stops rendering
    /// the previous channel's messages.
    pub fn close_channel(&mut self) {
        self.active_channel = None;
        self.messages.clear();
        self.typing.clear();
        self.pending.clear();
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

    /// Show a locally-authored message immediately, before the server confirms
    /// it. The message carries a client-generated id; once the server echoes it
    /// back, [`Self::confirm_optimistic`] swaps in the real id and timestamp.
    /// Dropped if it isn't for the active channel.
    pub fn push_optimistic(&mut self, message: MessageWithAuthor) {
        if self.active_channel != Some(message.channel_id) {
            return;
        }
        self.pending.insert(message.id);
        self.messages.push(message);
    }

    /// Reconcile a server `NewMessage` with a still-pending optimistic message:
    /// the oldest pending message with the same author and content adopts the
    /// server's id and timestamp. Returns `true` when one was reconciled, so the
    /// caller skips the normal insert; `false` leaves it to
    /// [`Self::push_message`]. Matching only pending messages keeps a resent
    /// duplicate from clobbering an already-confirmed message of the same text.
    pub fn confirm_optimistic(
        &mut self,
        channel_id: Uuid,
        server_id: Uuid,
        author_id: Option<Uuid>,
        content: &str,
        created_at: DateTime<Utc>,
    ) -> bool {
        if self.active_channel != Some(channel_id) {
            return false;
        }
        let pending = &self.pending;
        let Some(pos) = self.messages.iter().position(|m| {
            pending.contains(&m.id)
                && m.author.as_ref().map(|a| a.id) == author_id
                && m.content == content
        }) else {
            return false;
        };
        let old_id = self.messages[pos].id;
        self.pending.remove(&old_id);
        // If the confirmed id is already present (a duplicate echo), drop the
        // optimistic copy rather than leave two rows sharing one id.
        if old_id != server_id && self.messages.iter().any(|m| m.id == server_id) {
            self.messages.remove(pos);
        } else {
            self.messages[pos].id = server_id;
            self.messages[pos].created_at = created_at;
        }
        true
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

    fn mine(channel_id: Uuid, author: Uuid, content: &str) -> MessageWithAuthor {
        let mut m = msg(channel_id, 0, content);
        m.id = Uuid::new_v4();
        m.author = Some(MessageAuthor {
            id: author,
            username: "me".into(),
            avatar_url: None,
        });
        m
    }

    #[test]
    fn confirm_optimistic_swaps_in_server_id() {
        let mut chat = ChatState::new();
        let ch = Uuid::new_v4();
        let author = Uuid::new_v4();
        chat.open_channel(ch);
        chat.push_optimistic(mine(ch, author, "hello"));
        assert_eq!(chat.messages().len(), 1);

        let server_id = Uuid::new_v4();
        let reconciled = chat.confirm_optimistic(ch, server_id, Some(author), "hello", Utc::now());
        assert!(reconciled);
        assert_eq!(chat.messages().len(), 1);
        assert_eq!(chat.messages()[0].id, server_id);

        // The confirmed echo, now sharing the server id, dedupes to a no-op.
        let mut echo = mine(ch, author, "hello");
        echo.id = server_id;
        chat.push_message(echo);
        assert_eq!(chat.messages().len(), 1);
    }

    #[test]
    fn confirm_optimistic_without_match_leaves_it_to_push() {
        let mut chat = ChatState::new();
        let ch = Uuid::new_v4();
        chat.open_channel(ch);
        let reconciled =
            chat.confirm_optimistic(ch, Uuid::new_v4(), Some(Uuid::new_v4()), "hi", Utc::now());
        assert!(!reconciled);
        assert!(chat.is_empty());
    }

    #[test]
    fn confirm_optimistic_reconciles_oldest_pending_first() {
        let mut chat = ChatState::new();
        let ch = Uuid::new_v4();
        let author = Uuid::new_v4();
        chat.open_channel(ch);
        let second_id = {
            let first = mine(ch, author, "ok");
            let second = mine(ch, author, "ok");
            let id = second.id;
            chat.push_optimistic(first);
            chat.push_optimistic(second);
            id
        };

        let server_first = Uuid::new_v4();
        assert!(chat.confirm_optimistic(ch, server_first, Some(author), "ok", Utc::now()));
        // The first "ok" adopted the server id; the second still awaits its echo.
        assert_eq!(chat.messages()[0].id, server_first);
        assert_eq!(chat.messages()[1].id, second_id);
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
