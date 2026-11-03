//! The signed-in user's own API tokens.

use axum::{Json, extract::State, http::StatusCode};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use spinikus_control::{Control, NewToken};
use spinikus_core::{
    id::ApiTokenId,
    membership::{MembershipScope, Role},
    token::ApiToken,
};

use crate::{
    error::ApiError,
    extract::{Authenticated, Body, Path},
};

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = TokenCreate)]
pub struct Create {
    name: String,
    role: Role,
    /// `{ "scope": "project" | "environment", "scope_id": … }`, or absent for wherever the user has access.
    #[serde(default)]
    scope: Option<MembershipScope>,
    #[serde(default)]
    expires_at: Option<Timestamp>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = TokenCreated)]
pub struct Created {
    /// The bearer token. Shown only this once.
    token: String,
    api_token: ApiToken,
}

#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = TokenList)]
pub struct List {
    items: Vec<ApiToken>,
}

/// The user's tokens that are not revoked.
#[utoipa::path(
    get,
    path = "/tokens",
    operation_id = "tokens_list",
    tag = "tokens",
    responses((status = 200, body = List)),
)]
pub async fn list(State(control): State<Control>, Authenticated(principal): Authenticated) -> Result<Json<List>, ApiError> {
    Ok(Json(List { items: control.list_api_tokens(&principal).await? }))
}

/// Creates an API token, never wider than the one in use.
#[utoipa::path(
    post,
    path = "/tokens",
    operation_id = "tokens_create",
    tag = "tokens",
    request_body = Create,
    responses((status = 201, body = Created)),
)]
pub async fn create(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Body(body): Body<Create>,
) -> Result<(StatusCode, Json<Created>), ApiError> {
    let new = NewToken { name: body.name, role: body.role, scope: body.scope, expires_at: body.expires_at };
    let (api_token, secret) = control.create_api_token(&principal, new).await?;
    Ok((StatusCode::CREATED, Json(Created { token: secret.expose().to_string(), api_token })))
}

/// Revokes one of the user's tokens, which may be the one in use.
#[utoipa::path(
    delete,
    path = "/tokens/{id}",
    operation_id = "tokens_revoke",
    tag = "tokens",
    params(("id" = ApiTokenId, Path)),
    responses((status = 204, description = "Done.")),
)]
pub async fn revoke(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ApiTokenId>,
) -> Result<StatusCode, ApiError> {
    control.revoke_api_token(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
