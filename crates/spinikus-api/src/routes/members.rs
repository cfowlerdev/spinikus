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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Add {
    /// The user's email; they must already have an account.
    email: String,
    role: Role,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    role: Role,
}

#[derive(Serialize)]
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

#[derive(Serialize)]
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

pub async fn list_project(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ProjectId>,
) -> Result<Json<List>, ApiError> {
    list(&control, &principal, MembershipScope::Project(id)).await
}

pub async fn add_project(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ProjectId>,
    Body(body): Body<Add>,
) -> Result<Response, ApiError> {
    add(&control, &principal, MembershipScope::Project(id), body).await
}

pub async fn list_environment(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<EnvironmentId>,
) -> Result<Json<List>, ApiError> {
    list(&control, &principal, MembershipScope::Environment(id)).await
}

pub async fn add_environment(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<EnvironmentId>,
    Body(body): Body<Add>,
) -> Result<Response, ApiError> {
    add(&control, &principal, MembershipScope::Environment(id), body).await
}

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

pub async fn delete(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<MembershipId>,
) -> Result<StatusCode, ApiError> {
    control.remove_member(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
