//! Storage destinations, backup schedules and backups.

use async_trait::async_trait;
use jiff::Timestamp;
use spinikus_core::{
    backup::{
        Backup, BackupSchedule, BackupScheduleFields, BackupStatus, NewBackup, StorageDestination, StorageDestinationFields,
    },
    id::{BackupId, BackupScheduleId, ServerId, ServiceId, StorageDestinationId},
    secret::Secret,
};

use crate::{SqliteStore, StoreError, certificates::open_sealed, db::format_timestamp, error::parse_column};

const DESTINATION: &str = "storage destination";
const SCHEDULE: &str = "backup schedule";
const BACKUP: &str = "backup";

#[async_trait]
pub trait StorageDestinationRepository: Send + Sync {
    async fn create_storage_destination(
        &self,
        fields: StorageDestinationFields,
        secret_access_key: &Secret,
    ) -> Result<StorageDestination, StoreError>;

    /// Fails with `NotFound` for deleted destinations too.
    async fn get_storage_destination(&self, id: StorageDestinationId) -> Result<StorageDestination, StoreError>;

    /// Every destination that is not deleted, by name.
    async fn list_storage_destinations(&self) -> Result<Vec<StorageDestination>, StoreError>;

    /// Replaces the fields if the destination is still at `revision`; `None` keeps the
    /// secret access key.
    async fn update_storage_destination(
        &self,
        id: StorageDestinationId,
        revision: i64,
        fields: StorageDestinationFields,
        secret_access_key: Option<&Secret>,
    ) -> Result<StorageDestination, StoreError>;

    /// Soft-deletes the destination, which frees its name. Refused while schedules use it;
    /// its backups keep pointing at it.
    async fn delete_storage_destination(&self, id: StorageDestinationId) -> Result<(), StoreError>;

    /// The secret access key, of deleted destinations too, so their backups can be removed.
    async fn storage_destination_secret(&self, id: StorageDestinationId) -> Result<Secret, StoreError>;

    /// The destination, deleted or not, for reaching the backups it holds.
    async fn get_storage_destination_any(&self, id: StorageDestinationId) -> Result<StorageDestination, StoreError>;
}

#[async_trait]
pub trait BackupScheduleRepository: Send + Sync {
    /// `NotFound` if the service or the destination does not exist or is deleted.
    async fn create_backup_schedule(
        &self,
        service_id: ServiceId,
        fields: BackupScheduleFields,
        next_run_at: Option<Timestamp>,
    ) -> Result<BackupSchedule, StoreError>;

    async fn get_backup_schedule(&self, id: BackupScheduleId) -> Result<BackupSchedule, StoreError>;

    async fn list_backup_schedules(&self, service_id: ServiceId) -> Result<Vec<BackupSchedule>, StoreError>;

    /// The schedules due at `now`, of services that are not deleted.
    async fn list_due_backup_schedules(&self, now: Timestamp) -> Result<Vec<BackupSchedule>, StoreError>;

    /// Every schedule of a service that is not deleted, for retention.
    async fn list_all_backup_schedules(&self) -> Result<Vec<BackupSchedule>, StoreError>;

    /// Replaces the fields if the schedule is still at `revision`. `NotFound` if the new
    /// destination is deleted.
    async fn update_backup_schedule(
        &self,
        id: BackupScheduleId,
        revision: i64,
        fields: BackupScheduleFields,
        next_run_at: Option<Timestamp>,
    ) -> Result<BackupSchedule, StoreError>;

    /// Moves the schedule's next run, without a new revision: the scheduler does it.
    async fn set_backup_schedule_next_run(&self, id: BackupScheduleId, next_run_at: Option<Timestamp>) -> Result<(), StoreError>;

    /// Removes the schedule; its backups stay, without one.
    async fn delete_backup_schedule(&self, id: BackupScheduleId) -> Result<(), StoreError>;
}

#[async_trait]
pub trait BackupRepository: Send + Sync {
    /// Records a queued backup. `NotFound` if the service is deleted.
    async fn create_backup(&self, new: NewBackup) -> Result<Backup, StoreError>;

    /// Fails with `NotFound` for deleted backups too.
    async fn get_backup(&self, id: BackupId) -> Result<Backup, StoreError>;

    /// The service's backups that are not deleted, newest first, starting after `after`.
    async fn list_backups(&self, service_id: ServiceId, after: Option<BackupId>, limit: u32) -> Result<Vec<Backup>, StoreError>;

    /// Every queued and running backup, oldest first.
    async fn list_active_backups(&self) -> Result<Vec<Backup>, StoreError>;

