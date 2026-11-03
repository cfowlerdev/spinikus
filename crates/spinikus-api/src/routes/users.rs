//! Users, for instance admins, and changing one's own password.

use axum::{
    extract::State,
    http::StatusCode,
    response::{Json, Response},
};
use serde::{Deserialize, Serialize};
use spinikus_control::{Control, NewUser, UserChanges};
use spinikus_core::{id::UserId, secret::Secret, token::ApiToken, user::User};

use crate::{
    error::ApiError,
    extract::{Authenticated, Body, IfMatch, Path, Query},
    response::{PageBody, PageParams, created, tagged},
};

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = UserCreate)]
pub struct Create {
    email: String,
    name: String,
    #[serde(default)]
    admin: bool,
    /// The initial password; without one the user can't sign in with a password yet.
    password: Option<Secret>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = UserUpdate)]
pub struct Update {
    email: Option<String>,
    name: Option<String>,
    admin: Option<bool>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = UserPassword)]
pub struct Password {
    password: Secret,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = UserChangePassword)]
pub struct ChangePassword {
    current_password: Secret,
    new_password: Secret,
}

#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = UserTokens)]
pub struct Tokens {
    items: Vec<ApiToken>,
}

/// Lists the users, for instance admins.
#[utoipa::path(
    get,
    path = "/users",
    operation_id = "users_list",
    tag = "users",
    params(crate::response::PageQuery),
    responses((status = 200, body = PageBody<User>)),
)]
pub async fn list(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Query(params): Query<PageParams<UserId>>,
) -> Result<Json<PageBody<User>>, ApiError> {
    let page = control.list_users(&principal, params.after, params.limit()).await?;
    Ok(PageBody::new(page, |user| user.id.to_string()))
}

/// Creates a user with an initial password, for instance admins.
#[utoipa::path(
    post,
    path = "/users",
    operation_id = "users_create",
    tag = "users",
    request_body = Create,
    responses((status = 201, body = User, headers(("Location" = String, description = "Where the new resource is."), ("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn create(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Body(body): Body<Create>,
) -> Result<Response, ApiError> {
    let new = NewUser { email: body.email, name: body.name, admin: body.admin, password: body.password };
    let user = control.create_user(&principal, new).await?;
    Ok(created(format!("/api/v1/users/{}", user.id), user.revision, user))
}

/// Gets a user.
#[utoipa::path(
    get,
    path = "/users/{id}",
    operation_id = "users_show",
    tag = "users",
    params(("id" = UserId, Path)),
    responses((status = 200, body = User, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<UserId>,
) -> Result<Response, ApiError> {
    let user = control.get_user(&principal, id).await?;
    Ok(tagged(user.revision, user))
}

/// Updates a user's email, name or admin flag, for instance admins.
#[utoipa::path(
    patch,
    path = "/users/{id}",
    operation_id = "users_update",
    tag = "users",
    params(("id" = UserId, Path), crate::extract::IfMatch),
    request_body = Update,
    responses((status = 200, body = User, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn update(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<UserId>,
    IfMatch(revision): IfMatch,
    Body(body): Body<Update>,
) -> Result<Response, ApiError> {
    let changes = UserChanges { email: body.email, name: body.name, admin: body.admin };
    let user = control.update_user(&principal, id, revision, changes).await?;
    Ok(tagged(user.revision, user))
}

/// Disables the user: they can't sign in, and their sessions and tokens stop working.
#[utoipa::path(
    post,
    path = "/users/{id}/disable",
    operation_id = "users_disable",
    tag = "users",
    params(("id" = UserId, Path)),
    responses((status = 200, body = User, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn disable(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<UserId>,
) -> Result<Response, ApiError> {
    let user = control.disable_user(&principal, id).await?;
    Ok(tagged(user.revision, user))
}

/// Enables a disabled user.
#[utoipa::path(
    post,
    path = "/users/{id}/enable",
    operation_id = "users_enable",
    tag = "users",
    params(("id" = UserId, Path)),
    responses((status = 200, body = User, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn enable(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<UserId>,
) -> Result<Response, ApiError> {
    let user = control.enable_user(&principal, id).await?;
    Ok(tagged(user.revision, user))
}

/// Sets the user's password, ending their sessions.
#[utoipa::path(
    put,
    path = "/users/{id}/password",
    operation_id = "users_reset_password",
    tag = "users",
    params(("id" = UserId, Path)),
    request_body = Password,
    responses((status = 204, description = "Done.")),
)]
pub async fn reset_password(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<UserId>,
    Body(body): Body<Password>,
) -> Result<StatusCode, ApiError> {
    control.reset_password(&principal, id, body.password).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The user's API tokens that are not revoked; revoke one with `DELETE /tokens/{id}`.
#[utoipa::path(
    get,
    path = "/users/{id}/tokens",
    operation_id = "users_tokens",
    tag = "users",
    params(("id" = UserId, Path)),
    responses((status = 200, body = Tokens)),
)]
pub async fn tokens(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<UserId>,
) -> Result<Json<Tokens>, ApiError> {
    Ok(Json(Tokens { items: control.list_user_api_tokens(&principal, id).await? }))
}

/// Changes the caller's own password; their other sessions end.
#[utoipa::path(
    put,
    path = "/me/password",
    operation_id = "users_change_password",
    tag = "users",
    request_body = ChangePassword,
    responses((status = 204, description = "Done.")),
)]
pub async fn change_password(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Body(body): Body<ChangePassword>,
) -> Result<StatusCode, ApiError> {
    control.change_password(&principal, body.current_password, body.new_password).await?;
    Ok(StatusCode::NO_CONTENT)
}
