//! Storage destinations, backup schedules, backups and restores. Secret access keys are
//! write-only.

use axum::{
    extract::State,
    http::StatusCode,
    response::{Json, Response},
};
use serde::Deserialize;
use spinikus_control::{BackupScheduleChanges, Control, NewBackupSchedule, NewStorageDestination, StorageDestinationChanges};
use spinikus_core::{
    backup::{Backup, BackupSchedule, Restore, StorageDestination},
    id::{BackupId, BackupScheduleId, RestoreId, ServiceId, StorageDestinationId, VolumeId},
    secret::Secret,
};

use crate::{
    error::ApiError,
    extract::{Authenticated, Body, IfMatch, Path, Query},
    response::{PageBody, PageParams, created, located, tagged},
    routes::present,
};

#[derive(Deserialize, utoipa::ToSchema)]
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

#[derive(Deserialize, utoipa::ToSchema)]
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

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateSchedule {
    destination_id: StorageDestinationId,
    cron: String,
    keep_last: Option<u32>,
    enabled: Option<bool>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateSchedule {
    destination_id: Option<StorageDestinationId>,
    cron: Option<String>,
    keep_last: Option<u32>,
    enabled: Option<bool>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateBackup {
    destination_id: StorageDestinationId,
    /// The volume to back up, for a service that isn't a database; it may be left out when
    /// the service has only one. A database is backed up whole, with its engine's dump.
    #[serde(default)]
    volume_id: Option<VolumeId>,
}

/// Lists the storage destinations.
#[utoipa::path(
    get,
    path = "/storage-destinations",
    operation_id = "backups_list_destinations",
    tag = "backups",
    responses((status = 200, body = Vec<StorageDestination>)),
)]
pub async fn list_destinations(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
) -> Result<Json<Vec<StorageDestination>>, ApiError> {
    Ok(Json(control.list_storage_destinations(&principal).await?))
}

