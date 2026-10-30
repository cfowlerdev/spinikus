use axum::{
    extract::State,
    http::StatusCode,
    response::{Json, Response},
};
use serde::Deserialize;
use spinikus_control::{Control, NewProject, ProjectChanges};
use spinikus_core::{id::ProjectId, project::Project};

use crate::{
    error::ApiError,
    extract::{Authenticated, Body, IfMatch, Path, Query},
    response::{PageBody, PageParams, created, tagged},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    name: String,
    slug: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    name: Option<String>,
    slug: Option<String>,
}

pub async fn list(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Query(params): Query<PageParams<ProjectId>>,
) -> Result<Json<PageBody<Project>>, ApiError> {
    let page = control.list_projects(&principal, params.after, params.limit()).await?;
    Ok(PageBody::new(page, |project| project.id.to_string()))
}

pub async fn create(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Body(body): Body<Create>,
) -> Result<Response, ApiError> {
    let project = control.create_project(&principal, NewProject { name: body.name, slug: body.slug }).await?;
    Ok(created(format!("/api/v1/projects/{}", project.id), project.revision, project))
}

pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ProjectId>,
) -> Result<Response, ApiError> {
    let project = control.get_project(&principal, id).await?;
    Ok(tagged(project.revision, project))
}

pub async fn update(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ProjectId>,
    IfMatch(revision): IfMatch,
    Body(body): Body<Update>,
) -> Result<Response, ApiError> {
    let changes = ProjectChanges { name: body.name, slug: body.slug };
    let project = control.update_project(&principal, id, revision, changes).await?;
    Ok(tagged(project.revision, project))
}

pub async fn delete(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ProjectId>,
) -> Result<StatusCode, ApiError> {
    control.delete_project(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
