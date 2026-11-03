use axum::{
    extract::State,
    http::StatusCode,
    response::{Json, Response},
};
use serde::Deserialize;
use spinikus_control::{Control, NewService, ServiceChanges};
use spinikus_core::{
    id::{EnvironmentId, ServerId, ServiceId},
    service::{Service, ServiceConfig, ServiceKind},
};

use crate::{
    error::ApiError,
    extract::{Authenticated, Body, IfMatch, Path, Query},
    response::{PageBody, PageParams, created, tagged},
    routes::present,
};

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = ServiceCreate)]
pub struct Create {
    name: String,
    slug: Option<String>,
    kind: ServiceKind,
    #[serde(default)]
    #[schema(value_type = Object)]
    config: ServiceConfig,
    /// Where it runs; omitted means the embedded server.
    server_id: Option<ServerId>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = ServiceUpdate)]
pub struct Update {
    name: Option<String>,
    slug: Option<String>,
    /// Replaces the whole config.
    #[schema(value_type = Option<Object>)]
    config: Option<ServiceConfig>,
    /// `null` moves the service back to the embedded server; omitted keeps the placement.
    #[serde(default, deserialize_with = "present")]
    server_id: Option<Option<ServerId>>,
}

/// Lists an environment's services.
#[utoipa::path(
    get,
    path = "/environments/{id}/services",
    operation_id = "services_list",
    tag = "services",
    params(("id" = EnvironmentId, Path), crate::response::PageQuery),
    responses((status = 200, body = PageBody<Service>)),
)]
pub async fn list(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(environment_id): Path<EnvironmentId>,
    Query(params): Query<PageParams<ServiceId>>,
) -> Result<Json<PageBody<Service>>, ApiError> {
    let page = control.list_services(&principal, environment_id, params.after, params.limit()).await?;
    Ok(PageBody::new(page, |service| service.id.to_string()))
}

/// Creates a service.
#[utoipa::path(
    post,
    path = "/environments/{id}/services",
    operation_id = "services_create",
    tag = "services",
    params(("id" = EnvironmentId, Path)),
    request_body = Create,
    responses((status = 201, body = Service, headers(("Location" = String, description = "Where the new resource is."), ("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn create(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(environment_id): Path<EnvironmentId>,
    Body(body): Body<Create>,
) -> Result<Response, ApiError> {
    let new = NewService { name: body.name, slug: body.slug, kind: body.kind, config: body.config, server_id: body.server_id };
    let service = control.create_service(&principal, environment_id, new).await?;
    Ok(created(format!("/api/v1/services/{}", service.id), service.revision, service))
}

/// Gets a service.
#[utoipa::path(
    get,
    path = "/services/{id}",
    operation_id = "services_show",
    tag = "services",
    params(("id" = ServiceId, Path)),
    responses((status = 200, body = Service, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ServiceId>,
) -> Result<Response, ApiError> {
    let service = control.get_service(&principal, id).await?;
    Ok(tagged(service.revision, service))
}

/// Updates a service's name, config or placement.
#[utoipa::path(
    patch,
    path = "/services/{id}",
    operation_id = "services_update",
    tag = "services",
    params(("id" = ServiceId, Path), crate::extract::IfMatch),
    request_body = Update,
    responses((status = 200, body = Service, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn update(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ServiceId>,
    IfMatch(revision): IfMatch,
    Body(body): Body<Update>,
) -> Result<Response, ApiError> {
    let changes = ServiceChanges { name: body.name, slug: body.slug, config: body.config, server_id: body.server_id };
    let service = control.update_service(&principal, id, revision, changes).await?;
    Ok(tagged(service.revision, service))
}

/// Deletes a service; its deployments are stopped and cleaned up.
#[utoipa::path(
    delete,
    path = "/services/{id}",
    operation_id = "services_delete",
    tag = "services",
    params(("id" = ServiceId, Path)),
    responses((status = 204, description = "Done.")),
)]
pub async fn delete(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ServiceId>,
) -> Result<StatusCode, ApiError> {
    control.delete_service(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
