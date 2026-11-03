//! The instance's backup schedule and its backups.

use async_trait::async_trait;
use jiff::Timestamp;
use spinikus_core::{
    backup::BackupStatus,
    id::InstanceBackupId,
    instance_backup::{InstanceBackup, InstanceBackupSchedule, InstanceBackupScheduleFields, NewInstanceBackup},
    secret::Secret,
};

use crate::{SqliteStore, StoreError, db::format_timestamp, error::parse_column};

const SCHEDULE: &str = "instance backup schedule";
const BACKUP: &str = "instance backup";

/// What the schedule's passphrase is sealed with, as context: there is one schedule.
const PASSPHRASE_CONTEXT: &str = "instance-backup-schedule";

#[async_trait]
pub trait InstanceBackupRepository: Send + Sync {
    /// The schedule, if one was ever set.
    async fn get_instance_backup_schedule(&self) -> Result<Option<InstanceBackupSchedule>, StoreError>;

    /// Sets the schedule if it is still at `revision` (0 for one never set). `passphrase` is
    /// required for a new schedule; `None` keeps the current one. `NotFound` if the
    /// destination does not exist or is deleted.
    async fn put_instance_backup_schedule(
        &self,
        revision: i64,
        fields: InstanceBackupScheduleFields,
        passphrase: Option<&Secret>,
        next_run_at: Option<Timestamp>,
    ) -> Result<InstanceBackupSchedule, StoreError>;

    /// Moves the next run on, without a revision: the runner does it.
    async fn set_instance_backup_next_run(&self, next_run_at: Option<Timestamp>) -> Result<(), StoreError>;

    async fn instance_backup_passphrase(&self) -> Result<Secret, StoreError>;

    async fn create_instance_backup(&self, new: NewInstanceBackup) -> Result<InstanceBackup, StoreError>;

    async fn get_instance_backup(&self, id: InstanceBackupId) -> Result<InstanceBackup, StoreError>;

    /// Newest first, after the cursor.
    async fn list_instance_backups(&self, after: Option<InstanceBackupId>, limit: u32)
    -> Result<Vec<InstanceBackup>, StoreError>;

    /// Queued and running backups, oldest first.
    async fn list_active_instance_backups(&self) -> Result<Vec<InstanceBackup>, StoreError>;

    /// The schedule's successful backups beyond the newest `keep`, which retention deletes.
    async fn list_instance_backups_beyond(&self, keep: u32) -> Result<Vec<InstanceBackup>, StoreError>;

    /// Marks a queued backup running. False if it wasn't queued.
    async fn start_instance_backup(&self, id: InstanceBackupId) -> Result<bool, StoreError>;

    /// Records how an unfinished backup ended: its size, or why it failed. False if it had
    /// already finished.
    async fn finish_instance_backup(&self, id: InstanceBackupId, outcome: Result<u64, String>) -> Result<bool, StoreError>;

    async fn mark_instance_backup_deleted(&self, id: InstanceBackupId) -> Result<(), StoreError>;
}

#[derive(sqlx::FromRow)]
struct ScheduleRow {
    destination_id: String,
    cron: String,
    keep_last: i64,
    enabled: bool,
    next_run_at: Option<String>,
    revision: i64,
    updated_at: String,
}

impl TryFrom<ScheduleRow> for InstanceBackupSchedule {
    type Error = StoreError;

    fn try_from(row: ScheduleRow) -> Result<Self, Self::Error> {
        Ok(InstanceBackupSchedule {
            fields: InstanceBackupScheduleFields {
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
            updated_at: parse_column(SCHEDULE, "updated_at", &row.updated_at)?,
        })
    }
}

macro_rules! schedule_columns {
    () => {
        "destination_id, cron, keep_last, enabled, next_run_at, revision, updated_at"
    };
}

#[derive(sqlx::FromRow)]
struct BackupRow {
    id: String,
    destination_id: String,
    scheduled: bool,
    status: String,
    object_key: String,
    size_bytes: Option<i64>,
    error: Option<String>,
    created_at: String,
    started_at: Option<String>,
    finished_at: Option<String>,
}

impl TryFrom<BackupRow> for InstanceBackup {
    type Error = StoreError;

