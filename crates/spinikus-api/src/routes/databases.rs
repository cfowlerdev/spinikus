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

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = DatabaseCreate)]
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
    /// Deploy it right away (the default). With `false` its settings can be changed before
    /// its first deployment; not with `from_backup`.
    deploy: Option<bool>,
}

/// Creates a database service and deploys it, unless `deploy` is `false`. The body has the
/// service and its first deployment (`null` when not deployed), and the restore when it
/// starts from a backup; the location is the service's.
#[utoipa::path(
    post,
    path = "/environments/{id}/databases",
    operation_id = "databases_create",
    tag = "databases",
    params(("id" = EnvironmentId, Path)),
    request_body = Create,
    responses((status = 201, body = spinikus_control::databases::CreatedDatabase, headers(("Location" = String, description = "Where the new resource is.")))),
)]
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
        deploy: body.deploy.unwrap_or(true),
    };
    let created = control.create_database(&principal, environment_id, new).await?;
    Ok(located(format!("/api/v1/services/{}", created.service.id), created))
}
