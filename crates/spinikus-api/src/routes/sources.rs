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

#[derive(Deserialize, utoipa::ToSchema)]
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

/// Gets a service's source.
#[utoipa::path(
    get,
    path = "/services/{id}/source",
    operation_id = "sources_show",
    tag = "sources",
    params(("id" = ServiceId, Path)),
    responses((status = 200, body = spinikus_core::source::Source, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<Response, ApiError> {
    let source = control.get_source(&principal, service_id).await?;
    Ok(tagged(source.revision, source))
}

/// Sets the source of a service that has none.
#[utoipa::path(
    post,
    path = "/services/{id}/source",
    operation_id = "sources_create",
    tag = "sources",
    params(("id" = ServiceId, Path)),
    request_body = SourceBody,
    responses((status = 201, body = spinikus_core::source::Source, headers(("Location" = String, description = "Where the new resource is."), ("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
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
#[utoipa::path(
    put,
    path = "/services/{id}/source",
    operation_id = "sources_replace",
    tag = "sources",
    params(("id" = ServiceId, Path), crate::extract::IfMatch),
    request_body = SourceBody,
    responses((status = 200, body = spinikus_core::source::Source, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
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
#[utoipa::path(
    get,
    path = "/services/{id}/source/deploy-key",
    operation_id = "sources_show_deploy_key",
    tag = "sources",
    params(("id" = ServiceId, Path)),
    responses((status = 200, body = DeployKey)),
)]
pub async fn show_deploy_key(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<Json<DeployKey>, ApiError> {
    Ok(Json(control.get_deploy_key(&principal, service_id).await?))
}

/// Replaces the service's deploy key with a new one. The old key stops working for builds at once.
#[utoipa::path(
    post,
    path = "/services/{id}/source/deploy-key",
    operation_id = "sources_create_deploy_key",
    tag = "sources",
    params(("id" = ServiceId, Path)),
    responses((status = 201, body = DeployKey, headers(("Location" = String, description = "Where the new resource is.")))),
)]
pub async fn create_deploy_key(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<Response, ApiError> {
    let key = control.create_deploy_key(&principal, service_id).await?;
    Ok(located(format!("/api/v1/services/{service_id}/source/deploy-key"), key))
}

/// The service's webhook, which deploys pushes to its branch. Its secret is never shown again.
#[utoipa::path(
    get,
    path = "/services/{id}/source/webhook",
    operation_id = "sources_show_webhook",
    tag = "sources",
    params(("id" = ServiceId, Path)),
    responses((status = 200, body = Webhook)),
)]
pub async fn show_webhook(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<Json<Webhook>, ApiError> {
    Ok(Json(control.get_webhook(&principal, service_id).await?))
}

#[derive(Serialize, utoipa::ToSchema)]
struct CreatedWebhook<'a> {
    #[serde(flatten)]
    webhook: &'a Webhook,
    /// Shown only now: set it as the webhook's secret at the repository's host.
    secret: &'a str,
}

/// Turns on deploying on push, or replaces the webhook's secret (its URL stays the same).
#[utoipa::path(
    post,
    path = "/services/{id}/source/webhook",
    operation_id = "sources_create_webhook",
    tag = "sources",
    params(("id" = ServiceId, Path)),
    responses((status = 201, body = CreatedWebhook, headers(("Location" = String, description = "Where the new resource is.")))),
)]
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
#[utoipa::path(
    delete,
    path = "/services/{id}/source/webhook",
    operation_id = "sources_delete_webhook",
    tag = "sources",
    params(("id" = ServiceId, Path)),
    responses((status = 204, description = "Done.")),
)]
pub async fn delete_webhook(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<StatusCode, ApiError> {
    control.delete_webhook(&principal, service_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
