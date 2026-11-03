//! Inbound Git webhooks, at `/hooks/git/<provider>/<webhook ID>` outside `/api/v1`. The
//! provider's signature authenticates them, not a bearer token.

use axum::{
    body::Bytes,
    extract::{State, rejection::BytesRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
};
use serde::Serialize;
use spinikus_control::{
    Control,
    webhooks::{PushDelivery, PushOutcome},
};
use spinikus_core::{
    id::{DeploymentId, EnvironmentId, WebhookId},
    webhook::GitProvider,
};

use crate::{error::ApiError, extract::Path};

/// The largest delivery accepted. GitHub caps payloads at 25 MB, but a push that large lists
/// more commits than Spinikus needs.
pub const MAX_DELIVERY_SIZE: usize = 5 * 1024 * 1024;

/// What a delivery did.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum HookResult {
    /// A provider's test delivery.
    Pong,
    /// Nothing to deploy, and why.
    Ignored {
        reason: String,
    },
    Deployed {
        deployment_id: DeploymentId,
    },
    /// A pull request's preview environment, with what it deploys.
    Previewed {
        environment_id: EnvironmentId,
        deployment_ids: Vec<DeploymentId>,
    },
    /// A closed pull request's preview environment, now removed.
    PreviewRemoved {
        environment_id: EnvironmentId,
    },
}

/// Deploys a push or a pull request's preview, answering `202 Accepted` with what it
/// deploys, or `200 OK` with why it deployed nothing or with the preview it removed.
#[utoipa::path(
    post,
    path = "/git/{provider}/{id}",
    operation_id = "hooks_receive",
    tag = "hooks",
    params(
        ("provider" = String, Path, description = "`github`, `gitlab`, `gitea` or `forgejo`."),
        ("id" = WebhookId, Path),
    ),
    responses((status = 202, body = HookResult, description = "Deployed or previewed."), (status = 200, body = HookResult, description = "Nothing deployed, or a preview removed.")),
)]
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
    let (status, result) = match control.receive_push(provider, id, delivery).await? {
        PushOutcome::Pong => (StatusCode::OK, HookResult::Pong),
        PushOutcome::Ignored(reason) => (StatusCode::OK, HookResult::Ignored { reason }),
        PushOutcome::Deployed(deployment) => (StatusCode::ACCEPTED, HookResult::Deployed { deployment_id: deployment.id }),
        PushOutcome::Previewed { environment, deployments } => {
            let deployment_ids = deployments.iter().map(|deployment| deployment.id).collect();
            (StatusCode::ACCEPTED, HookResult::Previewed { environment_id: environment.id, deployment_ids })
        }
        PushOutcome::PreviewRemoved(environment) => {
            (StatusCode::OK, HookResult::PreviewRemoved { environment_id: environment.id })
        }
    };
    Ok((status, Json(result)).into_response())
}
