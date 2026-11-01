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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    name: String,
    role: Role,
    /// `{ "scope": "project" | "environment", "scope_id": … }`, or absent for wherever the user has access.
    #[serde(default)]
    scope: Option<MembershipScope>,
    #[serde(default)]
    expires_at: Option<Timestamp>,
}

#[derive(Serialize)]
pub struct Created {
    /// The bearer token. Shown only this once.
    token: String,
    api_token: ApiToken,
}

#[derive(Serialize)]
pub struct List {
    items: Vec<ApiToken>,
}

/// The user's tokens that are not revoked.
pub async fn list(State(control): State<Control>, Authenticated(principal): Authenticated) -> Result<Json<List>, ApiError> {
    Ok(Json(List { items: control.list_api_tokens(&principal).await? }))
}

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
pub async fn revoke(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ApiTokenId>,
) -> Result<StatusCode, ApiError> {
    control.revoke_api_token(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
