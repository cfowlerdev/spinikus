-- SSH keys services clone private Git repositories with, one per service. The private key
-- is sealed with the service ID as context.
CREATE TABLE deploy_keys (
    service_id  TEXT PRIMARY KEY NOT NULL REFERENCES services (id),
    public_key  TEXT NOT NULL CHECK (public_key <> ''),
    fingerprint TEXT NOT NULL CHECK (fingerprint <> ''),
    private_key BLOB NOT NULL,
    data_key_id TEXT NOT NULL REFERENCES data_keys (id),
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;
