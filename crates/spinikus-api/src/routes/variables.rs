use axum::{
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use serde::{Deserialize, Serialize};
use spinikus_control::{Control, NewVariable, VariableChanges};
use spinikus_core::{
    id::{EnvironmentId, ProjectId, ServiceId, VariableId},
    secret::Secret,
    variable::{Variable, VariableAvailability, VariableScope},
};

use crate::{
    error::ApiError,
    extract::{Authenticated, Body, IfMatch, Path},
    response::{created, tagged},
};

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = VariableCreate)]
pub struct Create {
    name: String,
    value: String,
    /// `runtime` if omitted.
    availability: Option<VariableAvailability>,
    #[serde(default)]
    secret: bool,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = VariableUpdate)]
pub struct Update {
    name: Option<String>,
    /// Omitted keeps the current value, which a secret's must be to keep it.
    value: Option<String>,
    availability: Option<VariableAvailability>,
    secret: Option<bool>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = VariableRevealed)]
struct Revealed<'a> {
    value: &'a str,
}

/// Lists a project's variables; secret values are left out.
#[utoipa::path(
    get,
    path = "/projects/{id}/variables",
    operation_id = "variables_list_project",
    tag = "variables",
    params(("id" = ProjectId, Path)),
    responses((status = 200, body = Vec<Variable>)),
)]
pub async fn list_project(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ProjectId>,
) -> Result<Json<Vec<Variable>>, ApiError> {
    Ok(Json(control.list_variables(&principal, VariableScope::Project(id)).await?))
}

/// Creates a project variable.
#[utoipa::path(
    post,
    path = "/projects/{id}/variables",
    operation_id = "variables_create_project",
    tag = "variables",
    params(("id" = ProjectId, Path)),
    request_body = Create,
    responses((status = 201, body = Variable, headers(("Location" = String, description = "Where the new resource is."), ("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn create_project(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ProjectId>,
    Body(body): Body<Create>,
) -> Result<Response, ApiError> {
    create(control, principal, VariableScope::Project(id), body).await
}

/// Lists an environment's variables; secret values are left out.
#[utoipa::path(
    get,
    path = "/environments/{id}/variables",
    operation_id = "variables_list_environment",
    tag = "variables",
    params(("id" = EnvironmentId, Path)),
    responses((status = 200, body = Vec<Variable>)),
)]
pub async fn list_environment(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<EnvironmentId>,
) -> Result<Json<Vec<Variable>>, ApiError> {
    Ok(Json(control.list_variables(&principal, VariableScope::Environment(id)).await?))
}

/// Creates an environment variable.
#[utoipa::path(
    post,
    path = "/environments/{id}/variables",
    operation_id = "variables_create_environment",
    tag = "variables",
    params(("id" = EnvironmentId, Path)),
    request_body = Create,
    responses((status = 201, body = Variable, headers(("Location" = String, description = "Where the new resource is."), ("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn create_environment(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<EnvironmentId>,
    Body(body): Body<Create>,
) -> Result<Response, ApiError> {
    create(control, principal, VariableScope::Environment(id), body).await
}

/// Lists a service's variables; secret values are left out.
#[utoipa::path(
    get,
    path = "/services/{id}/variables",
    operation_id = "variables_list_service",
    tag = "variables",
    params(("id" = ServiceId, Path)),
    responses((status = 200, body = Vec<Variable>)),
)]
pub async fn list_service(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ServiceId>,
) -> Result<Json<Vec<Variable>>, ApiError> {
    Ok(Json(control.list_variables(&principal, VariableScope::Service(id)).await?))
}

/// Creates a service variable.
#[utoipa::path(
    post,
    path = "/services/{id}/variables",
    operation_id = "variables_create_service",
    tag = "variables",
    params(("id" = ServiceId, Path)),
    request_body = Create,
    responses((status = 201, body = Variable, headers(("Location" = String, description = "Where the new resource is."), ("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn create_service(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ServiceId>,
    Body(body): Body<Create>,
) -> Result<Response, ApiError> {
    create(control, principal, VariableScope::Service(id), body).await
}

async fn create(
    control: Control,
    principal: spinikus_control::Principal,
    scope: VariableScope,
    body: Create,
) -> Result<Response, ApiError> {
    let new = NewVariable {
        name: body.name,
        value: Secret::new(body.value),
        availability: body.availability.unwrap_or(VariableAvailability::Runtime),
        secret: body.secret,
    };
    let variable = control.create_variable(&principal, scope, new).await?;
    Ok(created(format!("/api/v1/variables/{}", variable.id), variable.revision, variable))
}

/// Gets a variable; a secret one without its value.
#[utoipa::path(
    get,
    path = "/variables/{id}",
    operation_id = "variables_show",
    tag = "variables",
    params(("id" = VariableId, Path)),
    responses((status = 200, body = Variable, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<VariableId>,
) -> Result<Response, ApiError> {
    let variable = control.get_variable(&principal, id).await?;
    Ok(tagged(variable.revision, variable))
}

/// Updates a variable.
#[utoipa::path(
    patch,
    path = "/variables/{id}",
    operation_id = "variables_update",
    tag = "variables",
    params(("id" = VariableId, Path), crate::extract::IfMatch),
    request_body = Update,
    responses((status = 200, body = Variable, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn update(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<VariableId>,
    IfMatch(revision): IfMatch,
    Body(body): Body<Update>,
) -> Result<Response, ApiError> {
    let changes = VariableChanges {
        name: body.name,
        value: body.value.map(Secret::new),
        availability: body.availability,
        secret: body.secret,
    };
    let variable = control.update_variable(&principal, id, revision, changes).await?;
    Ok(tagged(variable.revision, variable))
}

/// Deletes a variable.
#[utoipa::path(
    delete,
    path = "/variables/{id}",
    operation_id = "variables_delete",
    tag = "variables",
    params(("id" = VariableId, Path)),
    responses((status = 204, description = "Done.")),
)]
pub async fn delete(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<VariableId>,
) -> Result<StatusCode, ApiError> {
    control.delete_variable(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The variable's value, secret or not; admins only, and audited. Never cached.
#[utoipa::path(
    post,
    path = "/variables/{id}/reveal",
    operation_id = "variables_reveal",
    tag = "variables",
    params(("id" = VariableId, Path)),
    responses((status = 200, body = Revealed, description = "The value. Not cached.")),
)]
pub async fn reveal(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<VariableId>,
) -> Result<Response, ApiError> {
    let value = control.reveal_variable(&principal, id).await?;
    let mut response = Json(Revealed { value: value.expose() }).into_response();
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}
