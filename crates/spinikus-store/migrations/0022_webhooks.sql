-- Services' inbound Git webhooks, one per service. The secret is sealed with the webhook
-- ID as context; making a new one keeps the ID, so the URL stays the same.
CREATE TABLE webhooks (
    id               TEXT PRIMARY KEY NOT NULL,
    service_id       TEXT NOT NULL UNIQUE REFERENCES services (id),
    secret           BLOB NOT NULL,
    data_key_id      TEXT NOT NULL REFERENCES data_keys (id),
    created_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    last_delivery_at TEXT,
    last_delivery    TEXT
) STRICT;
