-- The server the control plane itself runs on, whose agent runs inside the control
-- plane's process. At most one.
ALTER TABLE servers ADD COLUMN embedded INTEGER NOT NULL DEFAULT 0 CHECK (embedded IN (0, 1));

CREATE UNIQUE INDEX servers_embedded ON servers (embedded) WHERE embedded = 1;
