-- Restores of a backup into its database, replacing the data there.
CREATE TABLE restores (
    id               TEXT PRIMARY KEY NOT NULL,
    service_id       TEXT NOT NULL REFERENCES services (id),
    backup_id        TEXT NOT NULL REFERENCES backups (id),
    -- The backup of the data being replaced, taken first.
    safety_backup_id TEXT REFERENCES backups (id),
    status           TEXT NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed')),
    server_id        TEXT REFERENCES servers (id),
    error            TEXT,
    created_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    started_at       TEXT,
    finished_at      TEXT
) STRICT;

CREATE INDEX restores_service ON restores (service_id, id);
-- One restore at a time per database.
CREATE UNIQUE INDEX restores_one_active ON restores (service_id) WHERE status IN ('queued', 'running');
-- Backups that active restores read, which retention must keep.
CREATE INDEX restores_active_backup ON restores (backup_id) WHERE status IN ('queued', 'running');
