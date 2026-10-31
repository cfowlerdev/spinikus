use axum::{extract::State, response::Response};
use serde::Deserialize;
use spinikus_control::{Control, NewDatabase};
use spinikus_core::{
    database::Engine,
    id::{EnvironmentId, ServerId},
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
    engine: Engine,
    /// A major version, such as `"17"`; the newest if omitted.
    version: Option<String>,
    server_id: Option<ServerId>,
}

/// Creates a database service and deploys it. The body has the service and its first
/// deployment; the location is the service's.
pub async fn create(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(environment_id): Path<EnvironmentId>,
    Body(body): Body<Create>,
) -> Result<Response, ApiError> {
    let new =
        NewDatabase { name: body.name, slug: body.slug, engine: body.engine, version: body.version, server_id: body.server_id };
    let created = control.create_database(&principal, environment_id, new).await?;
    Ok(located(format!("/api/v1/services/{}", created.service.id), created))
}
