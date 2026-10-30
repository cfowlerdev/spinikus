-- The durable job queue. Workers claim due jobs with a lease (locked_by, locked_until)
-- and extend it while working; a job whose lease runs out is claimed again.
CREATE TABLE jobs (
    id           TEXT PRIMARY KEY NOT NULL,
    kind         TEXT NOT NULL CHECK (kind <> ''),
    payload      TEXT NOT NULL DEFAULT '{}' CHECK (json_type(payload) = 'object'),
    status       TEXT NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed', 'canceled')),
    attempts     INTEGER NOT NULL DEFAULT 0,
    max_attempts INTEGER NOT NULL CHECK (max_attempts >= 1),
    run_at       TEXT NOT NULL,
    dedupe_key   TEXT,
    locked_by    TEXT,
    locked_until TEXT,
    last_error   TEXT,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    finished_at  TEXT,
    CHECK (attempts BETWEEN 0 AND max_attempts),
    CHECK ((status = 'running') = (locked_by IS NOT NULL)),
    CHECK ((locked_by IS NULL) = (locked_until IS NULL)),
    CHECK ((status IN ('succeeded', 'failed', 'canceled')) = (finished_at IS NOT NULL))
) STRICT;

-- What workers poll: due queued jobs, and running jobs whose lease may have run out.
CREATE INDEX jobs_queued ON jobs (run_at, id) WHERE status = 'queued';
CREATE INDEX jobs_running ON jobs (locked_until) WHERE status = 'running';
CREATE INDEX jobs_status ON jobs (status, id);
-- One unfinished job per dedupe key.
CREATE UNIQUE INDEX jobs_dedupe_key ON jobs (dedupe_key) WHERE dedupe_key IS NOT NULL AND status IN ('queued', 'running');
