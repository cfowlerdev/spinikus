use axum::{
    extract::State,
    http::StatusCode,
    response::{Json, Response},
};
use serde::Deserialize;
use spinikus_control::{Control, EnvironmentChanges, NewEnvironment};
use spinikus_core::{
    environment::{Environment, EnvironmentKind},
    id::{EnvironmentId, ProjectId},
};

use crate::{
    error::ApiError,
    extract::{Authenticated, Body, IfMatch, Path, Query},
    response::{PageBody, PageParams, created, tagged},
};

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = EnvironmentCreate)]
pub struct Create {
    name: String,
    slug: Option<String>,
    /// `production`, `staging` or `preview`.
    kind: String,
    /// Required for `preview` environments only.
    pr_number: Option<u32>,
    /// Whether pull requests get previews of the environment.
    #[serde(default)]
    previews: bool,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = EnvironmentUpdate)]
pub struct Update {
    name: Option<String>,
    slug: Option<String>,
    /// Turning previews on or off needs the admin role in the project.
    previews: Option<bool>,
}

/// Lists a project's environments.
#[utoipa::path(
    get,
    path = "/projects/{id}/environments",
    operation_id = "environments_list",
    tag = "environments",
    params(("id" = ProjectId, Path), crate::response::PageQuery),
    responses((status = 200, body = PageBody<Environment>)),
)]
pub async fn list(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(project_id): Path<ProjectId>,
    Query(params): Query<PageParams<EnvironmentId>>,
) -> Result<Json<PageBody<Environment>>, ApiError> {
    let page = control.list_environments(&principal, project_id, params.after, params.limit()).await?;
    Ok(PageBody::new(page, |environment| environment.id.to_string()))
}

/// Creates an environment.
#[utoipa::path(
    post,
    path = "/projects/{id}/environments",
    operation_id = "environments_create",
    tag = "environments",
    params(("id" = ProjectId, Path)),
    request_body = Create,
    responses((status = 201, body = Environment, headers(("Location" = String, description = "Where the new resource is."), ("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn create(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(project_id): Path<ProjectId>,
    Body(body): Body<Create>,
) -> Result<Response, ApiError> {
    let kind = EnvironmentKind::from_parts(&body.kind, body.pr_number).map_err(|error| ApiError::invalid("kind", error))?;
    let new = NewEnvironment { name: body.name, slug: body.slug, kind, previews: body.previews };
    let environment = control.create_environment(&principal, project_id, new).await?;
    Ok(created(format!("/api/v1/environments/{}", environment.id), environment.revision, environment))
}

/// Gets an environment.
#[utoipa::path(
    get,
    path = "/environments/{id}",
    operation_id = "environments_show",
    tag = "environments",
    params(("id" = EnvironmentId, Path)),
    responses((status = 200, body = Environment, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<EnvironmentId>,
) -> Result<Response, ApiError> {
    let environment = control.get_environment(&principal, id).await?;
    Ok(tagged(environment.revision, environment))
}

/// Renames an environment or turns its previews on or off.
#[utoipa::path(
    patch,
    path = "/environments/{id}",
    operation_id = "environments_update",
    tag = "environments",
    params(("id" = EnvironmentId, Path), crate::extract::IfMatch),
    request_body = Update,
    responses((status = 200, body = Environment, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn update(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<EnvironmentId>,
    IfMatch(revision): IfMatch,
    Body(body): Body<Update>,
) -> Result<Response, ApiError> {
    let changes = EnvironmentChanges { name: body.name, slug: body.slug, previews: body.previews };
    let environment = control.update_environment(&principal, id, revision, changes).await?;
    Ok(tagged(environment.revision, environment))
}

/// Deletes an environment.
#[utoipa::path(
    delete,
    path = "/environments/{id}",
    operation_id = "environments_delete",
    tag = "environments",
    params(("id" = EnvironmentId, Path)),
    responses((status = 204, description = "Done.")),
)]
pub async fn delete(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<EnvironmentId>,
) -> Result<StatusCode, ApiError> {
    control.delete_environment(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
