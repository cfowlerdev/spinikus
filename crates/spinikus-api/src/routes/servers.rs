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

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = ServerCreate)]
pub struct Create {
    name: String,
}

/// Lists the servers.
#[utoipa::path(
    get,
    path = "/servers",
    operation_id = "servers_list",
    tag = "servers",
    params(crate::response::PageQuery),
    responses((status = 200, body = PageBody<Server>)),
)]
pub async fn list(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Query(params): Query<PageParams<ServerId>>,
) -> Result<Json<PageBody<Server>>, ApiError> {
    let page = control.list_servers(&principal, params.after, params.limit()).await?;
    Ok(PageBody::new(page, |server| server.id.to_string()))
}

/// Adds a server, for instance admins.
#[utoipa::path(
    post,
    path = "/servers",
    operation_id = "servers_create",
    tag = "servers",
    request_body = Create,
    responses((status = 201, body = Server, headers(("Location" = String, description = "Where the new resource is."), ("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn create(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Body(body): Body<Create>,
) -> Result<Response, ApiError> {
    let server = control.create_server(&principal, NewServer { name: body.name }).await?;
    Ok(created(format!("/api/v1/servers/{}", server.id), server.revision, server))
}

/// Gets a server.
#[utoipa::path(
    get,
    path = "/servers/{id}",
    operation_id = "servers_show",
    tag = "servers",
    params(("id" = ServerId, Path)),
    responses((status = 200, body = Server, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ServerId>,
) -> Result<Response, ApiError> {
    let server = control.get_server(&principal, id).await?;
    Ok(tagged(server.revision, server))
}

/// Revokes the server: its agent is disconnected and can never connect again.
#[utoipa::path(
    post,
    path = "/servers/{id}/revoke",
    operation_id = "servers_revoke",
    tag = "servers",
    params(("id" = ServerId, Path)),
    responses((status = 204, description = "Done.")),
)]
pub async fn revoke(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ServerId>,
) -> Result<StatusCode, ApiError> {
    control.revoke_server(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = ServerJoinTokenResponse)]
pub struct JoinTokenResponse {
    /// The join token for `spinikus agent join`. Shown only this once.
    token: String,
    enrollment_token: EnrollmentToken,
    /// The CA fingerprint the agent checks before trusting the control plane.
    ca_fingerprint: String,
}

/// Creates a join token for the server's agent.
#[utoipa::path(
    post,
    path = "/servers/{id}/enrollment-tokens",
    operation_id = "servers_create_join_token",
    tag = "servers",
    params(("id" = ServerId, Path)),
    responses((status = 201, body = JoinTokenResponse)),
)]
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
