-- When a finished deployment's containers and image were removed from its server.
ALTER TABLE deployments ADD COLUMN cleaned_at TEXT;

-- The runner looks at the deployments not cleaned up yet on every pass.
CREATE INDEX deployments_uncleaned ON deployments (service_id, id) WHERE cleaned_at IS NULL;

-- Deployments on a server sharing an image, so an image is removed only once none needs it.
CREATE INDEX deployments_server_image ON deployments (server_id, image) WHERE image IS NOT NULL;
