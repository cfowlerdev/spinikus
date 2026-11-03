//! Backups of the instance itself, for instance admins: the one schedule (its passphrase is
//! write-only) and the backups, made on it or by hand.

use axum::{
    extract::State,
    http::StatusCode,
    response::{Json, Response},
};
use serde::Deserialize;
use spinikus_control::{Control, ControlError, InstanceBackupScheduleInput};
use spinikus_core::{
    id::{InstanceBackupId, StorageDestinationId},
    instance_backup::InstanceBackup,
    secret::Secret,
};

use crate::{
    error::ApiError,
    extract::{Authenticated, Body, IfMatch, Path, Query},
    response::{PageBody, PageParams, located, tagged},
};

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = InstanceBackupPutSchedule)]
pub struct PutSchedule {
    destination_id: StorageDestinationId,
    cron: String,
    keep_last: Option<u32>,
    enabled: Option<bool>,
    /// Required the first time; omitted keeps the current one.
    passphrase: Option<String>,
}

/// The schedule, or 404 if it was never set.
#[utoipa::path(
    get,
    path = "/instance-backup-schedule",
    operation_id = "instance_backups_show_schedule",
    tag = "instance-backups",
    responses((status = 200, body = spinikus_core::instance_backup::InstanceBackupSchedule, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn show_schedule(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
) -> Result<Response, ApiError> {
    match control.get_instance_backup_schedule(&principal).await? {
        Some(schedule) => Ok(tagged(schedule.revision, schedule)),
        None => Err(ControlError::NotFound { entity: "instance backup schedule", id: String::new() }.into()),
    }
}

/// Sets the schedule. `If-Match: "0"` sets one that was never set.
#[utoipa::path(
    put,
    path = "/instance-backup-schedule",
    operation_id = "instance_backups_put_schedule",
    tag = "instance-backups",
    params(crate::extract::IfMatch),
    request_body = PutSchedule,
    responses((status = 200, body = spinikus_core::instance_backup::InstanceBackupSchedule, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn put_schedule(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    IfMatch(revision): IfMatch,
    Body(body): Body<PutSchedule>,
) -> Result<Response, ApiError> {
    let input = InstanceBackupScheduleInput {
        destination_id: body.destination_id,
        cron: body.cron,
        keep_last: body.keep_last,
        enabled: body.enabled.unwrap_or(true),
        passphrase: body.passphrase.map(Secret::new),
    };
    let schedule = control.set_instance_backup_schedule(&principal, revision, input).await?;
    Ok(tagged(schedule.revision, schedule))
}

/// Lists the instance's backups.
#[utoipa::path(
    get,
    path = "/instance-backups",
    operation_id = "instance_backups_list",
    tag = "instance-backups",
    params(crate::response::PageQuery),
    responses((status = 200, body = PageBody<InstanceBackup>)),
)]
pub async fn list(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Query(params): Query<PageParams<InstanceBackupId>>,
) -> Result<Json<PageBody<InstanceBackup>>, ApiError> {
    let page = control.list_instance_backups(&principal, params.after, params.limit()).await?;
    Ok(PageBody::new(page, |backup| backup.id.to_string()))
}

/// Backs the instance up now, to the schedule's destination with its passphrase. The backup
/// starts `queued`.
#[utoipa::path(
    post,
    path = "/instance-backups",
    operation_id = "instance_backups_create",
    tag = "instance-backups",
    responses((status = 201, body = InstanceBackup, headers(("Location" = String, description = "Where the new resource is.")))),
)]
pub async fn create(State(control): State<Control>, Authenticated(principal): Authenticated) -> Result<Response, ApiError> {
    let backup = control.back_up_instance(&principal).await?;
    Ok(located(format!("/api/v1/instance-backups/{}", backup.id), backup))
}

/// Gets an instance backup.
#[utoipa::path(
    get,
    path = "/instance-backups/{id}",
    operation_id = "instance_backups_show",
    tag = "instance-backups",
    params(("id" = InstanceBackupId, Path)),
    responses((status = 200, body = InstanceBackup)),
)]
pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<InstanceBackupId>,
) -> Result<Json<InstanceBackup>, ApiError> {
    Ok(Json(control.get_instance_backup(&principal, id).await?))
}

/// Deletes a finished backup and its file.
#[utoipa::path(
    delete,
    path = "/instance-backups/{id}",
    operation_id = "instance_backups_delete",
    tag = "instance-backups",
    params(("id" = InstanceBackupId, Path)),
    responses((status = 204, description = "Done.")),
)]
pub async fn delete(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<InstanceBackupId>,
) -> Result<StatusCode, ApiError> {
    control.delete_instance_backup(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
