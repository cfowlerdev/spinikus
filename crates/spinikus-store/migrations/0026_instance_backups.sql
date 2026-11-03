-- Scheduled backups of the instance itself (its database and master key), encrypted with a
-- passphrase and uploaded by the control plane to a storage destination.

-- The one schedule. The passphrase is sealed with `instance-backup-schedule` as context.
CREATE TABLE instance_backup_schedule (
    id             INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
    destination_id TEXT NOT NULL REFERENCES storage_destinations (id),
    cron           TEXT NOT NULL,
    keep_last      INTEGER NOT NULL CHECK (keep_last BETWEEN 1 AND 1000),
    enabled        INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    passphrase     BLOB NOT NULL,
    data_key_id    TEXT NOT NULL REFERENCES data_keys (id),
    -- When the next backup is due; NULL while disabled.
    next_run_at    TEXT,
    revision       INTEGER NOT NULL DEFAULT 1,
    updated_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

-- Each run. Deleted ones (by hand or by retention) are kept as rows with deleted_at, their
-- objects gone.
CREATE TABLE instance_backups (
    id             TEXT PRIMARY KEY NOT NULL,
    destination_id TEXT NOT NULL REFERENCES storage_destinations (id),
    scheduled      INTEGER NOT NULL CHECK (scheduled IN (0, 1)),
    status         TEXT NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed')),
    object_key     TEXT NOT NULL,
    size_bytes     INTEGER,
    error          TEXT,
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    started_at     TEXT,
    finished_at    TEXT,
    deleted_at     TEXT
) STRICT;

CREATE INDEX instance_backups_active ON instance_backups (status) WHERE status IN ('queued', 'running');
