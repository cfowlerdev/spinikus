-- no-transaction
-- Backups of app volumes: a backup is a database dump (with its engine) or one volume's
-- archive (with the volume). SQLite can't relax a NOT NULL or a CHECK in place, so the
-- table is rebuilt, the documented way: with foreign keys off (restores refer to it), in
-- one transaction, checking them before committing. Foreign keys can only be turned off
-- outside a transaction, hence `no-transaction` above.

PRAGMA foreign_keys = OFF;

BEGIN IMMEDIATE;

CREATE TABLE backups_new (
    id             TEXT PRIMARY KEY NOT NULL,
    service_id     TEXT NOT NULL REFERENCES services (id),
    destination_id TEXT NOT NULL REFERENCES storage_destinations (id),
    schedule_id    TEXT REFERENCES backup_schedules (id) ON DELETE SET NULL,
    status         TEXT NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed')),
    -- A database dump's engine, or the volume archived; exactly one. Not a foreign key:
    -- volumes are deleted, and their backups are kept.
    engine         TEXT CHECK (engine IN ('postgres', 'redis')),
    volume_id      TEXT,
    object_key     TEXT NOT NULL,
    server_id      TEXT REFERENCES servers (id),
    size_bytes     INTEGER,
    error          TEXT,
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    started_at     TEXT,
    finished_at    TEXT,
    deleted_at     TEXT,
    CHECK ((engine IS NULL) <> (volume_id IS NULL))
) STRICT;

INSERT INTO backups_new (
    id, service_id, destination_id, schedule_id, status, engine, volume_id, object_key, server_id, size_bytes, error,
    created_at, started_at, finished_at, deleted_at
)
SELECT
    id, service_id, destination_id, schedule_id, status, engine, NULL, object_key, server_id, size_bytes, error,
    created_at, started_at, finished_at, deleted_at
FROM backups;

DROP TABLE backups;
ALTER TABLE backups_new RENAME TO backups;

CREATE INDEX backups_service ON backups (service_id, id);
CREATE INDEX backups_schedule ON backups (schedule_id, id) WHERE schedule_id IS NOT NULL;
CREATE INDEX backups_active ON backups (status) WHERE status IN ('queued', 'running');
CREATE INDEX backups_volume ON backups (volume_id, id) WHERE volume_id IS NOT NULL;

COMMIT;

PRAGMA foreign_keys = ON;
