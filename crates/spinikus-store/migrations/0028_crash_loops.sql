-- Times a live deployment's container kept restarting or wasn't running, noticed by the
-- control plane from what agents report. Observed state: users never write these.
CREATE TABLE crash_loops (
    id            TEXT PRIMARY KEY NOT NULL,
    service_id    TEXT NOT NULL REFERENCES services (id),
    deployment_id TEXT NOT NULL REFERENCES deployments (id),
    server_id     TEXT NOT NULL REFERENCES servers (id),
    reason        TEXT NOT NULL CHECK (reason IN ('restarting', 'not_running')),
    restarts      INTEGER NOT NULL CHECK (restarts >= 0),
    started_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    ended_at      TEXT
) STRICT;

-- At most one lasting crash loop per deployment.
CREATE UNIQUE INDEX crash_loops_lasting ON crash_loops (deployment_id) WHERE ended_at IS NULL;
CREATE INDEX crash_loops_service ON crash_loops (service_id, id);
