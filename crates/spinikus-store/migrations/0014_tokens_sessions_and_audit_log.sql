-- API tokens. The token itself is shown once; only its SHA-256 hash is stored.
-- Revoked rather than deleted, so the audit log keeps pointing at them.
CREATE TABLE api_tokens (
    id             TEXT PRIMARY KEY NOT NULL,
    user_id        TEXT NOT NULL REFERENCES users (id),
    name           TEXT NOT NULL CHECK (name <> ''),
    token_hash     BLOB NOT NULL UNIQUE CHECK (length(token_hash) = 32),
    role           TEXT NOT NULL CHECK (role IN ('viewer', 'developer', 'admin')),
    -- At most one set: the token is limited to that project or environment.
    project_id     TEXT REFERENCES projects (id),
    environment_id TEXT REFERENCES environments (id),
    expires_at     TEXT,
    last_used_at   TEXT,
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    revoked_at     TEXT,
    CHECK ((project_id IS NOT NULL) + (environment_id IS NOT NULL) <= 1)
) STRICT;

CREATE INDEX api_tokens_user ON api_tokens (user_id, id);

-- Web sessions. Only the SHA-256 hash of the cookie value is stored. Deleted on sign-out
-- and once expired.
CREATE TABLE sessions (
    id           TEXT PRIMARY KEY NOT NULL,
    user_id      TEXT NOT NULL REFERENCES users (id),
    token_hash   BLOB NOT NULL UNIQUE CHECK (length(token_hash) = 32),
    expires_at   TEXT NOT NULL,
    last_seen_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    user_agent   TEXT,
    ip_address   TEXT,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE INDEX sessions_user ON sessions (user_id, id);
CREATE INDEX sessions_expires_at ON sessions (expires_at);

-- Who did what. Append-only: the triggers below refuse updates and deletes.
CREATE TABLE audit_log (
    id         TEXT PRIMARY KEY NOT NULL,
    -- `system`, a user ID or a server ID.
    actor      TEXT NOT NULL,
    token_id   TEXT REFERENCES api_tokens (id),
    action     TEXT NOT NULL CHECK (action <> ''),
    -- Any entity's ID, without a foreign key: the entity may be deleted later.
    target_id  TEXT,
    project_id TEXT REFERENCES projects (id),
    details    TEXT NOT NULL DEFAULT '{}' CHECK (json_type(details) = 'object'),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE INDEX audit_log_project ON audit_log (project_id, id) WHERE project_id IS NOT NULL;
CREATE INDEX audit_log_target ON audit_log (target_id, id) WHERE target_id IS NOT NULL;

CREATE TRIGGER audit_log_no_update BEFORE UPDATE ON audit_log
BEGIN
    SELECT RAISE(ABORT, 'the audit log is append-only');
END;

CREATE TRIGGER audit_log_no_delete BEFORE DELETE ON audit_log
BEGIN
    SELECT RAISE(ABORT, 'the audit log is append-only');
END;
