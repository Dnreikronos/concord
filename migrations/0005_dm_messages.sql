-- DM messaging integration (issue #23).
--
-- DM messages share the `messages` table with server-channel messages, keyed by
-- messages.channel_id = dm_channels.id. The original FK pinned channel_id to
-- channels(id) only, so a DM channel id could never be inserted. Postgres can't
-- express a foreign key whose target is one of two tables, so we drop that
-- single-table FK and reinstate its two guarantees with triggers:
--
--   1. existence — channel_id must name a row in channels OR dm_channels.
--   2. cascade   — deleting a channel of either kind deletes its messages,
--                  matching the ON DELETE CASCADE the FK provided.
--
-- Trade-off vs a real FK: the existence check does not lock the parent row, so
-- a message inserted concurrently with its channel's deletion could in
-- principle outlive the channel. Channel deletion is rare and owner/admin
-- gated, so this narrow race is acceptable; the alternative (a shared channel
-- registry that both tables key into) is a far larger schema change.

ALTER TABLE messages DROP CONSTRAINT messages_channel_id_fkey;

-- (1) Existence guard, replacing the FK's referential check. Fires only on the
-- columns that can break the invariant so plain content edits stay cheap. The
-- foreign_key_violation SQLSTATE keeps the failure indistinguishable from the
-- old FK for any caller that matched on it.
CREATE FUNCTION assert_message_channel_exists() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM channels WHERE id = NEW.channel_id)
       AND NOT EXISTS (SELECT 1 FROM dm_channels WHERE id = NEW.channel_id)
    THEN
        RAISE EXCEPTION
            'messages.channel_id % matches no channel or dm_channel',
            NEW.channel_id
            USING ERRCODE = 'foreign_key_violation';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER messages_channel_exists
    BEFORE INSERT OR UPDATE OF channel_id ON messages
    FOR EACH ROW EXECUTE FUNCTION assert_message_channel_exists();

-- (2) Cascade, replacing the FK's ON DELETE CASCADE. Fires once per deleted
-- parent row, including rows removed by an upstream cascade (deleting a server
-- cascades to its channels, whose deletion fires this). messages_channel_id is
-- indexed (messages_channel_created_idx), so the fan-out delete stays cheap.
CREATE FUNCTION delete_messages_for_channel() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM messages WHERE channel_id = OLD.id;
    RETURN OLD;
END;
$$;

CREATE TRIGGER channels_cascade_messages
    AFTER DELETE ON channels
    FOR EACH ROW EXECUTE FUNCTION delete_messages_for_channel();

CREATE TRIGGER dm_channels_cascade_messages
    AFTER DELETE ON dm_channels
    FOR EACH ROW EXECUTE FUNCTION delete_messages_for_channel();
