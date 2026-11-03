use axum::{
    extract::State,
    http::StatusCode,
    response::{Json, Response},
};
use serde::Deserialize;
use spinikus_control::{Control, DomainChanges, NewDomain, ServiceDomains};
use spinikus_core::id::{DomainId, ServiceId};

use crate::{
    error::ApiError,
    extract::{Authenticated, Body, IfMatch, Path},
    response::{created, tagged},
    routes::present,
};

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = DomainCreate)]
pub struct Create {
    hostname: String,
    /// Another domain of the service to redirect to.
    redirect_to: Option<DomainId>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = DomainUpdate)]
pub struct Update {
    hostname: Option<String>,
    /// `null` serves the app again; omitted keeps the redirect.
    #[serde(default, deserialize_with = "present")]
    redirect_to: Option<Option<DomainId>>,
}

/// The service's generated hostname and its domains.
#[utoipa::path(
    get,
    path = "/services/{id}/domains",
    operation_id = "domains_list",
    tag = "domains",
    params(("id" = ServiceId, Path)),
    responses((status = 200, body = ServiceDomains)),
)]
pub async fn list(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<Json<ServiceDomains>, ApiError> {
    Ok(Json(control.list_domains(&principal, service_id).await?))
}

/// Adds a custom domain to a service.
#[utoipa::path(
    post,
    path = "/services/{id}/domains",
    operation_id = "domains_create",
    tag = "domains",
    params(("id" = ServiceId, Path)),
    request_body = Create,
    responses((status = 201, body = spinikus_core::domain::Domain, headers(("Location" = String, description = "Where the new resource is."), ("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn create(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
    Body(body): Body<Create>,
) -> Result<Response, ApiError> {
    let new = NewDomain { hostname: body.hostname, redirect_to: body.redirect_to };
    let domain = control.create_domain(&principal, service_id, new).await?;
    Ok(created(format!("/api/v1/domains/{}", domain.id), domain.revision, domain))
}

/// Gets a domain.
#[utoipa::path(
    get,
    path = "/domains/{id}",
    operation_id = "domains_show",
    tag = "domains",
    params(("id" = DomainId, Path)),
    responses((status = 200, body = spinikus_core::domain::Domain, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<DomainId>,
) -> Result<Response, ApiError> {
    let domain = control.get_domain(&principal, id).await?;
    Ok(tagged(domain.revision, domain))
}

/// Changes a domain's hostname or where it redirects.
#[utoipa::path(
    patch,
    path = "/domains/{id}",
    operation_id = "domains_update",
    tag = "domains",
    params(("id" = DomainId, Path), crate::extract::IfMatch),
    request_body = Update,
    responses((status = 200, body = spinikus_core::domain::Domain, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn update(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<DomainId>,
    IfMatch(revision): IfMatch,
    Body(body): Body<Update>,
) -> Result<Response, ApiError> {
    let changes = DomainChanges { hostname: body.hostname, redirect_to: body.redirect_to };
    let domain = control.update_domain(&principal, id, revision, changes).await?;
    Ok(tagged(domain.revision, domain))
}

/// Removes a domain.
#[utoipa::path(
    delete,
    path = "/domains/{id}",
    operation_id = "domains_delete",
    tag = "domains",
    params(("id" = DomainId, Path)),
    responses((status = 204, description = "Done.")),
)]
pub async fn delete(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<DomainId>,
) -> Result<StatusCode, ApiError> {
    control.delete_domain(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
