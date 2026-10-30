-- Data keys for envelope encryption of secrets, each encrypted ("wrapped") with the
-- master key, which is kept outside the database. Secret values record the data key
-- that encrypted them, so rotating the master key only rewraps these rows.
CREATE TABLE data_keys (
    id            TEXT PRIMARY KEY NOT NULL,
    -- Fingerprint of the master key that wrapped this key, to catch a wrong master key at startup.
    master_key_id TEXT NOT NULL,
    -- 24-byte nonce, 32-byte key, 16-byte tag.
    wrapped_key   BLOB NOT NULL CHECK (length(wrapped_key) = 72),
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;
