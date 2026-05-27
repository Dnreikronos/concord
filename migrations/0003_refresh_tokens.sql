-- DB-backed refresh tokens for JWT auth (issue #6).
--
-- Stores a SHA-256 hash of the raw token so a database leak doesn't
-- directly compromise active sessions.

CREATE TABLE refresh_tokens (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id    UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash TEXT NOT NULL UNIQUE,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX refresh_tokens_user_id_idx    ON refresh_tokens (user_id);
CREATE INDEX refresh_tokens_expires_at_idx ON refresh_tokens (expires_at);
