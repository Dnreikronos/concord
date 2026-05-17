-- pgcrypto provides gen_random_uuid(), used as the default for every PK.
-- citext provides case-insensitive text columns, used for usernames and
-- emails so that 'Alice' and 'alice' can't both register and impersonate
-- each other.
-- Isolated from the schema migration because CREATE EXTENSION is privileged
-- and a test harness (or a hardened prod role) may pre-create them.
CREATE EXTENSION IF NOT EXISTS pgcrypto;
CREATE EXTENSION IF NOT EXISTS citext;
