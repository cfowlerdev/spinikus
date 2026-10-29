CREATE TABLE projects (
    id         TEXT PRIMARY KEY NOT NULL,
    name       TEXT NOT NULL CHECK (name <> ''),
    slug       TEXT NOT NULL,
    revision   INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    -- Soft delete: set when the project is deleted, which frees its slug.
    deleted_at TEXT
) STRICT;

CREATE UNIQUE INDEX projects_slug_unique ON projects (slug) WHERE deleted_at IS NULL;
