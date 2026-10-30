-- Environment variables, set on exactly one of a project, an environment or a service.
-- Every value is encrypted, secret or not; `secret` only controls whether it can be read back.
CREATE TABLE variables (
    id             TEXT PRIMARY KEY NOT NULL,
    project_id     TEXT REFERENCES projects (id),
    environment_id TEXT REFERENCES environments (id),
    service_id     TEXT REFERENCES services (id),
    name           TEXT NOT NULL CHECK (name <> ''),
    -- Sealed with the data key below, with the variable's ID as context.
    value          BLOB NOT NULL,
    data_key_id    TEXT NOT NULL REFERENCES data_keys (id),
    availability   TEXT NOT NULL CHECK (availability IN ('runtime', 'build', 'both')),
    secret         INTEGER NOT NULL CHECK (secret IN (0, 1)),
    revision       INTEGER NOT NULL DEFAULT 1,
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    CHECK ((project_id IS NOT NULL) + (environment_id IS NOT NULL) + (service_id IS NOT NULL) = 1)
) STRICT;

-- Names are unique per scope; these also serve listing a scope's variables.
CREATE UNIQUE INDEX variables_project_name ON variables (project_id, name) WHERE project_id IS NOT NULL;
CREATE UNIQUE INDEX variables_environment_name ON variables (environment_id, name) WHERE environment_id IS NOT NULL;
CREATE UNIQUE INDEX variables_service_name ON variables (service_id, name) WHERE service_id IS NOT NULL;
