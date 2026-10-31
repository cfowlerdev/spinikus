//! Inbound Git webhooks, at `/hooks/git/<provider>/<webhook ID>` outside `/api/v1`. The
//! provider's signature authenticates them, not a bearer token.

use axum::{
    body::Bytes,
    extract::{State, rejection::BytesRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
};
use serde_json::json;
use spinikus_control::{
    Control,
    webhooks::{PushDelivery, PushOutcome},
};
use spinikus_core::{id::WebhookId, webhook::GitProvider};

use crate::{error::ApiError, extract::Path};

/// The largest delivery accepted. GitHub caps payloads at 25 MB, but a push that large lists
/// more commits than Spinikus needs.
pub const MAX_DELIVERY_SIZE: usize = 5 * 1024 * 1024;

/// Deploys a push, answering `202 Accepted` with the deployment, or `200 OK` with why it did
/// not deploy.
pub async fn receive(
    State(control): State<Control>,
    Path((provider, id)): Path<(String, WebhookId)>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let body = body.map_err(|rejection| ApiError::BadRequest {
        status: rejection.status(),
        code: "invalid_body",
        detail: rejection.body_text(),
    })?;
    let provider: GitProvider = provider.parse().map_err(|_| ApiError::NoRoute)?;
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    let delivery =
        PushDelivery { event: header(provider.event_header()), signature: header(provider.signature_header()), body: &body };
    Ok(match control.receive_push(provider, id, delivery).await? {
        PushOutcome::Pong => Json(json!({ "result": "pong" })).into_response(),
        PushOutcome::Ignored(reason) => Json(json!({ "result": "ignored", "reason": reason })).into_response(),
        PushOutcome::Deployed(deployment) => {
            (StatusCode::ACCEPTED, Json(json!({ "result": "deployed", "deployment_id": deployment.id }))).into_response()
        }
    })
}
