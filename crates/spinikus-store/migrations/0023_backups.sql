-- Backups of database services to S3-compatible storage.

-- Buckets backups go to, shared by the instance. The secret access key is sealed with the
-- destination's ID as context. Deleted destinations are kept for the backups they hold.
CREATE TABLE storage_destinations (
    id                TEXT PRIMARY KEY NOT NULL,
    name              TEXT NOT NULL,
    endpoint          TEXT,
    region            TEXT NOT NULL,
    bucket            TEXT NOT NULL,
    path_prefix       TEXT NOT NULL,
    path_style        INTEGER NOT NULL CHECK (path_style IN (0, 1)),
    access_key_id     TEXT NOT NULL,
    secret_access_key BLOB NOT NULL,
    data_key_id       TEXT NOT NULL REFERENCES data_keys (id),
    revision          INTEGER NOT NULL DEFAULT 1,
    created_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    deleted_at        TEXT
) STRICT;

CREATE UNIQUE INDEX storage_destinations_name ON storage_destinations (name) WHERE deleted_at IS NULL;

-- When a service's backups are made, where to, and how many to keep.
CREATE TABLE backup_schedules (
    id             TEXT PRIMARY KEY NOT NULL,
    service_id     TEXT NOT NULL REFERENCES services (id),
    destination_id TEXT NOT NULL REFERENCES storage_destinations (id),
    cron           TEXT NOT NULL,
    keep_last      INTEGER NOT NULL CHECK (keep_last BETWEEN 1 AND 1000),
    enabled        INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    -- When the next backup is due; NULL while disabled.
    next_run_at    TEXT,
    revision       INTEGER NOT NULL DEFAULT 1,
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE INDEX backup_schedules_service ON backup_schedules (service_id);
CREATE INDEX backup_schedules_due ON backup_schedules (next_run_at) WHERE next_run_at IS NOT NULL;

-- Each backup run. Deleted ones (by hand or by a schedule's retention) are kept as rows
-- with deleted_at, their objects gone.
CREATE TABLE backups (
    id             TEXT PRIMARY KEY NOT NULL,
    service_id     TEXT NOT NULL REFERENCES services (id),
    destination_id TEXT NOT NULL REFERENCES storage_destinations (id),
    schedule_id    TEXT REFERENCES backup_schedules (id) ON DELETE SET NULL,
    status         TEXT NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed')),
    engine         TEXT NOT NULL CHECK (engine IN ('postgres', 'redis')),
    object_key     TEXT NOT NULL,
    server_id      TEXT REFERENCES servers (id),
    size_bytes     INTEGER,
    error          TEXT,
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    started_at     TEXT,
    finished_at    TEXT,
    deleted_at     TEXT
) STRICT;

CREATE INDEX backups_service ON backups (service_id, id);
CREATE INDEX backups_schedule ON backups (schedule_id, id) WHERE schedule_id IS NOT NULL;
CREATE INDEX backups_active ON backups (status) WHERE status IN ('queued', 'running');
