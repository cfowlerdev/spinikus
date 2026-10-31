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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    name: String,
    value: String,
    /// `runtime` if omitted.
    availability: Option<VariableAvailability>,
    #[serde(default)]
    secret: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    name: Option<String>,
    /// Omitted keeps the current value, which a secret's must be to keep it.
    value: Option<String>,
    availability: Option<VariableAvailability>,
    secret: Option<bool>,
}

#[derive(Serialize)]
struct Revealed<'a> {
    value: &'a str,
}

pub async fn list_project(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ProjectId>,
) -> Result<Json<Vec<Variable>>, ApiError> {
    Ok(Json(control.list_variables(&principal, VariableScope::Project(id)).await?))
}

pub async fn create_project(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ProjectId>,
    Body(body): Body<Create>,
) -> Result<Response, ApiError> {
    create(control, principal, VariableScope::Project(id), body).await
}

pub async fn list_environment(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<EnvironmentId>,
) -> Result<Json<Vec<Variable>>, ApiError> {
    Ok(Json(control.list_variables(&principal, VariableScope::Environment(id)).await?))
}

pub async fn create_environment(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<EnvironmentId>,
    Body(body): Body<Create>,
) -> Result<Response, ApiError> {
    create(control, principal, VariableScope::Environment(id), body).await
}

pub async fn list_service(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<ServiceId>,
) -> Result<Json<Vec<Variable>>, ApiError> {
    Ok(Json(control.list_variables(&principal, VariableScope::Service(id)).await?))
}

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

pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<VariableId>,
) -> Result<Response, ApiError> {
    let variable = control.get_variable(&principal, id).await?;
    Ok(tagged(variable.revision, variable))
}

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

pub async fn delete(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<VariableId>,
) -> Result<StatusCode, ApiError> {
    control.delete_variable(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The variable's value, secret or not; admins only, and audited. Never cached.
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
