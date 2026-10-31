use std::convert::Infallible;

use axum::{
    extract::State,
    http::{HeaderMap, header},
    response::{
        IntoResponse, Json, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use serde::Deserialize;
use serde_json::json;
use spinikus_control::{Control, deployments::stream::DeploymentUpdate};
use spinikus_core::{
    deployment::{Deployment, DeploymentEvent},
    id::{DeploymentId, ServiceId},
};
use tokio_stream::StreamExt;

use crate::{
    error::ApiError,
    extract::{Authenticated, OptionalBody, Path, Query},
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

/// The deployment's log so far (the clone, build, pull and health check), as plain text.
pub async fn logs(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<DeploymentId>,
) -> Result<Response, ApiError> {
    let text = control.deployment_logs(&principal, id).await?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackBody {
    deployment_id: Option<DeploymentId>,
}

/// Rolls the service back to an earlier deployment, by default the last one replaced. The
/// body, `{ "deployment_id": … }`, is optional.
pub async fn rollback(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
    OptionalBody(body): OptionalBody<RollbackBody>,
) -> Result<Response, ApiError> {
    let target = body.and_then(|body| body.deployment_id);
    let deployment = control.rollback_service(&principal, service_id, target).await?;
    Ok(located(format!("/api/v1/deployments/{}", deployment.id), deployment))
}

/// Follows the deployment as server-sent events: `log` events (a line each, with the byte
/// offset after it as the event ID), a `status` event first and on every change, and `end`
/// (with the final status) once it is live or has finished. `Last-Event-ID` resumes after that offset.
pub async fn stream(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<DeploymentId>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let offset = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    let updates = control.follow_deployment(&principal, id, offset).await?;
    let events = updates.map(|update| {
        Ok::<_, Infallible>(match update {
            DeploymentUpdate::Log { line, offset } => Event::default().event("log").id(offset.to_string()).data(line),
            DeploymentUpdate::Status { status, reason, message } => Event::default()
                .event("status")
                .data(json!({ "status": status, "reason": reason, "message": message }).to_string()),
            // With data, since EventSource drops events without any.
            DeploymentUpdate::End(status) => Event::default().event("end").data(status.as_str()),
        })
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::default()).into_response())
}
