-- User accounts. Disabled rather than deleted, so audit entries and deployment events
-- keep pointing at them. Emails are stored lowercased.
CREATE TABLE users (
    id            TEXT PRIMARY KEY NOT NULL,
    email         TEXT NOT NULL UNIQUE CHECK (email <> '' AND email = lower(email)),
    name          TEXT NOT NULL CHECK (name <> ''),
    -- PHC string; NULL for users who only sign in through SSO.
    password_hash TEXT CHECK (password_hash LIKE '$%'),
    -- Instance admin: every permission, regardless of memberships.
    admin         INTEGER NOT NULL CHECK (admin IN (0, 1)),
    revision      INTEGER NOT NULL DEFAULT 1,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    disabled_at   TEXT
) STRICT;

-- A user's role in a project (and all its environments) or in one environment.
CREATE TABLE memberships (
    id             TEXT PRIMARY KEY NOT NULL,
    user_id        TEXT NOT NULL REFERENCES users (id),
    project_id     TEXT REFERENCES projects (id),
    environment_id TEXT REFERENCES environments (id),
    role           TEXT NOT NULL CHECK (role IN ('viewer', 'developer', 'admin')),
    revision       INTEGER NOT NULL DEFAULT 1,
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    CHECK ((project_id IS NOT NULL) + (environment_id IS NOT NULL) = 1)
) STRICT;

-- One membership per user and scope; these also serve listing a user's memberships.
CREATE UNIQUE INDEX memberships_user_project ON memberships (user_id, project_id) WHERE project_id IS NOT NULL;
CREATE UNIQUE INDEX memberships_user_environment ON memberships (user_id, environment_id) WHERE environment_id IS NOT NULL;
CREATE INDEX memberships_project ON memberships (project_id) WHERE project_id IS NOT NULL;
CREATE INDEX memberships_environment ON memberships (environment_id) WHERE environment_id IS NOT NULL;
