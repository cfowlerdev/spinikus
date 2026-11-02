-- Previews of pull requests. An environment with `previews` on gets a preview environment
-- for each pull request to the branch its Git services deploy, a copy that remembers it as
-- its base environment.
ALTER TABLE environments ADD COLUMN previews INTEGER NOT NULL DEFAULT 0
    CHECK (previews IN (0, 1) AND (previews = 0 OR kind <> 'preview'));
ALTER TABLE environments ADD COLUMN base_environment_id TEXT REFERENCES environments (id)
    CHECK (base_environment_id IS NULL OR kind = 'preview');