    /// The schedule's successful backups beyond its newest `keep`, which are not deleted and
    /// not being restored.
    async fn list_backups_beyond(&self, schedule_id: BackupScheduleId, keep: u32) -> Result<Vec<Backup>, StoreError>;

    /// Moves a queued backup to running on `server`. `false` if it was not queued.
    async fn start_backup(&self, id: BackupId, server: ServerId) -> Result<bool, StoreError>;

    /// Finishes a backup that has not finished: succeeded with its size, or failed with why.
    /// `false` if it had finished already.
    async fn finish_backup(&self, id: BackupId, outcome: Result<u64, String>) -> Result<bool, StoreError>;

    async fn mark_backup_deleted(&self, id: BackupId) -> Result<(), StoreError>;
}

#[derive(sqlx::FromRow)]
struct DestinationRow {
    id: String,
    name: String,
    endpoint: Option<String>,
    region: String,
    bucket: String,
    path_prefix: String,
    path_style: bool,
    access_key_id: String,
    revision: i64,
    created_at: String,
    updated_at: String,
}

impl TryFrom<DestinationRow> for StorageDestination {
    type Error = StoreError;

    fn try_from(row: DestinationRow) -> Result<Self, Self::Error> {
        Ok(StorageDestination {
            id: parse_column(DESTINATION, "id", &row.id)?,
            fields: StorageDestinationFields {
                name: row.name,
                endpoint: row.endpoint,
                region: row.region,
                bucket: row.bucket,
                path_prefix: row.path_prefix,
                path_style: row.path_style,
                access_key_id: row.access_key_id,
            },
            revision: row.revision,
            created_at: parse_column(DESTINATION, "created_at", &row.created_at)?,
            updated_at: parse_column(DESTINATION, "updated_at", &row.updated_at)?,
        })
    }
}

macro_rules! destination_columns {
    () => {
        "id, name, endpoint, region, bucket, path_prefix, path_style, access_key_id, revision, created_at, updated_at"
    };
}

#[derive(sqlx::FromRow)]
struct ScheduleRow {
    id: String,
    service_id: String,
    destination_id: String,
    cron: String,
    keep_last: i64,
    enabled: bool,
    next_run_at: Option<String>,
    revision: i64,
    created_at: String,
    updated_at: String,
}

impl TryFrom<ScheduleRow> for BackupSchedule {
    type Error = StoreError;

    fn try_from(row: ScheduleRow) -> Result<Self, Self::Error> {
        Ok(BackupSchedule {
            id: parse_column(SCHEDULE, "id", &row.id)?,
            service_id: parse_column(SCHEDULE, "service_id", &row.service_id)?,
            fields: BackupScheduleFields {
                destination_id: parse_column(SCHEDULE, "destination_id", &row.destination_id)?,
                cron: row.cron,
                keep_last: u32::try_from(row.keep_last).map_err(|error| StoreError::InvalidRow {
                    entity: SCHEDULE,
                    column: "keep_last",
                    problem: error.to_string(),
                })?,
                enabled: row.enabled,
            },
            next_run_at: row.next_run_at.map(|at| parse_column(SCHEDULE, "next_run_at", &at)).transpose()?,
            revision: row.revision,
            created_at: parse_column(SCHEDULE, "created_at", &row.created_at)?,
            updated_at: parse_column(SCHEDULE, "updated_at", &row.updated_at)?,
        })
    }
}

macro_rules! schedule_columns {
    () => {
        "backup_schedules.id, backup_schedules.service_id, backup_schedules.destination_id, backup_schedules.cron,
         backup_schedules.keep_last, backup_schedules.enabled, backup_schedules.next_run_at, backup_schedules.revision,
         backup_schedules.created_at, backup_schedules.updated_at"
    };
}

#[derive(sqlx::FromRow)]
struct BackupRow {
    id: String,
    service_id: String,
    destination_id: String,
    schedule_id: Option<String>,
    status: String,
    engine: String,
    object_key: String,
    server_id: Option<String>,
    size_bytes: Option<i64>,
    error: Option<String>,
    created_at: String,
    started_at: Option<String>,
    finished_at: Option<String>,
}

impl TryFrom<BackupRow> for Backup {
    type Error = StoreError;

