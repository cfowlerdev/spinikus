-- Where a service's code or image comes from. At most one per service.
CREATE TABLE sources (
    id             TEXT PRIMARY KEY NOT NULL,
    service_id     TEXT NOT NULL UNIQUE REFERENCES services (id),
    type           TEXT NOT NULL CHECK (type IN ('git', 'image')),
    -- Git sources.
    repo_url       TEXT,
    branch         TEXT,
    root_directory TEXT,
    -- Image sources.
    image          TEXT,
    revision       INTEGER NOT NULL DEFAULT 1,
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    CHECK (
        (type = 'git' AND repo_url IS NOT NULL AND branch IS NOT NULL AND image IS NULL)
        OR (type = 'image' AND image IS NOT NULL AND repo_url IS NULL AND branch IS NULL AND root_directory IS NULL)
    )
) STRICT;

-- Inbound Git webhooks look up services by repository and branch.
CREATE INDEX sources_repo_branch ON sources (repo_url, branch) WHERE type = 'git';
