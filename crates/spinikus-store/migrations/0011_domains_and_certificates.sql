-- Accounts with ACME certificate authorities. Credentials (the account key) are sealed
-- with the account's ID as context.
CREATE TABLE acme_accounts (
    id            TEXT PRIMARY KEY NOT NULL,
    directory_url TEXT NOT NULL CHECK (directory_url <> ''),
    email         TEXT NOT NULL CHECK (email <> ''),
    credentials   BLOB NOT NULL,
    data_key_id   TEXT NOT NULL REFERENCES data_keys (id),
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (directory_url, email)
) STRICT;

-- Issued or uploaded certificates. A renewal stores a new certificate and moves the
-- domains over, so rows never change. The private key is sealed with the certificate's ID as context.
CREATE TABLE certificates (
    id              TEXT PRIMARY KEY NOT NULL,
    -- JSON array of the hostnames it is valid for.
    hostnames       TEXT NOT NULL CHECK (json_type(hostnames) = 'array' AND json_array_length(hostnames) > 0),
    chain_pem       TEXT NOT NULL CHECK (chain_pem <> ''),
    private_key     BLOB NOT NULL,
    data_key_id     TEXT NOT NULL REFERENCES data_keys (id),
    not_before      TEXT NOT NULL,
    not_after       TEXT NOT NULL CHECK (not_after > not_before),
    acme_account_id TEXT REFERENCES acme_accounts (id),
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE INDEX certificates_not_after ON certificates (not_after);

CREATE TABLE domains (
    id             TEXT PRIMARY KEY NOT NULL,
    service_id     TEXT NOT NULL REFERENCES services (id),
    hostname       TEXT NOT NULL CHECK (hostname <> ''),
    -- Another domain of the same service to redirect to, instead of proxying.
    redirect_to    TEXT REFERENCES domains (id) CHECK (redirect_to <> id),
    -- Set by the certificate manager; cleared when the domain is deleted or its hostname changes.
    certificate_id TEXT REFERENCES certificates (id),
    revision       INTEGER NOT NULL DEFAULT 1,
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    deleted_at     TEXT,
    CHECK (deleted_at IS NULL OR certificate_id IS NULL)
) STRICT;

-- A hostname belongs to one domain at a time, across all services.
CREATE UNIQUE INDEX domains_hostname_unique ON domains (hostname) WHERE deleted_at IS NULL;
CREATE INDEX domains_service ON domains (service_id, hostname);
CREATE INDEX domains_redirect_to ON domains (redirect_to);
CREATE INDEX domains_certificate ON domains (certificate_id);
