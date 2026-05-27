CREATE TABLE server_invites (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    server_id   UUID NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    creator_id  UUID NOT NULL REFERENCES users(id)   ON DELETE CASCADE,
    -- 8-char alphanumeric code, unique across all invites.
    code        TEXT NOT NULL UNIQUE
                    CHECK (char_length(code) BETWEEN 6 AND 16)
                    CHECK (code ~ '^[A-Za-z0-9]+$'),
    max_uses    INTEGER,
    uses        INTEGER NOT NULL DEFAULT 0,
    expires_at  TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT server_invites_uses_nonneg
        CHECK (uses >= 0),
    CONSTRAINT server_invites_max_uses_positive
        CHECK (max_uses IS NULL OR max_uses > 0)
);

CREATE INDEX server_invites_server_id_idx ON server_invites (server_id);
