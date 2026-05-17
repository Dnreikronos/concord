-- Initial schema for concord.
--
-- Conventions:
--   * Primary keys are UUIDs, defaulted via pgcrypto's gen_random_uuid().
--   * All timestamps are TIMESTAMPTZ, defaulted to now().
--   * Enum-like fields are TEXT + CHECK constraints (not Postgres ENUMs) so
--     they're easy to ALTER and play nicely with sqlx::query!.
--   * Tables are declared in dependency order so every FK target exists at
--     parse time.
--
-- See docs/ARCHITECTURE.md for the ER diagram and ON DELETE cascade policy.

-- ---------------------------------------------------------------------------
-- users
-- ---------------------------------------------------------------------------
CREATE TABLE users (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- CITEXT so 'Alice' and 'alice' compare (and conflict on UNIQUE) as equal.
    username        CITEXT NOT NULL UNIQUE
                        CHECK (char_length(username) BETWEEN 3 AND 32),
    email           CITEXT UNIQUE,
    password_hash   TEXT,
    avatar_url      TEXT,
    -- Default is the initial state; transitions are app-managed (e.g. the
    -- session handler flips to 'online' after a successful login).
    status          TEXT NOT NULL DEFAULT 'offline'
                        CHECK (status IN ('online', 'idle', 'dnd', 'offline')),
    oauth_provider  TEXT
                        CHECK (oauth_provider IS NULL
                            OR oauth_provider IN ('google', 'github')),
    oauth_subject   TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Every user must have at least one auth path: a password or an OAuth link.
    CONSTRAINT users_auth_path_present
        CHECK (password_hash IS NOT NULL OR oauth_provider IS NOT NULL),
    -- oauth_provider and oauth_subject are a pair: either both are set (the
    -- user has an OAuth link) or both are NULL (password-only user). Without
    -- this, provider='google'/subject=NULL would pass users_auth_path_present
    -- and slip past the partial unique index below (NULLS DISTINCT default).
    CONSTRAINT users_oauth_pair
        CHECK ((oauth_provider IS NULL) = (oauth_subject IS NULL))
);

-- An OAuth provider's (provider, subject) pair is globally unique, but only
-- meaningful when the user actually has an OAuth link — hence a partial index.
CREATE UNIQUE INDEX users_oauth_identity_idx
    ON users (oauth_provider, oauth_subject)
    WHERE oauth_provider IS NOT NULL;

-- ---------------------------------------------------------------------------
-- servers
-- ---------------------------------------------------------------------------
CREATE TABLE servers (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name        TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 100),
    icon_url    TEXT,
    -- RESTRICT: deleting a user who still owns servers is an error; the
    -- caller must transfer ownership or delete the server first.
    owner_id    UUID NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX servers_owner_id_idx ON servers (owner_id);

-- ---------------------------------------------------------------------------
-- server_members
-- ---------------------------------------------------------------------------
CREATE TABLE server_members (
    server_id   UUID NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    user_id     UUID NOT NULL REFERENCES users(id)   ON DELETE CASCADE,
    -- 'owner' is intentionally absent: servers.owner_id is the canonical
    -- source of ownership. Treating it as a role here would create two
    -- sources of truth that can drift.
    role        TEXT NOT NULL DEFAULT 'member'
                    CHECK (role IN ('admin', 'member')),
    joined_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (server_id, user_id)
);

-- Supports "what servers is this user in?" without scanning the PK.
CREATE INDEX server_members_user_id_idx ON server_members (user_id);

-- ---------------------------------------------------------------------------
-- channel_categories
-- ---------------------------------------------------------------------------
-- `position` ordering is app-managed: rows are not unique per server, so ties
-- (e.g. two rows at position 0) are resolved by the application — typically
-- by id or created_at — rather than the DB. Same applies to channels.position.
CREATE TABLE channel_categories (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    server_id   UUID NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    name        TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 100),
    position    INTEGER NOT NULL DEFAULT 0,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX channel_categories_server_position_idx
    ON channel_categories (server_id, position);

-- ---------------------------------------------------------------------------
-- channels
-- ---------------------------------------------------------------------------
CREATE TABLE channels (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    server_id     UUID NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    -- SET NULL: deleting a category drops the grouping but keeps the channels.
    category_id   UUID REFERENCES channel_categories(id) ON DELETE SET NULL,
    name          TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 100),
    topic         TEXT,
    channel_type  TEXT NOT NULL CHECK (channel_type IN ('text', 'voice')),
    position      INTEGER NOT NULL DEFAULT 0,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX channels_server_position_idx ON channels (server_id, position);

-- ---------------------------------------------------------------------------
-- messages
-- ---------------------------------------------------------------------------
CREATE TABLE messages (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    channel_id  UUID NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    -- SET NULL: deleting an author preserves message history (shows as
    -- "deleted user") rather than nuking the conversation.
    author_id   UUID REFERENCES users(id) ON DELETE SET NULL,
    -- btrim guards against whitespace-only messages slipping past the >= 1
    -- floor; the raw length cap keeps storage bounded.
    content     TEXT NOT NULL
                    CHECK (char_length(btrim(content)) >= 1
                       AND char_length(content) <= 4000),
    edited_at   TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- An edit can't precede the original write. Cheap guard against clock
    -- skew or a buggy client supplying its own timestamps.
    CONSTRAINT messages_edited_after_created
        CHECK (edited_at IS NULL OR edited_at >= created_at)
);

-- Backs the "latest N messages in this channel" query, the hot path of any
-- chat UI. DESC matches the natural pagination order.
CREATE INDEX messages_channel_created_idx
    ON messages (channel_id, created_at DESC);

-- ---------------------------------------------------------------------------
-- dm_channels
-- ---------------------------------------------------------------------------
CREATE TABLE dm_channels (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- 1:1 DMs have no name and no owner. Group DMs may carry both — owner_id
    -- is the creator at insert time, but legitimately becomes NULL via the
    -- ON DELETE SET NULL cascade if the creator's account is later deleted,
    -- so we don't require owner_id NOT NULL when is_group = true.
    name        TEXT,
    is_group    BOOLEAN NOT NULL DEFAULT false,
    owner_id    UUID REFERENCES users(id) ON DELETE SET NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT dm_channels_shape
        CHECK (
            (is_group = false AND name IS NULL AND owner_id IS NULL)
            OR is_group = true
        )
);

-- ---------------------------------------------------------------------------
-- dm_members
-- ---------------------------------------------------------------------------
CREATE TABLE dm_members (
    dm_channel_id  UUID NOT NULL REFERENCES dm_channels(id) ON DELETE CASCADE,
    user_id        UUID NOT NULL REFERENCES users(id)        ON DELETE CASCADE,
    joined_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (dm_channel_id, user_id)
);

CREATE INDEX dm_members_user_id_idx ON dm_members (user_id);
