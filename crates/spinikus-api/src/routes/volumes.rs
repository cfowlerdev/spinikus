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

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = VolumeCreate)]
pub struct Create {
    name: String,
    mount_path: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = VolumeUpdate)]
pub struct Update {
    name: Option<String>,
    mount_path: Option<String>,
}

/// Lists a service's volumes.
#[utoipa::path(
    get,
    path = "/services/{id}/volumes",
    operation_id = "volumes_list",
    tag = "volumes",
    params(("id" = ServiceId, Path)),
    responses((status = 200, body = Vec<Volume>)),
)]
pub async fn list(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<Json<Vec<Volume>>, ApiError> {
    Ok(Json(control.list_volumes(&principal, service_id).await?))
}

/// Adds a volume to a service, from its next deployment.
#[utoipa::path(
    post,
    path = "/services/{id}/volumes",
    operation_id = "volumes_create",
    tag = "volumes",
    params(("id" = ServiceId, Path)),
    request_body = Create,
    responses((status = 201, body = Volume, headers(("Location" = String, description = "Where the new resource is."), ("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
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

/// Gets a volume.
#[utoipa::path(
    get,
    path = "/volumes/{id}",
    operation_id = "volumes_show",
    tag = "volumes",
    params(("id" = VolumeId, Path)),
    responses((status = 200, body = Volume, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<VolumeId>,
) -> Result<Response, ApiError> {
    let volume = control.get_volume(&principal, id).await?;
    Ok(tagged(volume.revision, volume))
}

/// Applies from the service's next deployment; the data stays where it is.
#[utoipa::path(
    patch,
    path = "/volumes/{id}",
    operation_id = "volumes_update",
    tag = "volumes",
    params(("id" = VolumeId, Path), crate::extract::IfMatch),
    request_body = Update,
    responses((status = 200, body = Volume, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
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

/// Removes a volume from a service; its data stays on the server.
#[utoipa::path(
    delete,
    path = "/volumes/{id}",
    operation_id = "volumes_delete",
    tag = "volumes",
    params(("id" = VolumeId, Path)),
    responses((status = 204, description = "Done.")),
)]
pub async fn delete(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<VolumeId>,
) -> Result<StatusCode, ApiError> {
    control.delete_volume(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