    fn try_from(row: BackupRow) -> Result<Self, Self::Error> {
        let optional = |column, value: Option<String>| value.map(|value| parse_column(BACKUP, column, &value)).transpose();
        Ok(Backup {
            id: parse_column(BACKUP, "id", &row.id)?,
            service_id: parse_column(BACKUP, "service_id", &row.service_id)?,
            destination_id: parse_column(BACKUP, "destination_id", &row.destination_id)?,
            schedule_id: row.schedule_id.map(|id| parse_column(BACKUP, "schedule_id", &id)).transpose()?,
            status: parse_column(BACKUP, "status", &row.status)?,
            engine: parse_column(BACKUP, "engine", &row.engine)?,
            object_key: row.object_key,
            server_id: row.server_id.map(|id| parse_column(BACKUP, "server_id", &id)).transpose()?,
            size_bytes: row.size_bytes.map(|size| size.max(0) as u64),
            error: row.error,
            created_at: parse_column(BACKUP, "created_at", &row.created_at)?,
            started_at: optional("started_at", row.started_at)?,
            finished_at: optional("finished_at", row.finished_at)?,
        })
    }
}

macro_rules! backup_columns {
    () => {
        "id, service_id, destination_id, schedule_id, status, engine, object_key, server_id, size_bytes, error,
         created_at, started_at, finished_at"
    };
}

fn destination_not_found(id: StorageDestinationId) -> StoreError {
    StoreError::NotFound { entity: DESTINATION, id: id.to_string() }
}

fn name_taken(name: &str) -> impl FnOnce(&str) -> StoreError {
    let name = name.to_string();
    move |_| StoreError::Conflict { entity: DESTINATION, field: "name", value: name }
}

#[async_trait]
impl StorageDestinationRepository for SqliteStore {
    async fn create_storage_destination(
        &self,
        fields: StorageDestinationFields,
        secret_access_key: &Secret,
    ) -> Result<StorageDestination, StoreError> {
        let id = StorageDestinationId::new();
        let sealed = self.keyring().seal(secret_access_key.expose().as_bytes(), id.to_string().as_bytes());
        let row: DestinationRow = sqlx::query_as(concat!(
            "INSERT INTO storage_destinations
             (id, name, endpoint, region, bucket, path_prefix, path_style, access_key_id, secret_access_key, data_key_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             RETURNING ",
            destination_columns!()
        ))
        .bind(id.to_string())
        .bind(&fields.name)
        .bind(&fields.endpoint)
        .bind(&fields.region)
        .bind(&fields.bucket)
        .bind(&fields.path_prefix)
        .bind(fields.path_style)
        .bind(&fields.access_key_id)
        .bind(sealed.ciphertext)
        .bind(sealed.key_id.to_string())
        .fetch_one(self.pool())
        .await
        .map_err(|error| StoreError::on_unique_violation(error, name_taken(&fields.name)))?;
        row.try_into()
    }

    async fn get_storage_destination(&self, id: StorageDestinationId) -> Result<StorageDestination, StoreError> {
        let row: Option<DestinationRow> = sqlx::query_as(concat!(
            "SELECT ",
            destination_columns!(),
            " FROM storage_destinations WHERE id = ? AND deleted_at IS NULL"
        ))
        .bind(id.to_string())
        .fetch_optional(self.pool())
        .await?;
        row.ok_or_else(|| destination_not_found(id))?.try_into()
    }

    async fn get_storage_destination_any(&self, id: StorageDestinationId) -> Result<StorageDestination, StoreError> {
        let row: Option<DestinationRow> =
            sqlx::query_as(concat!("SELECT ", destination_columns!(), " FROM storage_destinations WHERE id = ?"))
                .bind(id.to_string())
                .fetch_optional(self.pool())
                .await?;
        row.ok_or_else(|| destination_not_found(id))?.try_into()
    }

    async fn list_storage_destinations(&self) -> Result<Vec<StorageDestination>, StoreError> {
        let rows: Vec<DestinationRow> = sqlx::query_as(concat!(
            "SELECT ",
            destination_columns!(),
            " FROM storage_destinations WHERE deleted_at IS NULL ORDER BY name"
        ))
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(StorageDestination::try_from).collect()
    }

    async fn update_storage_destination(
        &self,
        id: StorageDestinationId,
        revision: i64,
        fields: StorageDestinationFields,
        secret_access_key: Option<&Secret>,
    ) -> Result<StorageDestination, StoreError> {
        let sealed = secret_access_key.map(|secret| self.keyring().seal(secret.expose().as_bytes(), id.to_string().as_bytes()));
        let (ciphertext, key_id) = match sealed {
            Some(sealed) => (Some(sealed.ciphertext), Some(sealed.key_id.to_string())),
            None => (None, None),
        };
        let row: Option<DestinationRow> = sqlx::query_as(concat!(
            "UPDATE storage_destinations
             SET name = ?, endpoint = ?, region = ?, bucket = ?, path_prefix = ?, path_style = ?, access_key_id = ?,
                 secret_access_key = coalesce(?, secret_access_key), data_key_id = coalesce(?, data_key_id),
                 revision = revision + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ? AND revision = ? AND deleted_at IS NULL
             RETURNING ",
            destination_columns!()
        ))
        .bind(&fields.name)
        .bind(&fields.endpoint)
        .bind(&fields.region)
        .bind(&fields.bucket)
        .bind(&fields.path_prefix)
        .bind(fields.path_style)
        .bind(&fields.access_key_id)
        .bind(ciphertext)
        .bind(key_id)
        .bind(id.to_string())
        .bind(revision)
        .fetch_optional(self.pool())
        .await
        .map_err(|error| StoreError::on_unique_violation(error, name_taken(&fields.name)))?;
        match row {
            Some(row) => row.try_into(),
            None => {
                let current = self.get_storage_destination(id).await?;
                Err(StoreError::RevisionMismatch {
                    entity: DESTINATION,
                    id: id.to_string(),
                    expected: revision,
                    actual: current.revision,
                })
            }
        }
    }

    async fn delete_storage_destination(&self, id: StorageDestinationId) -> Result<(), StoreError> {
        let mut tx = self.begin_write().await?;
        let used: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM backup_schedules JOIN services ON services.id = backup_schedules.service_id
             WHERE backup_schedules.destination_id = ? AND services.deleted_at IS NULL LIMIT 1",
        )
        .bind(id.to_string())
        .fetch_optional(&mut *tx)
        .await?;
        if used.is_some() {
            return Err(StoreError::HasChildren { entity: DESTINATION, id: id.to_string(), children: "backup schedules" });
        }
        let deleted = sqlx::query("UPDATE storage_destinations SET deleted_at = ? WHERE id = ? AND deleted_at IS NULL")
            .bind(format_timestamp(Timestamp::now()))
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if deleted == 0 {
            return Err(destination_not_found(id));
        }
        tx.commit().await?;
        Ok(())
    }

