use axum::{extract::State, response::Response};
use serde::Deserialize;
use spinikus_control::{Control, NewDatabase};
use spinikus_core::{
    database::Engine,
    id::{BackupId, EnvironmentId, ServerId},
};

use crate::{
    error::ApiError,
    extract::{Authenticated, Body, Path},
    response::located,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    name: String,
    slug: Option<String>,
    /// Required unless `from_backup` is given; then the backup's.
    engine: Option<Engine>,
    /// A major version, such as `"17"`; the newest if omitted, or the backup's database's.
    version: Option<String>,
    server_id: Option<ServerId>,
    /// A backup of another database to restore into this one once it runs.
    from_backup: Option<BackupId>,
}

/// Creates a database service and deploys it. The body has the service and its first
/// deployment, and the restore when it starts from a backup; the location is the service's.
pub async fn create(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(environment_id): Path<EnvironmentId>,
    Body(body): Body<Create>,
) -> Result<Response, ApiError> {
    let new = NewDatabase {
        name: body.name,
        slug: body.slug,
        engine: body.engine,
        version: body.version,
        server_id: body.server_id,
        from_backup: body.from_backup,
    };
    let created = control.create_database(&principal, environment_id, new).await?;
    Ok(located(format!("/api/v1/services/{}", created.service.id), created))
}
