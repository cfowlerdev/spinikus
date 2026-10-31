use axum::{
    extract::State,
    http::StatusCode,
    response::{Json, Response},
};
use serde::{Deserialize, Serialize};
use spinikus_control::Control;
use spinikus_core::{
    id::ServiceId,
    source::{DeployKey, GitSource, ImageSource, SourceSpec},
    webhook::Webhook,
};

use crate::{
    error::ApiError,
    extract::{Authenticated, Body, IfMatch, Path},
    response::{created, located, tagged},
};

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceBody {
    Git { repo_url: String, branch: String, root_directory: Option<String> },
    Image { image: String },
}

impl From<SourceBody> for SourceSpec {
    fn from(body: SourceBody) -> Self {
        match body {
            SourceBody::Git { repo_url, branch, root_directory } => Self::Git(GitSource { repo_url, branch, root_directory }),
            SourceBody::Image { image } => Self::Image(ImageSource { image }),
        }
    }
}

pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<Response, ApiError> {
    let source = control.get_source(&principal, service_id).await?;
    Ok(tagged(source.revision, source))
}

pub async fn create(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
    Body(body): Body<SourceBody>,
) -> Result<Response, ApiError> {
    let source = control.create_source(&principal, service_id, body.into()).await?;
    Ok(created(format!("/api/v1/services/{service_id}/source"), source.revision, source))
}

/// Replaces the whole source, such as to switch from an image to a Git repository.
pub async fn replace(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
    IfMatch(revision): IfMatch,
    Body(body): Body<SourceBody>,
) -> Result<Response, ApiError> {
    let source = control.replace_source(&principal, service_id, revision, body.into()).await?;
    Ok(tagged(source.revision, source))
}

/// The public half of the service's deploy key, to add to the repository's host.
pub async fn show_deploy_key(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<Json<DeployKey>, ApiError> {
    Ok(Json(control.get_deploy_key(&principal, service_id).await?))
}

/// Replaces the service's deploy key with a new one. The old key stops working for builds at once.
pub async fn create_deploy_key(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<Response, ApiError> {
    let key = control.create_deploy_key(&principal, service_id).await?;
    Ok(located(format!("/api/v1/services/{service_id}/source/deploy-key"), key))
}

/// The service's webhook, which deploys pushes to its branch. Its secret is never shown again.
pub async fn show_webhook(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<Json<Webhook>, ApiError> {
    Ok(Json(control.get_webhook(&principal, service_id).await?))
}

#[derive(Serialize)]
struct CreatedWebhook<'a> {
    #[serde(flatten)]
    webhook: &'a Webhook,
    /// Shown only now: set it as the webhook's secret at the repository's host.
    secret: &'a str,
}

/// Turns on deploying on push, or replaces the webhook's secret (its URL stays the same).
pub async fn create_webhook(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<Response, ApiError> {
    let created = control.create_webhook(&principal, service_id).await?;
    let body = CreatedWebhook { webhook: &created.webhook, secret: created.secret.expose() };
    Ok(located(format!("/api/v1/services/{service_id}/source/webhook"), body))
}

/// Turns off deploying on push.
pub async fn delete_webhook(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<StatusCode, ApiError> {
    control.delete_webhook(&principal, service_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
