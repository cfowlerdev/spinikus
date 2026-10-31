-- Placement: the server a service's new deployments run on. NULL means the embedded server.
ALTER TABLE services ADD COLUMN server_id TEXT REFERENCES servers (id);

-- The server a deployment runs on, fixed when it is created. Required: SQLite checks the
-- constraint against existing rows, so this fails on a database with older deployments.
ALTER TABLE deployments ADD COLUMN server_id TEXT REFERENCES servers (id) CHECK (server_id IS NOT NULL);
CREATE INDEX deployments_server ON deployments (server_id, status);
CREATE INDEX deployments_active ON deployments (id)
    WHERE status IN ('queued', 'preparing', 'releasing', 'starting', 'verifying', 'draining');

-- What went wrong, in words, next to the reason code of a failed deployment.
ALTER TABLE deployment_events ADD COLUMN message TEXT;
