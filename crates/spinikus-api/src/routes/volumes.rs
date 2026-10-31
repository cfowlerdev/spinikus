use axum::{
    extract::State,
    http::StatusCode,
    response::{Json, Response},
};
use serde::Deserialize;
use spinikus_control::{Control, NewVolume, VolumeChanges};
use spinikus_core::{
    id::{ServiceId, VolumeId},
    volume::Volume,
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
    mount_path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    name: Option<String>,
    mount_path: Option<String>,
}

pub async fn list(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<Json<Vec<Volume>>, ApiError> {
    Ok(Json(control.list_volumes(&principal, service_id).await?))
}

pub async fn create(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
    Body(body): Body<Create>,
) -> Result<Response, ApiError> {
    let new = NewVolume { name: body.name, mount_path: body.mount_path };
    let volume = control.create_volume(&principal, service_id, new).await?;
    Ok(created(format!("/api/v1/volumes/{}", volume.id), volume.revision, volume))
}

pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<VolumeId>,
) -> Result<Response, ApiError> {
    let volume = control.get_volume(&principal, id).await?;
    Ok(tagged(volume.revision, volume))
}

/// Applies from the service's next deployment; the data stays where it is.
pub async fn update(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<VolumeId>,
    IfMatch(revision): IfMatch,
    Body(body): Body<Update>,
) -> Result<Response, ApiError> {
    let changes = VolumeChanges { name: body.name, mount_path: body.mount_path };
    let volume = control.update_volume(&principal, id, revision, changes).await?;
    Ok(tagged(volume.revision, volume))
}

pub async fn delete(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<VolumeId>,
) -> Result<StatusCode, ApiError> {
    control.delete_volume(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
