-- The variables a deployment runs with, resolved when it was created. Kept out of
-- `deployments.snapshot` because they include secrets: the whole list is sealed as one
-- JSON value, with the deployment's ID as context. A separate table also lets old
-- deployments' secrets be deleted later without touching the immutable deployment.
CREATE TABLE deployment_variables (
    deployment_id TEXT PRIMARY KEY NOT NULL REFERENCES deployments (id),
    variables     BLOB NOT NULL,
    data_key_id   TEXT NOT NULL REFERENCES data_keys (id)
) STRICT;
