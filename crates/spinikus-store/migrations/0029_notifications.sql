-- Notification channels, and the notifications queued for them (an outbox the control
-- plane sends from, with retries).
CREATE TABLE notification_channels (
    id          TEXT PRIMARY KEY NOT NULL,
    name        TEXT NOT NULL,
    kind        TEXT NOT NULL CHECK (kind IN ('webhook', 'slack', 'discord', 'telegram', 'email')),
    -- JSON: the kind's settings, without the secret.
    settings    TEXT NOT NULL,
    -- JSON array of event names; empty means every event.
    events      TEXT NOT NULL,
    enabled     INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    secret      BLOB,
    data_key_id TEXT REFERENCES data_keys (id),
    revision    INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    CHECK ((secret IS NULL) = (data_key_id IS NULL))
) STRICT;

CREATE UNIQUE INDEX notification_channels_name ON notification_channels (name);

CREATE TABLE notifications (
    id              TEXT PRIMARY KEY NOT NULL,
    channel_id      TEXT NOT NULL REFERENCES notification_channels (id) ON DELETE CASCADE,
    event           TEXT NOT NULL,
    title           TEXT NOT NULL,
    text            TEXT NOT NULL,
    url             TEXT,
    -- JSON object of strings, for webhooks.
    details         TEXT NOT NULL,
    status          TEXT NOT NULL CHECK (status IN ('queued', 'sent', 'failed')),
    attempts        INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    error           TEXT,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    next_attempt_at TEXT,
    sent_at         TEXT,
    CHECK ((status = 'queued') = (next_attempt_at IS NOT NULL))
) STRICT;

CREATE INDEX notifications_due ON notifications (next_attempt_at) WHERE status = 'queued';
CREATE INDEX notifications_channel ON notifications (channel_id, id);
