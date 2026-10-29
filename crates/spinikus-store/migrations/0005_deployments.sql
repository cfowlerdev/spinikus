-- One release of a service. Its snapshot never changes; the status moves through the
-- state machine in spinikus-core, with one deployment_events row per change.
CREATE TABLE deployments (
    id           TEXT PRIMARY KEY NOT NULL,
    service_id   TEXT NOT NULL REFERENCES services (id),
    trigger_type TEXT NOT NULL CHECK (trigger_type IN ('manual', 'git_push', 'rollback')),
    strategy     TEXT NOT NULL CHECK (strategy IN ('blue_green', 'recreate')),
    status       TEXT NOT NULL CHECK (status IN (
        'queued', 'preparing', 'releasing', 'starting', 'verifying', 'live', 'draining', 'superseded', 'failed', 'canceled'
    )),
    reason       TEXT CHECK (reason IN (
        'build_failed', 'release_failed', 'start_failed', 'health_check_failed', 'timeout', 'agent_lost',
        'canceled_by_user', 'superseded_in_queue'
    )),
    rollback_of  TEXT REFERENCES deployments (id),
    snapshot     TEXT NOT NULL CHECK (json_type(snapshot) = 'object'),
    commit_sha   TEXT,
    image        TEXT,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    CHECK ((status IN ('failed', 'canceled')) = (reason IS NOT NULL)),
    CHECK ((trigger_type = 'rollback') = (rollback_of IS NOT NULL))
) STRICT;

CREATE INDEX deployments_service ON deployments (service_id, id);
-- Per service: one waiting deployment, one in progress, and one live. A service's
-- current deployment is its live one.
CREATE UNIQUE INDEX deployments_one_queued ON deployments (service_id) WHERE status = 'queued';
CREATE UNIQUE INDEX deployments_one_in_progress ON deployments (service_id)
    WHERE status IN ('preparing', 'releasing', 'starting', 'verifying');
CREATE UNIQUE INDEX deployments_one_live ON deployments (service_id) WHERE status = 'live';

CREATE TABLE deployment_events (
    id            TEXT PRIMARY KEY NOT NULL,
    deployment_id TEXT NOT NULL REFERENCES deployments (id),
    -- NULL for the event that creates the deployment.
    from_status   TEXT,
    to_status     TEXT NOT NULL,
    reason        TEXT,
    -- `system`, a user ID or a server ID.
    actor         TEXT NOT NULL,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE INDEX deployment_events_deployment ON deployment_events (deployment_id, id);
