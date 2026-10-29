CREATE TABLE environments (
    id         TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects (id),
    name       TEXT NOT NULL CHECK (name <> ''),
    slug       TEXT NOT NULL,
    kind       TEXT NOT NULL CHECK (kind IN ('production', 'staging', 'preview')),
    -- Set for previews only.
    pr_number  INTEGER CHECK ((kind = 'preview') = (pr_number IS NOT NULL)),
    revision   INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    deleted_at TEXT
) STRICT;

CREATE UNIQUE INDEX environments_slug_unique ON environments (project_id, slug) WHERE deleted_at IS NULL;
-- At most one production environment, and one preview per pull request, per project.
CREATE UNIQUE INDEX environments_production_unique ON environments (project_id)
    WHERE kind = 'production' AND deleted_at IS NULL;
CREATE UNIQUE INDEX environments_preview_unique ON environments (project_id, pr_number)
    WHERE kind = 'preview' AND deleted_at IS NULL;

CREATE TABLE services (
    id             TEXT PRIMARY KEY NOT NULL,
    environment_id TEXT NOT NULL REFERENCES environments (id),
    name           TEXT NOT NULL CHECK (name <> ''),
    slug           TEXT NOT NULL,
    kind           TEXT NOT NULL CHECK (kind IN ('app', 'database', 'template')),
    -- Kind-specific settings.
    config         TEXT NOT NULL DEFAULT '{}' CHECK (json_type(config) = 'object'),
    revision       INTEGER NOT NULL DEFAULT 1,
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    deleted_at     TEXT
) STRICT;

CREATE UNIQUE INDEX services_slug_unique ON services (environment_id, slug) WHERE deleted_at IS NULL;
