//! The members of projects and environments, for their admins.

use axum::{
    extract::State,
    http::StatusCode,
    response::{Json, Response},
};
use serde::{Deserialize, Serialize};
use spinikus_control::{Control, Member};
use spinikus_core::{
    id::{EnvironmentId, MembershipId, ProjectId},
    membership::{Membership, MembershipScope, Role},
    user::User,
};

use crate::{
    error::ApiError,
    extract::{Authenticated, Body, IfMatch, Path},
    response::{created, tagged},
};

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = MemberAdd)]
pub struct Add {
    /// The user's email; they must already have an account.
    email: String,
    role: Role,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = MemberUpdate)]
pub struct Update {
    role: Role,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct MemberBody {
    #[serde(flatten)]
    membership: Membership,
    user: User,
}

impl From<Member> for MemberBody {
    fn from(member: Member) -> Self {
        Self { membership: member.membership, user: member.user }
    }
}

#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = MemberList)]
pub struct List {
    items: Vec<MemberBody>,
}

async fn list(
    control: &Control,
    principal: &spinikus_control::Principal,
    scope: MembershipScope,
) -> Result<Json<List>, ApiError> {
    let members = control.list_members(principal, scope).await?;
    Ok(Json(List { items: members.into_iter().map(MemberBody::from).collect() }))
}

async fn add(
    control: &Control,
    principal: &spinikus_control::Principal,
    scope: MembershipScope,
    body: Add,
) -> Result<Response, ApiError> {
    let member = control.add_member(principal, scope, &body.email, body.role).await?;
    let location = format!("/api/v1/memberships/{}", member.membership.id);
    let revision = member.membership.revision;
    Ok(created(location, revision, MemberBody::from(member)))
}

/// Lists a project's members.
#[utoipa::path(
    get,
    path = "/projects/{id}/members",
    operation_id = "members_list_project",
    tag = "members",
    params(("id" = ProjectId, Path)),
    responses((status = 200, body = List)),
)]
pub async fn list_project(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ProjectId>,
) -> Result<Json<List>, ApiError> {
    list(&control, &principal, MembershipScope::Project(id)).await
}

/// Adds a user to a project by email.
#[utoipa::path(
    post,
    path = "/projects/{id}/members",
    operation_id = "members_add_project",
    tag = "members",
    params(("id" = ProjectId, Path)),
    request_body = Add,
    responses((status = 201, body = MemberBody, headers(("Location" = String, description = "Where the new resource is."), ("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn add_project(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ProjectId>,
    Body(body): Body<Add>,
) -> Result<Response, ApiError> {
    add(&control, &principal, MembershipScope::Project(id), body).await
}

/// Lists an environment's members.
#[utoipa::path(
    get,
    path = "/environments/{id}/members",
    operation_id = "members_list_environment",
    tag = "members",
    params(("id" = EnvironmentId, Path)),
    responses((status = 200, body = List)),
)]
pub async fn list_environment(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<EnvironmentId>,
) -> Result<Json<List>, ApiError> {
    list(&control, &principal, MembershipScope::Environment(id)).await
}

/// Adds a user to an environment by email.
#[utoipa::path(
    post,
    path = "/environments/{id}/members",
    operation_id = "members_add_environment",
    tag = "members",
    params(("id" = EnvironmentId, Path)),
    request_body = Add,
    responses((status = 201, body = MemberBody, headers(("Location" = String, description = "Where the new resource is."), ("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn add_environment(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<EnvironmentId>,
    Body(body): Body<Add>,
) -> Result<Response, ApiError> {
    add(&control, &principal, MembershipScope::Environment(id), body).await
}

/// Changes a member's role.
#[utoipa::path(
    patch,
    path = "/memberships/{id}",
    operation_id = "members_update",
    tag = "members",
    params(("id" = MembershipId, Path), crate::extract::IfMatch),
    request_body = Update,
    responses((status = 200, body = Membership, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn update(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<MembershipId>,
    IfMatch(revision): IfMatch,
    Body(body): Body<Update>,
) -> Result<Response, ApiError> {
    let membership = control.update_member(&principal, id, revision, body.role).await?;
    Ok(tagged(membership.revision, membership))
}

/// Removes a member.
#[utoipa::path(
    delete,
    path = "/memberships/{id}",
    operation_id = "members_delete",
    tag = "members",
    params(("id" = MembershipId, Path)),
    responses((status = 204, description = "Done.")),
)]
pub async fn delete(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<MembershipId>,
) -> Result<StatusCode, ApiError> {
    control.remove_member(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
