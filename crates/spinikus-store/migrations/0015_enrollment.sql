-- The internal certificate authority. It signs agents' client certificates and the agent
-- listener's certificate. One row; its private key (PEM) is sealed with the context
-- `certificate_authority`.
CREATE TABLE certificate_authority (
    id              INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
    certificate_pem TEXT NOT NULL CHECK (certificate_pem <> ''),
    private_key     BLOB NOT NULL,
    data_key_id     TEXT NOT NULL REFERENCES data_keys (id),
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

-- Single-use join tokens for enrolling a server's agent. Only the SHA-256 hash of the
-- token is stored.
CREATE TABLE enrollment_tokens (
    id         TEXT PRIMARY KEY NOT NULL,
    server_id  TEXT NOT NULL REFERENCES servers (id),
    token_hash BLOB NOT NULL UNIQUE CHECK (length(token_hash) = 32),
    expires_at TEXT NOT NULL,
    used_at    TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE INDEX enrollment_tokens_server ON enrollment_tokens (server_id, id);
