//! Storage destinations, backup schedules and backups. Secret access keys are write-only.

use axum::{
    extract::State,
    http::StatusCode,
    response::{Json, Response},
};
use serde::Deserialize;
use spinikus_control::{BackupScheduleChanges, Control, NewBackupSchedule, NewStorageDestination, StorageDestinationChanges};
use spinikus_core::{
    backup::{Backup, BackupSchedule, StorageDestination},
    id::{BackupId, BackupScheduleId, ServiceId, StorageDestinationId},
    secret::Secret,
};

use crate::{
    error::ApiError,
    extract::{Authenticated, Body, IfMatch, Path, Query},
    response::{PageBody, PageParams, created, located, tagged},
    routes::present,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateDestination {
    name: String,
    /// The S3 API's URL; AWS if omitted.
    endpoint: Option<String>,
    region: Option<String>,
    bucket: String,
    path_prefix: Option<String>,
    #[serde(default)]
    path_style: bool,
    access_key_id: String,
    secret_access_key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateDestination {
    name: Option<String>,
    /// `null` goes back to AWS; omitted keeps the endpoint.
    #[serde(default, deserialize_with = "present")]
    endpoint: Option<Option<String>>,
    region: Option<String>,
    bucket: Option<String>,
    path_prefix: Option<String>,
    path_style: Option<bool>,
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSchedule {
    destination_id: StorageDestinationId,
    cron: String,
    keep_last: Option<u32>,
    enabled: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateSchedule {
    destination_id: Option<StorageDestinationId>,
    cron: Option<String>,
    keep_last: Option<u32>,
    enabled: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBackup {
    destination_id: StorageDestinationId,
}

pub async fn list_destinations(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
) -> Result<Json<Vec<StorageDestination>>, ApiError> {
    Ok(Json(control.list_storage_destinations(&principal).await?))
}

pub async fn create_destination(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Body(body): Body<CreateDestination>,
) -> Result<Response, ApiError> {
    let new = NewStorageDestination {
        name: body.name,
        endpoint: body.endpoint,
        region: body.region,
        bucket: body.bucket,
        path_prefix: body.path_prefix,
        path_style: body.path_style,
        access_key_id: body.access_key_id,
        secret_access_key: Secret::new(body.secret_access_key),
    };
    let destination = control.create_storage_destination(&principal, new).await?;
    Ok(created(format!("/api/v1/storage-destinations/{}", destination.id), destination.revision, destination))
}

pub async fn show_destination(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<StorageDestinationId>,
) -> Result<Response, ApiError> {
    let destination = control.get_storage_destination(&principal, id).await?;
    Ok(tagged(destination.revision, destination))
}

pub async fn update_destination(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<StorageDestinationId>,
    IfMatch(revision): IfMatch,
    Body(body): Body<UpdateDestination>,
) -> Result<Response, ApiError> {
    let changes = StorageDestinationChanges {
        name: body.name,
        endpoint: body.endpoint,
        region: body.region,
        bucket: body.bucket,
        path_prefix: body.path_prefix,
        path_style: body.path_style,
        access_key_id: body.access_key_id,
        secret_access_key: body.secret_access_key.map(Secret::new),
    };
    let destination = control.update_storage_destination(&principal, id, revision, changes).await?;
    Ok(tagged(destination.revision, destination))
}

pub async fn delete_destination(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<StorageDestinationId>,
) -> Result<StatusCode, ApiError> {
    control.delete_storage_destination(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Writes and deletes a small object, so a destination is known to work before a backup
/// needs it. `204` if it does; a `rejected` problem says what the storage answered.
pub async fn test_destination(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<StorageDestinationId>,
) -> Result<StatusCode, ApiError> {
    control.test_storage_destination(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_schedules(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<Json<Vec<BackupSchedule>>, ApiError> {
    Ok(Json(control.list_backup_schedules(&principal, service_id).await?))
}

pub async fn create_schedule(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
    Body(body): Body<CreateSchedule>,
) -> Result<Response, ApiError> {
    let new = NewBackupSchedule {
        destination_id: body.destination_id,
        cron: body.cron,
        keep_last: body.keep_last,
        enabled: body.enabled,
    };
    let schedule = control.create_backup_schedule(&principal, service_id, new).await?;
    Ok(created(format!("/api/v1/backup-schedules/{}", schedule.id), schedule.revision, schedule))
}

pub async fn show_schedule(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<BackupScheduleId>,
) -> Result<Response, ApiError> {
    let schedule = control.get_backup_schedule(&principal, id).await?;
    Ok(tagged(schedule.revision, schedule))
}

pub async fn update_schedule(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<BackupScheduleId>,
    IfMatch(revision): IfMatch,
    Body(body): Body<UpdateSchedule>,
) -> Result<Response, ApiError> {
    let changes = BackupScheduleChanges {
        destination_id: body.destination_id,
        cron: body.cron,
        keep_last: body.keep_last,
        enabled: body.enabled,
    };
    let schedule = control.update_backup_schedule(&principal, id, revision, changes).await?;
    Ok(tagged(schedule.revision, schedule))
}

pub async fn delete_schedule(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<BackupScheduleId>,
) -> Result<StatusCode, ApiError> {
    control.delete_backup_schedule(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The service's backups, newest first.
pub async fn list(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
    Query(params): Query<PageParams<BackupId>>,
) -> Result<Json<PageBody<Backup>>, ApiError> {
    let page = control.list_backups(&principal, service_id, params.after, params.limit()).await?;
    Ok(PageBody::new(page, |backup| backup.id.to_string()))
}

/// Backs the database up now. The backup starts `queued`.
pub async fn create(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
    Body(body): Body<CreateBackup>,
) -> Result<Response, ApiError> {
    let backup = control.create_backup(&principal, service_id, body.destination_id).await?;
    Ok(located(format!("/api/v1/backups/{}", backup.id), backup))
}

pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<BackupId>,
) -> Result<Json<Backup>, ApiError> {
    Ok(Json(control.get_backup(&principal, id).await?))
}

/// Deletes a finished backup and its dump.
pub async fn delete(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<BackupId>,
) -> Result<StatusCode, ApiError> {
    control.delete_backup(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
