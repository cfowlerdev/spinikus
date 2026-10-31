-- Where getting a certificate for each hostname the proxies serve stands: the current
-- certificate, and the failures since the last success with when to try again.
CREATE TABLE hostname_certificates (
    hostname       TEXT PRIMARY KEY NOT NULL CHECK (hostname <> ''),
    certificate_id TEXT REFERENCES certificates (id),
    failures       INTEGER NOT NULL DEFAULT 0 CHECK (failures >= 0),
    last_error     TEXT,
    retry_at       TEXT,
    updated_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE INDEX hostname_certificates_certificate ON hostname_certificates (certificate_id);

-- The certificates last built for each server's proxy, and the revision its agent last
-- applied. Revisions work like those of route tables.
CREATE TABLE server_certificates (
    server_id        TEXT PRIMARY KEY NOT NULL REFERENCES servers (id),
    revision         INTEGER NOT NULL CHECK (revision > 0),
    certificate_ids  TEXT NOT NULL CHECK (json_valid(certificate_ids) AND json_type(certificate_ids) = 'array'),
    applied_revision INTEGER NOT NULL DEFAULT 0 CHECK (applied_revision <= revision),
    updated_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;
