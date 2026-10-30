use axum::{
    extract::State,
    http::StatusCode,
    response::{Json, Response},
};
use serde::Deserialize;
use spinikus_control::{Control, NewService, ServiceChanges};
use spinikus_core::{
    id::{EnvironmentId, ServiceId},
    service::{Service, ServiceConfig, ServiceKind},
};

use crate::{
    error::ApiError,
    extract::{Authenticated, Body, IfMatch, Path, Query},
    response::{PageBody, PageParams, created, tagged},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    name: String,
    slug: Option<String>,
    kind: ServiceKind,
    #[serde(default)]
    config: ServiceConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    name: Option<String>,
    slug: Option<String>,
    /// Replaces the whole config.
    config: Option<ServiceConfig>,
}

pub async fn list(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(environment_id): Path<EnvironmentId>,
    Query(params): Query<PageParams<ServiceId>>,
) -> Result<Json<PageBody<Service>>, ApiError> {
    let page = control.list_services(&principal, environment_id, params.after, params.limit()).await?;
    Ok(PageBody::new(page, |service| service.id.to_string()))
}

pub async fn create(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(environment_id): Path<EnvironmentId>,
    Body(body): Body<Create>,
) -> Result<Response, ApiError> {
    let new = NewService { name: body.name, slug: body.slug, kind: body.kind, config: body.config };
    let service = control.create_service(&principal, environment_id, new).await?;
    Ok(created(format!("/api/v1/services/{}", service.id), service.revision, service))
}

pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ServiceId>,
) -> Result<Response, ApiError> {
    let service = control.get_service(&principal, id).await?;
    Ok(tagged(service.revision, service))
}

pub async fn update(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ServiceId>,
    IfMatch(revision): IfMatch,
    Body(body): Body<Update>,
) -> Result<Response, ApiError> {
    let changes = ServiceChanges { name: body.name, slug: body.slug, config: body.config };
    let service = control.update_service(&principal, id, revision, changes).await?;
    Ok(tagged(service.revision, service))
}

pub async fn delete(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ServiceId>,
) -> Result<StatusCode, ApiError> {
    control.delete_service(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