    async fn storage_destination_secret(&self, id: StorageDestinationId) -> Result<Secret, StoreError> {
        open_sealed(
            self,
            "SELECT secret_access_key, data_key_id FROM storage_destinations WHERE id = ?",
            DESTINATION,
            id.to_string(),
        )
        .await
    }
}

#[async_trait]
impl BackupScheduleRepository for SqliteStore {
    async fn create_backup_schedule(
        &self,
        service_id: ServiceId,
        fields: BackupScheduleFields,
        next_run_at: Option<Timestamp>,
    ) -> Result<BackupSchedule, StoreError> {
        let mut tx = self.begin_write().await?;
        check_schedule_parents(&mut tx, service_id, fields.destination_id).await?;
        let row: ScheduleRow = sqlx::query_as(concat!(
            "INSERT INTO backup_schedules (id, service_id, destination_id, cron, keep_last, enabled, next_run_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             RETURNING ",
            schedule_columns!()
        ))
        .bind(BackupScheduleId::new().to_string())
        .bind(service_id.to_string())
        .bind(fields.destination_id.to_string())
        .bind(&fields.cron)
        .bind(i64::from(fields.keep_last))
        .bind(fields.enabled)
        .bind(next_run_at.map(format_timestamp))
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        row.try_into()
    }

    async fn get_backup_schedule(&self, id: BackupScheduleId) -> Result<BackupSchedule, StoreError> {
        let row: Option<ScheduleRow> = sqlx::query_as(concat!(
            "SELECT ",
            schedule_columns!(),
            " FROM backup_schedules JOIN services ON services.id = backup_schedules.service_id
             WHERE backup_schedules.id = ? AND services.deleted_at IS NULL"
        ))
        .bind(id.to_string())
        .fetch_optional(self.pool())
        .await?;
        row.ok_or_else(|| StoreError::NotFound { entity: SCHEDULE, id: id.to_string() })?.try_into()
    }

    async fn list_backup_schedules(&self, service_id: ServiceId) -> Result<Vec<BackupSchedule>, StoreError> {
        let rows: Vec<ScheduleRow> =
            sqlx::query_as(concat!("SELECT ", schedule_columns!(), " FROM backup_schedules WHERE service_id = ? ORDER BY id"))
                .bind(service_id.to_string())
                .fetch_all(self.pool())
                .await?;
        rows.into_iter().map(BackupSchedule::try_from).collect()
    }

