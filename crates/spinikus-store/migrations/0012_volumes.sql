-- Persistent volumes of a service. The data lives on the server, in a runtime volume
-- named after the ID; deleting the row leaves removing the data to a cleanup job.
CREATE TABLE volumes (
    id         TEXT PRIMARY KEY NOT NULL,
    service_id TEXT NOT NULL REFERENCES services (id),
    name       TEXT NOT NULL,
    mount_path TEXT NOT NULL CHECK (mount_path LIKE '/_%'),
    revision   INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (service_id, name),
    UNIQUE (service_id, mount_path)
) STRICT;
