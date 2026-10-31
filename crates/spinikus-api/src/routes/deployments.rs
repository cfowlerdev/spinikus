use axum::{
    extract::State,
    response::{Json, Response},
};
use spinikus_control::Control;
use spinikus_core::{
    deployment::{Deployment, DeploymentEvent},
    id::{DeploymentId, ServiceId},
};

use crate::{
    error::ApiError,
    extract::{Authenticated, Path, Query},
    response::{PageBody, PageParams, located},
};

/// The service's deployments, newest first.
pub async fn list(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
    Query(params): Query<PageParams<DeploymentId>>,
) -> Result<Json<PageBody<Deployment>>, ApiError> {
    let page = control.list_deployments(&principal, service_id, params.after, params.limit()).await?;
    Ok(PageBody::new(page, |deployment| deployment.id.to_string()))
}

/// Deploys the service as it is now. The deployment starts `queued`.
pub async fn create(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<Response, ApiError> {
    let deployment = control.create_deployment(&principal, service_id).await?;
    Ok(located(format!("/api/v1/deployments/{}", deployment.id), deployment))
}

pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<DeploymentId>,
) -> Result<Json<Deployment>, ApiError> {
    Ok(Json(control.get_deployment(&principal, id).await?))
}

/// Every status change, oldest first.
pub async fn events(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<DeploymentId>,
) -> Result<Json<Vec<DeploymentEvent>>, ApiError> {
    Ok(Json(control.list_deployment_events(&principal, id).await?))
}

pub async fn cancel(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<DeploymentId>,
) -> Result<Json<Deployment>, ApiError> {
    Ok(Json(control.cancel_deployment(&principal, id).await?))
}
