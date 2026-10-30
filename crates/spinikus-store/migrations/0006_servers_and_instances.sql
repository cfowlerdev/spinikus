-- Servers as users manage them (desired state).
CREATE TABLE servers (
    id         TEXT PRIMARY KEY NOT NULL,
    name       TEXT NOT NULL CHECK (name <> ''),
    revision   INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    -- Revoking blocks the server's agent; the row stays for history.
    revoked_at TEXT
) STRICT;

-- What a server's agent reports about its connection (observed state). No row
-- means no agent has connected yet.
CREATE TABLE server_state (
    server_id    TEXT PRIMARY KEY NOT NULL REFERENCES servers (id),
    connected    INTEGER NOT NULL CHECK (connected IN (0, 1)),
    -- The agent's last Hello: versions and host facts.
    agent_info   TEXT NOT NULL CHECK (json_type(agent_info) = 'object'),
    connected_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL
) STRICT;

-- Containers as agents report them (observed state). The service and deployment come
-- from container labels and have no foreign keys: an agent can report containers the
-- database does not know, such as after a restore, and the reconciler cleans those up.
CREATE TABLE instances (
    id            TEXT PRIMARY KEY NOT NULL,
    server_id     TEXT NOT NULL REFERENCES servers (id),
    service_id    TEXT NOT NULL,
    deployment_id TEXT NOT NULL,
    container_id  TEXT NOT NULL,
    status        TEXT NOT NULL CHECK (status IN ('created', 'running', 'paused', 'restarting', 'removing', 'exited', 'dead')),
    health        TEXT NOT NULL CHECK (health IN ('none', 'starting', 'healthy', 'unhealthy')),
    restart_count INTEGER NOT NULL CHECK (restart_count >= 0),
    started_at    TEXT,
    reported_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (server_id, container_id)
) STRICT;

CREATE INDEX instances_service ON instances (service_id);
CREATE INDEX instances_deployment ON instances (deployment_id);
