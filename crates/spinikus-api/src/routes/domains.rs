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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    hostname: String,
    /// Another domain of the service to redirect to.
    redirect_to: Option<DomainId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    hostname: Option<String>,
    /// `null` serves the app again; omitted keeps the redirect.
    #[serde(default, deserialize_with = "present")]
    redirect_to: Option<Option<DomainId>>,
}

/// The service's generated hostname and its domains.
pub async fn list(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<Json<ServiceDomains>, ApiError> {
    Ok(Json(control.list_domains(&principal, service_id).await?))
}

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

pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<DomainId>,
) -> Result<Response, ApiError> {
    let domain = control.get_domain(&principal, id).await?;
    Ok(tagged(domain.revision, domain))
}

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

pub async fn delete(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<DomainId>,
) -> Result<StatusCode, ApiError> {
    control.delete_domain(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