    fn try_from(row: BackupRow) -> Result<Self, Self::Error> {
        let optional = |column, value: Option<String>| value.map(|value| parse_column(BACKUP, column, &value)).transpose();
        Ok(InstanceBackup {
            id: parse_column(BACKUP, "id", &row.id)?,
            destination_id: parse_column(BACKUP, "destination_id", &row.destination_id)?,
            scheduled: row.scheduled,
            status: parse_column(BACKUP, "status", &row.status)?,
            object_key: row.object_key,
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
        "id, destination_id, scheduled, status, object_key, size_bytes, error, created_at, started_at, finished_at"
    };
}

#[async_trait]
impl InstanceBackupRepository for SqliteStore {
    async fn get_instance_backup_schedule(&self) -> Result<Option<InstanceBackupSchedule>, StoreError> {
        let row: Option<ScheduleRow> =
            sqlx::query_as(concat!("SELECT ", schedule_columns!(), " FROM instance_backup_schedule WHERE id = 1"))
                .fetch_optional(self.pool())
                .await?;
        row.map(InstanceBackupSchedule::try_from).transpose()
    }

    async fn put_instance_backup_schedule(
        &self,
        revision: i64,
        fields: InstanceBackupScheduleFields,
        passphrase: Option<&Secret>,
        next_run_at: Option<Timestamp>,
    ) -> Result<InstanceBackupSchedule, StoreError> {
        let mut tx = self.begin_write().await?;
        let destination: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM storage_destinations WHERE id = ? AND deleted_at IS NULL")
                .bind(fields.destination_id.to_string())
                .fetch_optional(&mut *tx)
                .await?;
        if destination.is_none() {
            return Err(StoreError::NotFound { entity: "storage destination", id: fields.destination_id.to_string() });
        }
        let current: Option<i64> =
            sqlx::query_scalar("SELECT revision FROM instance_backup_schedule WHERE id = 1").fetch_optional(&mut *tx).await?;
        let mismatch =
            |actual| StoreError::RevisionMismatch { entity: SCHEDULE, id: "1".to_string(), expected: revision, actual };
        let sealed =
            passphrase.map(|passphrase| self.keyring().seal(passphrase.expose().as_bytes(), PASSPHRASE_CONTEXT.as_bytes()));
        let (ciphertext, key_id) = match sealed {
            Some(sealed) => (Some(sealed.ciphertext), Some(sealed.key_id.to_string())),
            None => (None, None),
        };
        let row: ScheduleRow = match current {
            None if revision != 0 => return Err(mismatch(0)),
            None => {
                let (Some(ciphertext), Some(key_id)) = (ciphertext, key_id) else {
                    return Err(StoreError::Rejected {
                        entity: SCHEDULE,
                        id: "1".to_string(),
                        problem: "a new schedule needs a passphrase",
                    });
                };
                sqlx::query_as(concat!(
                    "INSERT INTO instance_backup_schedule
                       (id, destination_id, cron, keep_last, enabled, passphrase, data_key_id, next_run_at)
                     VALUES (1, ?, ?, ?, ?, ?, ?, ?)
                     RETURNING ",
                    schedule_columns!()
                ))
                .bind(fields.destination_id.to_string())
                .bind(&fields.cron)
                .bind(i64::from(fields.keep_last))
                .bind(fields.enabled)
                .bind(ciphertext)
                .bind(key_id)
                .bind(next_run_at.map(format_timestamp))
                .fetch_one(&mut *tx)
                .await?
            }
            Some(actual) if actual != revision => return Err(mismatch(actual)),
            Some(_) => {
                sqlx::query_as(concat!(
                    "UPDATE instance_backup_schedule
                     SET destination_id = ?, cron = ?, keep_last = ?, enabled = ?,
                         passphrase = COALESCE(?, passphrase), data_key_id = COALESCE(?, data_key_id),
                         next_run_at = ?, revision = revision + 1, updated_at = ?
                     WHERE id = 1
                     RETURNING ",
                    schedule_columns!()
                ))
                .bind(fields.destination_id.to_string())
                .bind(&fields.cron)
                .bind(i64::from(fields.keep_last))
                .bind(fields.enabled)
                .bind(ciphertext)
                .bind(key_id)
                .bind(next_run_at.map(format_timestamp))
                .bind(format_timestamp(Timestamp::now()))
                .fetch_one(&mut *tx)
                .await?
            }
        };
        tx.commit().await?;
        row.try_into()
    }

    async fn set_instance_backup_next_run(&self, next_run_at: Option<Timestamp>) -> Result<(), StoreError> {
        sqlx::query("UPDATE instance_backup_schedule SET next_run_at = ? WHERE id = 1")
            .bind(next_run_at.map(format_timestamp))
            .execute(self.pool())
            .await?;
        Ok(())
    }

    async fn instance_backup_passphrase(&self) -> Result<Secret, StoreError> {
        // The context is fixed rather than the row's ID, which is always 1.
        let row: Option<(Vec<u8>, String)> =
            sqlx::query_as("SELECT passphrase, data_key_id FROM instance_backup_schedule WHERE id = 1")
                .fetch_optional(self.pool())
                .await?;
        let (ciphertext, key_id) = row.ok_or_else(|| StoreError::NotFound { entity: SCHEDULE, id: "1".to_string() })?;
        let sealed = crate::Sealed { key_id: parse_column(SCHEDULE, "data_key_id", &key_id)?, ciphertext };
        let plaintext = self.keyring().open(&sealed, PASSPHRASE_CONTEXT.as_bytes())?;
        let text = String::from_utf8(plaintext.to_vec()).map_err(|_| StoreError::InvalidRow {
            entity: SCHEDULE,
            column: "passphrase",
            problem: "not UTF-8".to_string(),
        })?;
        Ok(Secret::new(text))
    }

    async fn create_instance_backup(&self, new: NewInstanceBackup) -> Result<InstanceBackup, StoreError> {
        let row: BackupRow = sqlx::query_as(concat!(
            "INSERT INTO instance_backups (id, destination_id, scheduled, status, object_key)
             VALUES (?, ?, ?, 'queued', ?)
             RETURNING ",
            backup_columns!()
        ))
        .bind(new.id.to_string())
        .bind(new.destination_id.to_string())
        .bind(new.scheduled)
        .bind(&new.object_key)
        .fetch_one(self.pool())
        .await?;
        row.try_into()
    }

    async fn get_instance_backup(&self, id: InstanceBackupId) -> Result<InstanceBackup, StoreError> {
        let row: Option<BackupRow> =
            sqlx::query_as(concat!("SELECT ", backup_columns!(), " FROM instance_backups WHERE id = ? AND deleted_at IS NULL"))
                .bind(id.to_string())
                .fetch_optional(self.pool())
                .await?;
        row.ok_or_else(|| StoreError::NotFound { entity: BACKUP, id: id.to_string() })?.try_into()
    }

    async fn list_instance_backups(
        &self,
        after: Option<InstanceBackupId>,
        limit: u32,
    ) -> Result<Vec<InstanceBackup>, StoreError> {
        let rows: Vec<BackupRow> = sqlx::query_as(concat!(
            "SELECT ",
            backup_columns!(),
            " FROM instance_backups WHERE deleted_at IS NULL AND (? IS NULL OR id < ?) ORDER BY id DESC LIMIT ?"
        ))
        .bind(after.map(|id| id.to_string()))
        .bind(after.map(|id| id.to_string()))
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(InstanceBackup::try_from).collect()
    }

    async fn list_active_instance_backups(&self) -> Result<Vec<InstanceBackup>, StoreError> {
        let rows: Vec<BackupRow> = sqlx::query_as(concat!(
            "SELECT ",
            backup_columns!(),
            " FROM instance_backups WHERE status IN ('queued', 'running') AND deleted_at IS NULL ORDER BY id"
        ))
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(InstanceBackup::try_from).collect()
    }

    async fn list_instance_backups_beyond(&self, keep: u32) -> Result<Vec<InstanceBackup>, StoreError> {
        let rows: Vec<BackupRow> = sqlx::query_as(concat!(
            "SELECT ",
            backup_columns!(),
            " FROM instance_backups WHERE scheduled = 1 AND status = 'succeeded' AND deleted_at IS NULL
             ORDER BY id DESC LIMIT -1 OFFSET ?"
        ))
        .bind(keep)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(InstanceBackup::try_from).collect()
    }

    async fn start_instance_backup(&self, id: InstanceBackupId) -> Result<bool, StoreError> {
        let started =
            sqlx::query("UPDATE instance_backups SET status = 'running', started_at = ? WHERE id = ? AND status = 'queued'")
                .bind(format_timestamp(Timestamp::now()))
                .bind(id.to_string())
                .execute(self.pool())
                .await?
                .rows_affected();
        Ok(started > 0)
    }

    async fn finish_instance_backup(&self, id: InstanceBackupId, outcome: Result<u64, String>) -> Result<bool, StoreError> {
        let (status, size, error) = match outcome {
            Ok(size) => (BackupStatus::Succeeded, Some(i64::try_from(size).unwrap_or(i64::MAX)), None),
            Err(error) => (BackupStatus::Failed, None, Some(error)),
        };
        let finished = sqlx::query(
            "UPDATE instance_backups SET status = ?, size_bytes = ?, error = ?, finished_at = ?
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

    async fn mark_instance_backup_deleted(&self, id: InstanceBackupId) -> Result<(), StoreError> {
        sqlx::query("UPDATE instance_backups SET deleted_at = ? WHERE id = ? AND deleted_at IS NULL")
            .bind(format_timestamp(Timestamp::now()))
            .bind(id.to_string())
            .execute(self.pool())
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use spinikus_core::{backup::StorageDestinationFields, id::StorageDestinationId, instance_backup::object_key};

    use super::*;
    use crate::{StorageDestinationRepository, test_support::store};

    async fn destination(store: &SqliteStore) -> StorageDestinationId {
        let fields = StorageDestinationFields {
            name: "Backups".to_string(),
            endpoint: None,
            region: "us-east-1".to_string(),
            bucket: "backups".to_string(),
            path_prefix: "spinikus/".to_string(),
            path_style: false,
            access_key_id: "AKIA".to_string(),
        };
        store.create_storage_destination(fields, &Secret::new("secret")).await.unwrap().id
    }

    fn fields(destination_id: StorageDestinationId, keep_last: u32) -> InstanceBackupScheduleFields {
        InstanceBackupScheduleFields { destination_id, cron: "0 3 * * *".to_string(), keep_last, enabled: true }
    }

    #[tokio::test]
    async fn sets_the_schedule_with_a_sealed_passphrase() {
        let (_dir, store) = store().await;
        let destination = destination(&store).await;
        assert!(store.get_instance_backup_schedule().await.unwrap().is_none());

        let refused = store.put_instance_backup_schedule(0, fields(destination, 7), None, None).await;
        assert!(matches!(refused, Err(StoreError::Rejected { .. })), "a new one needs a passphrase");
        let stale = store.put_instance_backup_schedule(3, fields(destination, 7), Some(&Secret::new("p1")), None).await;
        assert!(matches!(stale, Err(StoreError::RevisionMismatch { actual: 0, .. })));

        let created =
            store.put_instance_backup_schedule(0, fields(destination, 7), Some(&Secret::new("p1")), None).await.unwrap();
        assert_eq!((created.revision, created.fields.keep_last), (1, 7));
        assert_eq!(store.instance_backup_passphrase().await.unwrap().expose(), "p1");
        let again = store.put_instance_backup_schedule(0, fields(destination, 7), Some(&Secret::new("p2")), None).await;
        assert!(matches!(again, Err(StoreError::RevisionMismatch { actual: 1, .. })));

        // Without a passphrase it keeps the one it has.
        let next = Timestamp::now();
        let updated = store.put_instance_backup_schedule(1, fields(destination, 3), None, Some(next)).await.unwrap();
        assert_eq!((updated.revision, updated.fields.keep_last), (2, 3));
        assert_eq!(store.instance_backup_passphrase().await.unwrap().expose(), "p1");
        store.put_instance_backup_schedule(2, fields(destination, 3), Some(&Secret::new("p2")), None).await.unwrap();
        assert_eq!(store.instance_backup_passphrase().await.unwrap().expose(), "p2");

        // Its destination can't be deleted, and a deleted one can't be chosen.
        let refused = store.delete_storage_destination(destination).await;
        assert!(matches!(refused, Err(StoreError::HasChildren { .. })), "{refused:?}");
        let other = {
            let mut fields = StorageDestinationFields {
                name: "Other".to_string(),
                ..store.get_storage_destination(destination).await.unwrap().fields
            };
            fields.bucket = "other".to_string();
            store.create_storage_destination(fields, &Secret::new("secret")).await.unwrap().id
        };
        store.delete_storage_destination(other).await.unwrap();
        let gone = store.put_instance_backup_schedule(3, fields(other, 3), None, None).await;
        assert!(matches!(gone, Err(StoreError::NotFound { .. })), "{gone:?}");
    }

    #[tokio::test]
    async fn records_backups_and_finds_those_beyond_retention() {
        let (_dir, store) = store().await;
        let destination = destination(&store).await;
        let mut ids = Vec::new();
        for scheduled in [true, true, false, true] {
            let id = InstanceBackupId::new();
            let new = NewInstanceBackup { id, destination_id: destination, scheduled, object_key: object_key("spinikus/", id) };
            let backup = store.create_instance_backup(new).await.unwrap();
            assert_eq!(backup.status, BackupStatus::Queued);
            ids.push(id);
        }
        assert_eq!(store.list_active_instance_backups().await.unwrap().len(), 4);

        for id in &ids {
            assert!(store.start_instance_backup(*id).await.unwrap());
            assert!(!store.start_instance_backup(*id).await.unwrap(), "only once");
            assert!(store.finish_instance_backup(*id, Ok(42)).await.unwrap());
        }
        assert!(!store.finish_instance_backup(ids[0], Err("late".to_string())).await.unwrap(), "already finished");
        assert!(store.list_active_instance_backups().await.unwrap().is_empty());
        let backup = store.get_instance_backup(ids[0]).await.unwrap();
        assert_eq!((backup.status, backup.size_bytes), (BackupStatus::Succeeded, Some(42)));

        // The schedule keeps two of its own: the oldest scheduled one goes, the manual one stays.
        let beyond: Vec<_> = store.list_instance_backups_beyond(2).await.unwrap().into_iter().map(|backup| backup.id).collect();
        assert_eq!(beyond, [ids[0]]);

        store.mark_instance_backup_deleted(ids[0]).await.unwrap();
        assert!(matches!(store.get_instance_backup(ids[0]).await, Err(StoreError::NotFound { .. })));
        let listed: Vec<_> = store.list_instance_backups(None, 10).await.unwrap().into_iter().map(|backup| backup.id).collect();
        assert_eq!(listed, [ids[3], ids[2], ids[1]]);
        let page: Vec<_> =
            store.list_instance_backups(Some(ids[3]), 1).await.unwrap().into_iter().map(|backup| backup.id).collect();
        assert_eq!(page, [ids[2]]);
    }
}
