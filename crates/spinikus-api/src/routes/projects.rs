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

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = ProjectCreate)]
pub struct Create {
    name: String,
    slug: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = ProjectUpdate)]
pub struct Update {
    name: Option<String>,
    slug: Option<String>,
}

/// Lists the projects the caller has a role in.
#[utoipa::path(
    get,
    path = "/projects",
    operation_id = "projects_list",
    tag = "projects",
    params(crate::response::PageQuery),
    responses((status = 200, body = PageBody<Project>)),
)]
pub async fn list(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Query(params): Query<PageParams<ProjectId>>,
) -> Result<Json<PageBody<Project>>, ApiError> {
    let page = control.list_projects(&principal, params.after, params.limit()).await?;
    Ok(PageBody::new(page, |project| project.id.to_string()))
}

/// Creates a project, for instance admins.
#[utoipa::path(
    post,
    path = "/projects",
    operation_id = "projects_create",
    tag = "projects",
    request_body = Create,
    responses((status = 201, body = Project, headers(("Location" = String, description = "Where the new resource is."), ("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn create(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Body(body): Body<Create>,
) -> Result<Response, ApiError> {
    let project = control.create_project(&principal, NewProject { name: body.name, slug: body.slug }).await?;
    Ok(created(format!("/api/v1/projects/{}", project.id), project.revision, project))
}

/// Gets a project.
#[utoipa::path(
    get,
    path = "/projects/{id}",
    operation_id = "projects_show",
    tag = "projects",
    params(("id" = ProjectId, Path)),
    responses((status = 200, body = Project, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ProjectId>,
) -> Result<Response, ApiError> {
    let project = control.get_project(&principal, id).await?;
    Ok(tagged(project.revision, project))
}

/// Renames a project.
#[utoipa::path(
    patch,
    path = "/projects/{id}",
    operation_id = "projects_update",
    tag = "projects",
    params(("id" = ProjectId, Path), crate::extract::IfMatch),
    request_body = Update,
    responses((status = 200, body = Project, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
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

/// Deletes a project with no environments.
#[utoipa::path(
    delete,
    path = "/projects/{id}",
    operation_id = "projects_delete",
    tag = "projects",
    params(("id" = ProjectId, Path)),
    responses((status = 204, description = "Done.")),
)]
pub async fn delete(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ProjectId>,
) -> Result<StatusCode, ApiError> {
    control.delete_project(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
