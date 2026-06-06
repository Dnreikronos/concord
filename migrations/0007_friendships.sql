-- ---------------------------------------------------------------------------
-- friendships
-- ---------------------------------------------------------------------------
-- A friend relationship (or a pending friend request) between two users.
--
-- One row per relationship; the only directionality is who initiated it:
-- `requester_id` sent the request to `addressee_id`. `status` walks
-- 'pending' -> 'accepted'. Rejecting a request, cancelling one, or unfriending
-- deletes the row rather than leaving a tombstone, so a later re-add is a fresh
-- row. A reverse request (B asks A while A has already asked B) is collapsed
-- into an accept by the application, never a second row.
--
-- "Are these two users related?" must be order-independent, so a unique index on
-- the canonical (LEAST, GREATEST) pair forbids both a duplicate and the mirror
-- row (A->B alongside B->A). Lookups by either side -- "my friends", "requests
-- to me", "requests I sent" -- are served by the per-column indexes.
CREATE TABLE friendships (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    requester_id  UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    addressee_id  UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    status        TEXT NOT NULL DEFAULT 'pending'
                      CHECK (status IN ('pending', 'accepted')),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- No befriending yourself.
    CONSTRAINT friendships_distinct_users CHECK (requester_id <> addressee_id)
);

-- One relationship per unordered pair: forbids a duplicate and the mirror row.
CREATE UNIQUE INDEX friendships_pair_uniq
    ON friendships (LEAST(requester_id, addressee_id), GREATEST(requester_id, addressee_id));

-- "requests I sent" / "my friends" from the requester side.
CREATE INDEX friendships_requester_idx ON friendships (requester_id);
-- "requests to me" / "my friends" from the addressee side.
CREATE INDEX friendships_addressee_idx ON friendships (addressee_id);