    async fn list_due_backup_schedules(&self, now: Timestamp) -> Result<Vec<BackupSchedule>, StoreError> {
        let rows: Vec<ScheduleRow> = sqlx::query_as(concat!(
            "SELECT ",
            schedule_columns!(),
            " FROM backup_schedules JOIN services ON services.id = backup_schedules.service_id
             WHERE backup_schedules.next_run_at <= ? AND services.deleted_at IS NULL ORDER BY backup_schedules.id"
        ))
        .bind(format_timestamp(now))
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(BackupSchedule::try_from).collect()
    }

    async fn list_all_backup_schedules(&self) -> Result<Vec<BackupSchedule>, StoreError> {
        let rows: Vec<ScheduleRow> = sqlx::query_as(concat!(
            "SELECT ",
            schedule_columns!(),
            " FROM backup_schedules JOIN services ON services.id = backup_schedules.service_id
             WHERE services.deleted_at IS NULL ORDER BY backup_schedules.id"
        ))
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(BackupSchedule::try_from).collect()
    }

    async fn update_backup_schedule(
        &self,
        id: BackupScheduleId,
        revision: i64,
        fields: BackupScheduleFields,
        next_run_at: Option<Timestamp>,
    ) -> Result<BackupSchedule, StoreError> {
        let current = self.get_backup_schedule(id).await?;
        let mut tx = self.begin_write().await?;
        check_schedule_parents(&mut tx, current.service_id, fields.destination_id).await?;
        let row: Option<ScheduleRow> = sqlx::query_as(concat!(
            "UPDATE backup_schedules
             SET destination_id = ?, cron = ?, keep_last = ?, enabled = ?, next_run_at = ?, revision = revision + 1,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ? AND revision = ?
             RETURNING ",
            schedule_columns!()
        ))
        .bind(fields.destination_id.to_string())
        .bind(&fields.cron)
        .bind(i64::from(fields.keep_last))
        .bind(fields.enabled)
        .bind(next_run_at.map(format_timestamp))
        .bind(id.to_string())
        .bind(revision)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        match row {
            Some(row) => row.try_into(),
            None => Err(StoreError::RevisionMismatch {
                entity: SCHEDULE,
                id: id.to_string(),
                expected: revision,
                actual: current.revision,
            }),
        }
    }

    async fn set_backup_schedule_next_run(&self, id: BackupScheduleId, next_run_at: Option<Timestamp>) -> Result<(), StoreError> {
        sqlx::query("UPDATE backup_schedules SET next_run_at = ? WHERE id = ?")
            .bind(next_run_at.map(format_timestamp))
            .bind(id.to_string())
            .execute(self.pool())
            .await?;
        Ok(())
    }

    async fn delete_backup_schedule(&self, id: BackupScheduleId) -> Result<(), StoreError> {
        let deleted = sqlx::query("DELETE FROM backup_schedules WHERE id = ?")
            .bind(id.to_string())
            .execute(self.pool())
            .await?
            .rows_affected();
        match deleted {
            0 => Err(StoreError::NotFound { entity: SCHEDULE, id: id.to_string() }),
            _ => Ok(()),
        }
    }
}

/// The service and the destination of a schedule must exist and not be deleted.
async fn check_schedule_parents(
    tx: &mut sqlx::SqliteConnection,
    service_id: ServiceId,
    destination_id: StorageDestinationId,
) -> Result<(), StoreError> {
    let service: Option<i64> = sqlx::query_scalar("SELECT 1 FROM services WHERE id = ? AND deleted_at IS NULL")
        .bind(service_id.to_string())
        .fetch_optional(&mut *tx)
        .await?;
    if service.is_none() {
        return Err(StoreError::NotFound { entity: "service", id: service_id.to_string() });
    }
    let destination: Option<i64> = sqlx::query_scalar("SELECT 1 FROM storage_destinations WHERE id = ? AND deleted_at IS NULL")
        .bind(destination_id.to_string())
        .fetch_optional(&mut *tx)
        .await?;
    if destination.is_none() {
        return Err(destination_not_found(destination_id));
    }
    Ok(())
}

