//! First-run setup, logging in for an API token, and the current user.

use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use spinikus_control::Control;
use spinikus_core::{secret::Secret, token::ApiToken, user::User};

use crate::{
    error::ApiError,
    extract::{Authenticated, Body, Client},
};

#[derive(Serialize, utoipa::ToSchema)]
pub struct SetupStatus {
    needs_setup: bool,
    /// Whether setting up needs the setup code the install script printed.
    setup_code_required: bool,
}

/// Whether the instance still needs its first admin.
#[utoipa::path(
    get,
    path = "/setup",
    operation_id = "session_setup_status",
    tag = "session",
    responses((status = 200, body = SetupStatus)),
)]
pub async fn setup_status(State(control): State<Control>) -> Result<Json<SetupStatus>, ApiError> {
    Ok(Json(SetupStatus { needs_setup: control.needs_setup().await?, setup_code_required: control.setup_code_required() }))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Setup {
    /// The setup code, when the instance has one.
    #[serde(default)]
    setup_code: String,
    email: String,
    name: String,
    password: Secret,
}

/// Creates the first admin. Unauthenticated, and refused once any user exists.
#[utoipa::path(
    post,
    path = "/setup",
    operation_id = "session_setup",
    tag = "session",
    request_body = Setup,
    responses((status = 201, body = User)),
)]
pub async fn setup(State(control): State<Control>, Body(setup): Body<Setup>) -> Result<(StatusCode, Json<User>), ApiError> {
    let user = control.set_up(&setup.setup_code, &setup.email, &setup.name, setup.password).await?;
    Ok((StatusCode::CREATED, Json(user)))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Login {
    email: String,
    password: Secret,
    /// What to call the token, such as "CLI on my laptop".
    #[serde(default = "default_token_name")]
    token_name: String,
}

fn default_token_name() -> String {
    "spinikus CLI".to_string()
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct LoginResponse {
    /// The bearer token. Shown only this once.
    token: String,
    api_token: ApiToken,
}

/// Exchanges an email and password for an API token, as `spinikus login` does.
#[utoipa::path(
    post,
    path = "/login",
    operation_id = "session_login",
    tag = "session",
    request_body = Login,
    responses((status = 201, body = LoginResponse)),
)]
pub async fn login(
    State(control): State<Control>,
    Client(client): Client,
    Body(login): Body<Login>,
) -> Result<(StatusCode, Json<LoginResponse>), ApiError> {
    let (api_token, secret) = control.create_login_token(&login.email, login.password, &login.token_name, client).await?;
    Ok((StatusCode::CREATED, Json(LoginResponse { token: secret.expose().to_string(), api_token })))
}

/// The user the token belongs to.
#[utoipa::path(
    get,
    path = "/me",
    operation_id = "session_me",
    tag = "session",
    responses((status = 200, body = User)),
)]
pub async fn me(Authenticated(principal): Authenticated) -> Json<User> {
    Json(principal.user)
}
