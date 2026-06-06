-- Per-member DM read state, for the conversation-list unread indicator.
--
-- `last_read_at` records when a member last opened a DM conversation. The DM
-- list (`GET /api/dms`) marks a conversation unread when a message from another
-- member is newer than this timestamp. Nullable with no default: existing
-- memberships start as "never read" (NULL), so any message from another member
-- shows as unread until the conversation is first opened.
ALTER TABLE dm_members ADD COLUMN last_read_at TIMESTAMPTZ;
