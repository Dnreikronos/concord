-- Follow-up to 0001_initial_schema.sql, addressing PR #44 review feedback.
--
-- 1. Reject whitespace-only password_hash so that the
--    users_auth_path_present constraint can't be satisfied by an empty
--    string sentinel. Matches the btrim pattern already used on
--    messages.content.
-- 2. Index the three nullable FK columns whose parents cascade
--    ON DELETE SET NULL. messages.author_id is the load-bearing one
--    (user delete would otherwise seqscan the messages table at scale);
--    the other two complete the pattern at negligible cost.

ALTER TABLE users
    ADD CONSTRAINT users_password_hash_non_empty
        CHECK (password_hash IS NULL
            OR char_length(btrim(password_hash)) > 0);

CREATE INDEX messages_author_id_idx     ON messages    (author_id);
CREATE INDEX channels_category_id_idx   ON channels    (category_id);
CREATE INDEX dm_channels_owner_id_idx   ON dm_channels (owner_id);