#[async_trait]
impl BackupRepository for SqliteStore {
    async fn create_backup(&self, new: NewBackup) -> Result<Backup, StoreError> {
        let row: Option<BackupRow> = sqlx::query_as(concat!(
            "INSERT INTO backups (id, service_id, destination_id, schedule_id, status, engine, object_key)
             SELECT ?, id, ?, ?, 'queued', ?, ? FROM services WHERE id = ? AND deleted_at IS NULL
             RETURNING ",
            backup_columns!()
        ))
        .bind(new.id.to_string())
        .bind(new.destination_id.to_string())
        .bind(new.schedule_id.map(|id| id.to_string()))
        .bind(new.engine.as_str())
        .bind(&new.object_key)
        .bind(new.service_id.to_string())
        .fetch_optional(self.pool())
        .await?;
        match row {
            Some(row) => row.try_into(),
            None => Err(StoreError::NotFound { entity: "service", id: new.service_id.to_string() }),
        }
    }

    async fn get_backup(&self, id: BackupId) -> Result<Backup, StoreError> {
        let row: Option<BackupRow> =
            sqlx::query_as(concat!("SELECT ", backup_columns!(), " FROM backups WHERE id = ? AND deleted_at IS NULL"))
                .bind(id.to_string())
                .fetch_optional(self.pool())
                .await?;
        row.ok_or_else(|| StoreError::NotFound { entity: BACKUP, id: id.to_string() })?.try_into()
    }

    async fn list_backups(&self, service_id: ServiceId, after: Option<BackupId>, limit: u32) -> Result<Vec<Backup>, StoreError> {
        let rows: Vec<BackupRow> = match after {
            Some(after) => {
                sqlx::query_as(concat!(
                    "SELECT ",
                    backup_columns!(),
                    " FROM backups WHERE service_id = ? AND deleted_at IS NULL AND id < ? ORDER BY id DESC LIMIT ?"
                ))
                .bind(service_id.to_string())
                .bind(after.to_string())
                .bind(limit)
                .fetch_all(self.pool())
                .await?
            }
            None => {
                sqlx::query_as(concat!(
                    "SELECT ",
                    backup_columns!(),
                    " FROM backups WHERE service_id = ? AND deleted_at IS NULL ORDER BY id DESC LIMIT ?"
                ))
                .bind(service_id.to_string())
                .bind(limit)
                .fetch_all(self.pool())
                .await?
            }
        };
        rows.into_iter().map(Backup::try_from).collect()
    }

    async fn list_active_backups(&self) -> Result<Vec<Backup>, StoreError> {
        let rows: Vec<BackupRow> = sqlx::query_as(concat!(
            "SELECT ",
            backup_columns!(),
            " FROM backups WHERE status IN ('queued', 'running') AND deleted_at IS NULL ORDER BY id"
        ))
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(Backup::try_from).collect()
    }

    async fn list_backups_beyond(&self, schedule_id: BackupScheduleId, keep: u32) -> Result<Vec<Backup>, StoreError> {
        let rows: Vec<BackupRow> = sqlx::query_as(concat!(
            "SELECT ",
            backup_columns!(),
            " FROM backups WHERE schedule_id = ? AND status = 'succeeded' AND deleted_at IS NULL
               AND NOT EXISTS (SELECT 1 FROM restores
                               WHERE restores.backup_id = backups.id AND restores.status IN ('queued', 'running'))
             ORDER BY id DESC LIMIT -1 OFFSET ?"
        ))
        .bind(schedule_id.to_string())
        .bind(keep)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(Backup::try_from).collect()
    }

    async fn start_backup(&self, id: BackupId, server: ServerId) -> Result<bool, StoreError> {
        let started = sqlx::query(
            "UPDATE backups SET status = 'running', server_id = ?, started_at = ? WHERE id = ? AND status = 'queued'",
        )
        .bind(server.to_string())
        .bind(format_timestamp(Timestamp::now()))
        .bind(id.to_string())
        .execute(self.pool())
        .await?
        .rows_affected();
        Ok(started > 0)
    }

    async fn finish_backup(&self, id: BackupId, outcome: Result<u64, String>) -> Result<bool, StoreError> {
        let (status, size, error) = match outcome {
            Ok(size) => (BackupStatus::Succeeded, Some(i64::try_from(size).unwrap_or(i64::MAX)), None),
            Err(error) => (BackupStatus::Failed, None, Some(error)),
        };
        let finished = sqlx::query(
            "UPDATE backups SET status = ?, size_bytes = ?, error = ?, finished_at = ?
             WHERE id = ? AND status IN ('queued', 'running')",
        )
        .bind(status.as_str())
        .bind(size)
        .bind(error)
        .bind(format_timestamp(Timestamp::now()))
        .bind(id.to_string())
        .execute(self.pool())
        .await?
        .rows_affected();
        Ok(finished > 0)
    }

