-- Settings are updated with If-Match like other resources.
ALTER TABLE settings ADD COLUMN revision INTEGER NOT NULL DEFAULT 1;

-- The route table last built for each server, and the revision its agent last applied.
-- Revisions are at least the time of the change in milliseconds, so a table built after
-- a restore never reuses a revision an agent already has for different routes.
CREATE TABLE route_tables (
    server_id        TEXT PRIMARY KEY NOT NULL REFERENCES servers (id),
    revision         INTEGER NOT NULL CHECK (revision > 0),
    routes           TEXT NOT NULL CHECK (json_valid(routes) AND json_type(routes) = 'array'),
    applied_revision INTEGER NOT NULL DEFAULT 0 CHECK (applied_revision <= revision),
    updated_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE INDEX deployments_live ON deployments (id) WHERE status = 'live';
