-- A service stopped by a user: its live deployment's container is stopped in place until
-- it is started again or another deployment goes live.
ALTER TABLE services ADD COLUMN stopped_at TEXT;
