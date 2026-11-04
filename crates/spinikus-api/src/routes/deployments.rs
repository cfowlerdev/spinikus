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
    service::Service,
};
use tokio_stream::StreamExt;

use crate::{
    error::ApiError,
    extract::{Authenticated, OptionalBody, Path, Query},
    response::{PageBody, PageParams, located},
};

/// The service's deployments, newest first.
#[utoipa::path(
    get,
    path = "/services/{id}/deployments",
    operation_id = "deployments_list",
    tag = "deployments",
    params(("id" = ServiceId, Path), crate::response::PageQuery),
    responses((status = 200, body = PageBody<Deployment>)),
)]
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
#[utoipa::path(
    post,
    path = "/services/{id}/deployments",
    operation_id = "deployments_create",
    tag = "deployments",
    params(("id" = ServiceId, Path)),
    responses((status = 201, body = Deployment, headers(("Location" = String, description = "Where the new resource is.")))),
)]
pub async fn create(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<Response, ApiError> {
    let deployment = control.create_deployment(&principal, service_id).await?;
    Ok(located(format!("/api/v1/deployments/{}", deployment.id), deployment))
}

/// Gets a deployment.
#[utoipa::path(
    get,
    path = "/deployments/{id}",
    operation_id = "deployments_show",
    tag = "deployments",
    params(("id" = DeploymentId, Path)),
    responses((status = 200, body = Deployment)),
)]
pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<DeploymentId>,
) -> Result<Json<Deployment>, ApiError> {
    Ok(Json(control.get_deployment(&principal, id).await?))
}

/// Every status change, oldest first.
#[utoipa::path(
    get,
    path = "/deployments/{id}/events",
    operation_id = "deployments_events",
    tag = "deployments",
    params(("id" = DeploymentId, Path)),
    responses((status = 200, body = Vec<DeploymentEvent>)),
)]
pub async fn events(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<DeploymentId>,
) -> Result<Json<Vec<DeploymentEvent>>, ApiError> {
    Ok(Json(control.list_deployment_events(&principal, id).await?))
}

/// Cancels a deployment in progress.
#[utoipa::path(
    post,
    path = "/deployments/{id}/cancel",
    operation_id = "deployments_cancel",
    tag = "deployments",
    params(("id" = DeploymentId, Path)),
    responses((status = 200, body = Deployment)),
)]
pub async fn cancel(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<DeploymentId>,
) -> Result<Json<Deployment>, ApiError> {
    Ok(Json(control.cancel_deployment(&principal, id).await?))
}

/// The deployment's log so far (the clone, build, pull and health check), as plain text.
#[utoipa::path(
    get,
    path = "/deployments/{id}/logs",
    operation_id = "deployments_logs",
    tag = "deployments",
    params(("id" = DeploymentId, Path)),
    responses((status = 200, content_type = "text/plain", body = String, description = "The log so far.")),
)]
pub async fn logs(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<DeploymentId>,
) -> Result<Response, ApiError> {
    let text = control.deployment_logs(&principal, id).await?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RollbackBody {
    deployment_id: Option<DeploymentId>,
}

/// Rolls the service back to an earlier deployment, by default the last one replaced. The
/// body, `{ "deployment_id": … }`, is optional.
#[utoipa::path(
    post,
    path = "/services/{id}/rollback",
    operation_id = "deployments_rollback",
    tag = "deployments",
    params(("id" = ServiceId, Path)),
    request_body(content = Option<RollbackBody>, description = "Optional."),
    responses((status = 201, body = Deployment, headers(("Location" = String, description = "Where the new resource is.")))),
)]
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

/// Restarts the service's live container, answering with its live deployment once the
/// agent has.
#[utoipa::path(
    post,
    path = "/services/{id}/restart",
    operation_id = "deployments_restart",
    tag = "deployments",
    params(("id" = ServiceId, Path)),
    responses((status = 200, body = Deployment)),
)]
pub async fn restart(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<Json<Deployment>, ApiError> {
    Ok(Json(control.restart_service(&principal, service_id).await?))
}

/// Stops the service's live container, answering with the service (`stopped_at` set) once the
/// agent has. It stays stopped until it is started, or another deployment goes live.
#[utoipa::path(
    post,
    path = "/services/{id}/stop",
    operation_id = "deployments_stop",
    tag = "deployments",
    params(("id" = ServiceId, Path)),
    responses((status = 200, body = Service)),
)]
pub async fn stop(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<Json<Service>, ApiError> {
    Ok(Json(control.stop_service(&principal, service_id).await?))
}

/// Starts the service's stopped live container again, answering with the service once the
/// agent has.
#[utoipa::path(
    post,
    path = "/services/{id}/start",
    operation_id = "deployments_start",
    tag = "deployments",
    params(("id" = ServiceId, Path)),
    responses((status = 200, body = Service)),
)]
pub async fn start(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<Json<Service>, ApiError> {
    Ok(Json(control.start_service(&principal, service_id).await?))
}

/// Follows the deployment as server-sent events: `log` events (a line each, with the byte
/// offset after it as the event ID), a `status` event first and on every change, and `end`
/// (with the final status) once it is live or has finished. `Last-Event-ID` resumes after that offset.
#[utoipa::path(
    get,
    path = "/deployments/{id}/stream",
    operation_id = "deployments_stream",
    tag = "deployments",
    params(("id" = DeploymentId, Path)),
    responses((status = 200, content_type = "text/event-stream", body = String, description = "`log`, `status` and `end` events.")),
)]
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
