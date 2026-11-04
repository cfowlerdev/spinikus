-- Resource use as agents report it: one row per container (its deployment) or server per
-- minute, kept a day, and hourly means of those, kept 30 days. Observed state.
CREATE TABLE metric_points (
    -- The deployment whose container it is, or the server's ID for the server as a whole.
    subject            TEXT NOT NULL,
    server_id          TEXT NOT NULL REFERENCES servers (id),
    -- NULL for the server as a whole.
    service_id         TEXT REFERENCES services (id),
    resolution         TEXT NOT NULL CHECK (resolution IN ('minute', 'hour')),
    -- The end of the period it covers.
    at                 TEXT NOT NULL,
    cpu_cores          REAL CHECK (cpu_cores >= 0),
    memory_bytes       INTEGER CHECK (memory_bytes >= 0),
    memory_limit_bytes INTEGER CHECK (memory_limit_bytes >= 0),
    network_rx_rate    REAL CHECK (network_rx_rate >= 0),
    network_tx_rate    REAL CHECK (network_tx_rate >= 0),
    disk_used_bytes    INTEGER CHECK (disk_used_bytes >= 0),
    disk_total_bytes   INTEGER CHECK (disk_total_bytes >= 0),
    PRIMARY KEY (subject, resolution, at)
) STRICT, WITHOUT ROWID;

CREATE INDEX metric_points_service ON metric_points (service_id, resolution, at) WHERE service_id IS NOT NULL;
CREATE INDEX metric_points_age ON metric_points (resolution, at);
