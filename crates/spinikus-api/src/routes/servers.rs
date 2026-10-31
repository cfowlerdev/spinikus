use axum::{
    extract::State,
    http::StatusCode,
    response::{Json, Response},
};
use serde::{Deserialize, Serialize};
use spinikus_control::{Control, NewServer};
use spinikus_core::{enrollment::EnrollmentToken, id::ServerId, server::Server};

use crate::{
    error::ApiError,
    extract::{Authenticated, Body, Path, Query},
    response::{PageBody, PageParams, created, tagged},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    name: String,
}

pub async fn list(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Query(params): Query<PageParams<ServerId>>,
) -> Result<Json<PageBody<Server>>, ApiError> {
    let page = control.list_servers(&principal, params.after, params.limit()).await?;
    Ok(PageBody::new(page, |server| server.id.to_string()))
}

pub async fn create(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Body(body): Body<Create>,
) -> Result<Response, ApiError> {
    let server = control.create_server(&principal, NewServer { name: body.name }).await?;
    Ok(created(format!("/api/v1/servers/{}", server.id), server.revision, server))
}

pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ServerId>,
) -> Result<Response, ApiError> {
    let server = control.get_server(&principal, id).await?;
    Ok(tagged(server.revision, server))
}

/// Revokes the server: its agent is disconnected and can never connect again.
pub async fn revoke(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ServerId>,
) -> Result<StatusCode, ApiError> {
    control.revoke_server(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct JoinTokenResponse {
    /// The join token for `spinikus agent join`. Shown only this once.
    token: String,
    enrollment_token: EnrollmentToken,
    /// The CA fingerprint the agent checks before trusting the control plane.
    ca_fingerprint: String,
}

pub async fn create_join_token(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ServerId>,
) -> Result<(StatusCode, Json<JoinTokenResponse>), ApiError> {
    let join = control.create_join_token(&principal, id).await?;
    let body = JoinTokenResponse {
        token: join.secret.expose().to_string(),
        enrollment_token: join.token,
        ca_fingerprint: join.ca_fingerprint,
    };
    Ok((StatusCode::CREATED, Json(body)))
}