/// Creates a storage destination, for instance admins.
#[utoipa::path(
    post,
    path = "/storage-destinations",
    operation_id = "backups_create_destination",
    tag = "backups",
    request_body = CreateDestination,
    responses((status = 201, body = StorageDestination, headers(("Location" = String, description = "Where the new resource is."), ("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
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

/// Gets a storage destination.
#[utoipa::path(
    get,
    path = "/storage-destinations/{id}",
    operation_id = "backups_show_destination",
    tag = "backups",
    params(("id" = StorageDestinationId, Path)),
    responses((status = 200, body = StorageDestination, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn show_destination(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<StorageDestinationId>,
) -> Result<Response, ApiError> {
    let destination = control.get_storage_destination(&principal, id).await?;
    Ok(tagged(destination.revision, destination))
}

/// Updates a storage destination.
#[utoipa::path(
    patch,
    path = "/storage-destinations/{id}",
    operation_id = "backups_update_destination",
    tag = "backups",
    params(("id" = StorageDestinationId, Path), crate::extract::IfMatch),
    request_body = UpdateDestination,
    responses((status = 200, body = StorageDestination, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
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

/// Deletes a storage destination no schedule uses.
#[utoipa::path(
    delete,
    path = "/storage-destinations/{id}",
    operation_id = "backups_delete_destination",
    tag = "backups",
    params(("id" = StorageDestinationId, Path)),
    responses((status = 204, description = "Done.")),
)]
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
#[utoipa::path(
    post,
    path = "/storage-destinations/{id}/test",
    operation_id = "backups_test_destination",
    tag = "backups",
    params(("id" = StorageDestinationId, Path)),
    responses((status = 204, description = "Done.")),
)]
pub async fn test_destination(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<StorageDestinationId>,
) -> Result<StatusCode, ApiError> {
    control.test_storage_destination(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Lists a database's or a service's backup schedules.
#[utoipa::path(
    get,
    path = "/services/{id}/backup-schedules",
    operation_id = "backups_list_schedules",
    tag = "backups",
    params(("id" = ServiceId, Path)),
    responses((status = 200, body = Vec<BackupSchedule>)),
)]
pub async fn list_schedules(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<Json<Vec<BackupSchedule>>, ApiError> {
    Ok(Json(control.list_backup_schedules(&principal, service_id).await?))
}

/// Creates a backup schedule for a database, or for a service with volumes: each run backs
/// up the database, or each of the service's volumes, and `keep_last` counts each volume's
/// backups on their own.
#[utoipa::path(
    post,
    path = "/services/{id}/backup-schedules",
    operation_id = "backups_create_schedule",
    tag = "backups",
    params(("id" = ServiceId, Path)),
    request_body = CreateSchedule,
    responses((status = 201, body = BackupSchedule, headers(("Location" = String, description = "Where the new resource is."), ("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
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

/// Gets a backup schedule.
#[utoipa::path(
    get,
    path = "/backup-schedules/{id}",
    operation_id = "backups_show_schedule",
    tag = "backups",
    params(("id" = BackupScheduleId, Path)),
    responses((status = 200, body = BackupSchedule, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn show_schedule(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<BackupScheduleId>,
) -> Result<Response, ApiError> {
    let schedule = control.get_backup_schedule(&principal, id).await?;
    Ok(tagged(schedule.revision, schedule))
}

/// Updates a backup schedule.
#[utoipa::path(
    patch,
    path = "/backup-schedules/{id}",
    operation_id = "backups_update_schedule",
    tag = "backups",
    params(("id" = BackupScheduleId, Path), crate::extract::IfMatch),
    request_body = UpdateSchedule,
    responses((status = 200, body = BackupSchedule, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
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

/// Deletes a backup schedule.
#[utoipa::path(
    delete,
    path = "/backup-schedules/{id}",
    operation_id = "backups_delete_schedule",
    tag = "backups",
    params(("id" = BackupScheduleId, Path)),
    responses((status = 204, description = "Done.")),
)]
pub async fn delete_schedule(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<BackupScheduleId>,
) -> Result<StatusCode, ApiError> {
    control.delete_backup_schedule(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The service's backups, newest first.
#[utoipa::path(
    get,
    path = "/services/{id}/backups",
    operation_id = "backups_list",
    tag = "backups",
    params(("id" = ServiceId, Path), crate::response::PageQuery),
    responses((status = 200, body = PageBody<Backup>)),
)]
pub async fn list(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
    Query(params): Query<PageParams<BackupId>>,
) -> Result<Json<PageBody<Backup>>, ApiError> {
    let page = control.list_backups(&principal, service_id, params.after, params.limit()).await?;
    Ok(PageBody::new(page, |backup| backup.id.to_string()))
}

/// Backs a database, or one of a service's volumes, up now. The backup starts `queued`.
/// A database's backup holds its engine's dump (`engine`); a volume's, a gzipped tar of it
/// (`volume_id`), made in a helper container while the service keeps running.
#[utoipa::path(
    post,
    path = "/services/{id}/backups",
    operation_id = "backups_create",
    tag = "backups",
    params(("id" = ServiceId, Path)),
    request_body = CreateBackup,
    responses((status = 201, body = Backup, headers(("Location" = String, description = "Where the new resource is.")))),
)]
pub async fn create(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
    Body(body): Body<CreateBackup>,
) -> Result<Response, ApiError> {
    let backup = control.create_backup(&principal, service_id, body.destination_id, body.volume_id).await?;
    Ok(located(format!("/api/v1/backups/{}", backup.id), backup))
}

/// Gets a backup.
#[utoipa::path(
    get,
    path = "/backups/{id}",
    operation_id = "backups_show",
    tag = "backups",
    params(("id" = BackupId, Path)),
    responses((status = 200, body = Backup)),
)]
pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<BackupId>,
) -> Result<Json<Backup>, ApiError> {
    Ok(Json(control.get_backup(&principal, id).await?))
}

/// Deletes a finished backup and its dump.
#[utoipa::path(
    delete,
    path = "/backups/{id}",
    operation_id = "backups_delete",
    tag = "backups",
    params(("id" = BackupId, Path)),
    responses((status = 204, description = "Done.")),
)]
pub async fn delete(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<BackupId>,
) -> Result<StatusCode, ApiError> {
    control.delete_backup(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Restores the backup where it came from, replacing the data there, after a safety backup
/// of it: a dump into its live database, or an archive into its volume, with the service's
/// container stopped meanwhile if it runs. The restore starts `queued`; it runs once the
/// safety backup succeeded.
#[utoipa::path(
    post,
    path = "/backups/{id}/restore",
    operation_id = "backups_restore",
    tag = "backups",
    params(("id" = BackupId, Path)),
    responses((status = 201, body = Restore, headers(("Location" = String, description = "Where the new resource is.")))),
)]
pub async fn restore(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<BackupId>,
) -> Result<Response, ApiError> {
    let restore = control.restore_backup(&principal, id).await?;
    Ok(located(format!("/api/v1/restores/{}", restore.id), restore))
}

/// The service's restores, newest first.
#[utoipa::path(
    get,
    path = "/services/{id}/restores",
    operation_id = "backups_list_restores",
    tag = "backups",
    params(("id" = ServiceId, Path), crate::response::PageQuery),
    responses((status = 200, body = PageBody<Restore>)),
)]
pub async fn list_restores(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
    Query(params): Query<PageParams<RestoreId>>,
) -> Result<Json<PageBody<Restore>>, ApiError> {
    let page = control.list_restores(&principal, service_id, params.after, params.limit()).await?;
    Ok(PageBody::new(page, |restore| restore.id.to_string()))
}

/// Gets a restore.
#[utoipa::path(
    get,
    path = "/restores/{id}",
    operation_id = "backups_show_restore",
    tag = "backups",
    params(("id" = RestoreId, Path)),
    responses((status = 200, body = Restore)),
)]
pub async fn show_restore(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<RestoreId>,
) -> Result<Json<Restore>, ApiError> {
    Ok(Json(control.get_restore(&principal, id).await?))
}