    async fn mark_backup_deleted(&self, id: BackupId) -> Result<(), StoreError> {
        sqlx::query("UPDATE backups SET deleted_at = ? WHERE id = ? AND deleted_at IS NULL")
            .bind(format_timestamp(Timestamp::now()))
            .bind(id.to_string())
            .execute(self.pool())
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use spinikus_core::{
        backup::object_key,
        database::Engine,
        environment::{EnvironmentFields, EnvironmentKind},
        project::ProjectFields,
        server::ServerFields,
        service::{ServiceFields, ServiceKind},
    };

    use super::*;
    use crate::{EnvironmentRepository, ProjectRepository, ServerRepository, ServiceRepository, test_support::store};

    fn destination(name: &str) -> StorageDestinationFields {
        StorageDestinationFields {
            name: name.to_string(),
            endpoint: Some("http://127.0.0.1:9000".to_string()),
            region: "us-east-1".to_string(),
            bucket: "backups".to_string(),
            path_prefix: "spinikus/".to_string(),
            path_style: true,
            access_key_id: "AKIA".to_string(),
        }
    }

    async fn database(store: &SqliteStore) -> ServiceId {
        let project =
            store.create_project(ProjectFields { name: "Shop".to_string(), slug: "shop".parse().unwrap() }).await.unwrap();
        let fields = EnvironmentFields { name: "Production".to_string(), slug: "production".parse().unwrap(), previews: false };
        let environment = store.create_environment(project.id, EnvironmentKind::Production, fields).await.unwrap();
        let fields = ServiceFields {
            name: "Postgres".to_string(),
            slug: "postgres".parse().unwrap(),
            config: Default::default(),
            server_id: None,
        };
        store.create_service(environment.id, ServiceKind::Database, fields).await.unwrap().id
    }

    fn new_backup(service: ServiceId, destination: StorageDestinationId, schedule: Option<BackupScheduleId>) -> NewBackup {
        let id = BackupId::new();
        NewBackup {
            id,
            service_id: service,
            destination_id: destination,
            schedule_id: schedule,
            engine: Engine::Postgres,
            object_key: object_key("spinikus/", service, id, Engine::Postgres),
        }
    }

    #[tokio::test]
    async fn destinations_keep_their_secret_sealed_and_names_unique() {
        let (_dir, store) = store().await;

        let created = store.create_storage_destination(destination("S3"), &Secret::new("s3cret")).await.unwrap();
        let taken = store.create_storage_destination(destination("S3"), &Secret::new("x")).await;
        let mut renamed = destination("Archive");
        renamed.path_style = false;
        let updated = store.update_storage_destination(created.id, created.revision, renamed.clone(), None).await.unwrap();
        let stale = store.update_storage_destination(created.id, created.revision, renamed.clone(), None).await;
        let rotated =
            store.update_storage_destination(created.id, updated.revision, renamed, Some(&Secret::new("n3w"))).await.unwrap();
        let stored: Vec<u8> =
            sqlx::query_scalar("SELECT secret_access_key FROM storage_destinations").fetch_one(store.pool()).await.unwrap();

        assert!(matches!(taken, Err(StoreError::Conflict { field: "name", .. })), "{taken:?}");
        assert!(!updated.fields.path_style && updated.fields.name == "Archive");
        assert!(matches!(stale, Err(StoreError::RevisionMismatch { .. })), "{stale:?}");
        assert_eq!(rotated.revision, 3);
        assert_eq!(store.storage_destination_secret(created.id).await.unwrap().expose(), "n3w");
        assert!(!stored.windows(3).any(|window| window == b"n3w"));
        assert_eq!(store.list_storage_destinations().await.unwrap(), [rotated]);
    }

    #[tokio::test]
    async fn destinations_in_use_are_not_deleted() {
        let (_dir, store) = store().await;
        let service = database(&store).await;
        let used = store.create_storage_destination(destination("S3"), &Secret::new("s")).await.unwrap();
        let fields = BackupScheduleFields { destination_id: used.id, cron: "0 3 * * *".to_string(), keep_last: 7, enabled: true };
        let schedule = store.create_backup_schedule(service, fields, None).await.unwrap();

        let refused = store.delete_storage_destination(used.id).await;
        store.delete_backup_schedule(schedule.id).await.unwrap();
        store.delete_storage_destination(used.id).await.unwrap();
        let again = store.create_storage_destination(destination("S3"), &Secret::new("s")).await;
        let fields = BackupScheduleFields { destination_id: used.id, cron: "0 3 * * *".to_string(), keep_last: 7, enabled: true };
        let to_deleted = store.create_backup_schedule(service, fields, None).await;

        assert!(matches!(refused, Err(StoreError::HasChildren { .. })), "{refused:?}");
        assert!(again.is_ok());
        assert!(matches!(store.get_storage_destination(used.id).await, Err(StoreError::NotFound { .. })));
        assert_eq!(store.get_storage_destination_any(used.id).await.unwrap().id, used.id);
        assert!(matches!(to_deleted, Err(StoreError::NotFound { entity: DESTINATION, .. })), "{to_deleted:?}");
    }

    #[tokio::test]
    async fn schedules_come_due_and_move_on() {
        let (_dir, store) = store().await;
        let service = database(&store).await;
        let destination = store.create_storage_destination(destination("S3"), &Secret::new("s")).await.unwrap();
        let fields = |enabled| BackupScheduleFields {
            destination_id: destination.id,
            cron: "0 3 * * *".to_string(),
            keep_last: 2,
            enabled,
        };
        let now = Timestamp::now();
        let due =
            store.create_backup_schedule(service, fields(true), Some(now - jiff::SignedDuration::from_secs(1))).await.unwrap();
        store.create_backup_schedule(service, fields(true), Some(now + jiff::SignedDuration::from_hours(1))).await.unwrap();
        store.create_backup_schedule(service, fields(false), None).await.unwrap();

        let listed = store.list_due_backup_schedules(now).await.unwrap();
        store.set_backup_schedule_next_run(due.id, Some(now + jiff::SignedDuration::from_hours(24))).await.unwrap();
        let after = store.list_due_backup_schedules(now).await.unwrap();
        let disabled = store.update_backup_schedule(due.id, due.revision, fields(false), None).await.unwrap();

        assert_eq!(listed.iter().map(|schedule| schedule.id).collect::<Vec<_>>(), [due.id]);
        assert!(after.is_empty());
        assert_eq!((disabled.fields.enabled, disabled.next_run_at, disabled.revision), (false, None, 2));
        assert_eq!(store.list_backup_schedules(service).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn backups_run_once_and_retention_finds_the_old_ones() {
        let (_dir, store) = store().await;
        let service = database(&store).await;
        let server = store.create_server(ServerFields { name: "db-1".to_string() }).await.unwrap().id;
        let destination = store.create_storage_destination(destination("S3"), &Secret::new("s")).await.unwrap();
        let fields =
            BackupScheduleFields { destination_id: destination.id, cron: "0 3 * * *".to_string(), keep_last: 2, enabled: true };
        let schedule = store.create_backup_schedule(service, fields, None).await.unwrap();
        let mut made = Vec::new();
        for _ in 0..4 {
            let backup = store.create_backup(new_backup(service, destination.id, Some(schedule.id))).await.unwrap();
            assert!(store.start_backup(backup.id, server).await.unwrap());
            assert!(store.finish_backup(backup.id, Ok(1024)).await.unwrap());
            made.push(backup.id);
        }
        let manual = store.create_backup(new_backup(service, destination.id, None)).await.unwrap();

        let started_twice = store.start_backup(made[0], server).await.unwrap();
        let finished_twice = store.finish_backup(made[0], Err("late".to_string())).await.unwrap();
        let active = store.list_active_backups().await.unwrap();
        let beyond = store.list_backups_beyond(schedule.id, 2).await.unwrap();
        store.mark_backup_deleted(made[0]).await.unwrap();
        let listed = store.list_backups(service, None, 10).await.unwrap();
        let page = store.list_backups(service, Some(listed[1].id), 10).await.unwrap();
        let first = store.get_backup(made[1]).await.unwrap();

        assert!(!started_twice && !finished_twice);
        assert_eq!(active.iter().map(|backup| backup.id).collect::<Vec<_>>(), [manual.id]);
        assert_eq!(beyond.iter().map(|backup| backup.id).collect::<Vec<_>>(), [made[1], made[0]]);
        assert_eq!(listed.len(), 4);
        assert_eq!(listed[0].id, manual.id);
        assert_eq!(page.iter().map(|backup| backup.id).collect::<Vec<_>>(), [made[2], made[1]]);
        assert_eq!((first.status, first.size_bytes, first.server_id), (BackupStatus::Succeeded, Some(1024), Some(server)));
        assert!(first.started_at.is_some() && first.finished_at.is_some());
        assert!(matches!(store.get_backup(made[0]).await, Err(StoreError::NotFound { .. })));
    }
}
