use axum::{extract::State, response::Json};
use serde::Deserialize;
use spinikus_control::Control;
use spinikus_core::{
    id::{ServerId, ServiceId},
    metrics::{MetricRange, Metrics},
};

use crate::{
    error::ApiError,
    extract::{Authenticated, Path, Query},
};

#[derive(Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct RangeParams {
    /// How far back: `1h` (the default), `6h`, `24h`, `7d` or `30d`.
    range: Option<MetricRange>,
}

/// The service's CPU, memory and network use over a range: the sum of its containers, in
/// points of `step_seconds` each, oldest first. Viewers may read it.
#[utoipa::path(
    get,
    path = "/services/{id}/metrics",
    operation_id = "metrics_service",
    tag = "metrics",
    params(("id" = ServiceId, Path), RangeParams),
    responses((status = 200, body = Metrics)),
)]
pub async fn service(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ServiceId>,
    Query(params): Query<RangeParams>,
) -> Result<Json<Metrics>, ApiError> {
    Ok(Json(control.service_metrics(&principal, id, params.range.unwrap_or_default()).await?))
}

/// The server's CPU, memory, network and disk use over a range, for instance admins.
#[utoipa::path(
    get,
    path = "/servers/{id}/metrics",
    operation_id = "metrics_server",
    tag = "metrics",
    params(("id" = ServerId, Path), RangeParams),
    responses((status = 200, body = Metrics)),
)]
pub async fn server(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ServerId>,
    Query(params): Query<RangeParams>,
) -> Result<Json<Metrics>, ApiError> {
    Ok(Json(control.server_metrics(&principal, id, params.range.unwrap_or_default()).await?))
}
