-- Instance-wide key/value settings managed through the UI and API,
-- such as the ACME email. Values are JSON.
CREATE TABLE settings (
    key        TEXT PRIMARY KEY NOT NULL,
    value      TEXT NOT NULL CHECK (json_valid(value)),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;
