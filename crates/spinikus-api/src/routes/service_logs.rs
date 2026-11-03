use std::convert::Infallible;

use axum::{
    extract::State,
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::json;
use spinikus_control::{
    Control,
    service_logs::{ContainerLogUpdate, LogQuery},
};
use spinikus_core::id::{DeploymentId, ServiceId};
use tokio_stream::StreamExt;

use crate::{
    error::ApiError,
    extract::{Authenticated, Path, Query},
};

/// How many lines already written come first, unless `tail` says otherwise.
const DEFAULT_TAIL: u32 = 100;

#[derive(Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct LogParams {
    deployment_id: Option<DeploymentId>,
    since: Option<Timestamp>,
    tail: Option<u32>,
    #[serde(default)]
    follow: bool,
}

/// The service's runtime logs as server-sent events, from its live deployment's container or
/// `deployment_id`'s: a `log` event per line (`{ "at", "stream", "text" }`), then `end`
/// (`{ "error" }`) when there are no more. With `follow=true` new lines keep coming until
/// the client goes away.
#[utoipa::path(
    get,
    path = "/services/{id}/logs",
    operation_id = "service_logs_show",
    tag = "logs",
    params(("id" = ServiceId, Path), LogParams),
    responses((status = 200, content_type = "text/event-stream", body = String, description = "`log` events, then `end`.")),
)]
pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
    Query(params): Query<LogParams>,
) -> Result<Response, ApiError> {
    let query = LogQuery {
        deployment_id: params.deployment_id,
        since: params.since,
        tail: Some(params.tail.unwrap_or(DEFAULT_TAIL)).filter(|tail| *tail > 0),
        follow: params.follow,
    };
    let updates = control.service_logs(&principal, service_id, query).await?;
    let events = updates.map(|update| {
        Ok::<_, Infallible>(match update {
            ContainerLogUpdate::Line { at, stderr, text } => {
                let stream = if stderr { "stderr" } else { "stdout" };
                Event::default().event("log").data(json!({ "at": at, "stream": stream, "text": text }).to_string())
            }
            ContainerLogUpdate::End { error } => Event::default().event("end").data(json!({ "error": error }).to_string()),
        })
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::default()).into_response())
}
