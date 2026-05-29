-- DM read tracking (issue #24).
--
-- The DM-list endpoint surfaces an "unread" flag per conversation. Unread is
-- per-viewer state, not a property of the channel, so it lives in its own
-- table keyed on (dm_channel_id, user_id): one row per member who has read a
-- DM at least once. `last_read_at` is a high-water mark — a conversation is
-- unread when it holds a message from someone *other* than the viewer that is
-- newer than this timestamp.
--
-- A member with no row here has never marked the DM read, which reads as "all
-- messages from others are unread" (the list query treats a missing row as a
-- last_read_at of -infinity). Marking read is an explicit client action
-- (POST /api/dms/{id}/read); sending a message does not implicitly clear the
-- flag, since unread already ignores the viewer's own messages.
CREATE TABLE dm_read_state (
    dm_channel_id  UUID NOT NULL REFERENCES dm_channels(id) ON DELETE CASCADE,
    user_id        UUID NOT NULL REFERENCES users(id)        ON DELETE CASCADE,
    last_read_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (dm_channel_id, user_id)
);

-- Mirrors dm_members_user_id_idx: keeps the ON DELETE CASCADE from users(id)
-- off a full-table scan when an account is deleted.
CREATE INDEX dm_read_state_user_id_idx ON dm_read_state (user_id);
