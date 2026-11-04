//! Restores of backups into their databases.

use async_trait::async_trait;
use jiff::Timestamp;
use spinikus_core::{
    backup::{BackupStatus, NewRestore, Restore},
    id::{RestoreId, ServerId, ServiceId},
};

use crate::{SqliteStore, StoreError, db::format_timestamp, error::parse_column};

const RESTORE: &str = "restore";

#[async_trait]
pub trait RestoreRepository: Send + Sync {
    /// Records a queued restore. `NotFound` if the service is deleted, `Conflict` if the
    /// service already has a restore that has not finished.
    async fn create_restore(&self, new: NewRestore) -> Result<Restore, StoreError>;

    async fn get_restore(&self, id: RestoreId) -> Result<Restore, StoreError>;

    /// The service's restores, newest first, starting after `after`.
    async fn list_restores(
        &self,
        service_id: ServiceId,
        after: Option<RestoreId>,
        limit: u32,
    ) -> Result<Vec<Restore>, StoreError>;

    /// Every queued and running restore, oldest first.
    async fn list_active_restores(&self) -> Result<Vec<Restore>, StoreError>;

    /// Moves a queued restore to running on `server`. `false` if it was not queued.
    async fn start_restore(&self, id: RestoreId, server: ServerId) -> Result<bool, StoreError>;

    /// Finishes a restore that has not finished, with why it failed if it did. `false` if it
    /// had finished already.
    async fn finish_restore(&self, id: RestoreId, outcome: Result<(), String>) -> Result<bool, StoreError>;
}

#[derive(sqlx::FromRow)]
struct RestoreRow {
    id: String,
    service_id: String,
    backup_id: String,
    safety_backup_id: Option<String>,
    status: String,
    server_id: Option<String>,
    error: Option<String>,
    created_at: String,
    started_at: Option<String>,
    finished_at: Option<String>,
}

impl TryFrom<RestoreRow> for Restore {
    type Error = StoreError;

    fn try_from(row: RestoreRow) -> Result<Self, Self::Error> {
        Ok(Restore {
            id: parse_column(RESTORE, "id", &row.id)?,
            service_id: parse_column(RESTORE, "service_id", &row.service_id)?,
            backup_id: parse_column(RESTORE, "backup_id", &row.backup_id)?,
            safety_backup_id: optional("safety_backup_id", row.safety_backup_id)?,
            status: parse_column(RESTORE, "status", &row.status)?,
            server_id: optional("server_id", row.server_id)?,
            error: row.error,
            created_at: parse_column(RESTORE, "created_at", &row.created_at)?,
            started_at: optional("started_at", row.started_at)?,
            finished_at: optional("finished_at", row.finished_at)?,
        })
    }
}

fn optional<T>(column: &'static str, value: Option<String>) -> Result<Option<T>, StoreError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value.map(|value| parse_column(RESTORE, column, &value)).transpose()
}

macro_rules! columns {
    () => {
        "id, service_id, backup_id, safety_backup_id, status, server_id, error, created_at, started_at, finished_at"
    };
}

#[async_trait]
impl RestoreRepository for SqliteStore {
    async fn create_restore(&self, new: NewRestore) -> Result<Restore, StoreError> {
        let row: Option<RestoreRow> = sqlx::query_as(concat!(
            "INSERT INTO restores (id, service_id, backup_id, safety_backup_id, status)
             SELECT ?, id, ?, ?, 'queued' FROM services WHERE id = ? AND deleted_at IS NULL
             RETURNING ",
            columns!()
        ))
        .bind(RestoreId::new().to_string())
        .bind(new.backup_id.to_string())
        .bind(new.safety_backup_id.map(|id| id.to_string()))
        .bind(new.service_id.to_string())
        .fetch_optional(self.pool())
        .await
        .map_err(|error| {
            StoreError::on_unique_violation(error, |_| StoreError::Conflict {
                entity: RESTORE,
                field: "service_id",
                value: new.service_id.to_string(),
            })
        })?;
        match row {
            Some(row) => row.try_into(),
            None => Err(StoreError::NotFound { entity: "service", id: new.service_id.to_string() }),
        }
    }

    async fn get_restore(&self, id: RestoreId) -> Result<Restore, StoreError> {
        let row: Option<RestoreRow> = sqlx::query_as(concat!("SELECT ", columns!(), " FROM restores WHERE id = ?"))
            .bind(id.to_string())
            .fetch_optional(self.pool())
            .await?;
        row.ok_or_else(|| StoreError::NotFound { entity: RESTORE, id: id.to_string() })?.try_into()
    }

    async fn list_restores(
        &self,
        service_id: ServiceId,
        after: Option<RestoreId>,
        limit: u32,
    ) -> Result<Vec<Restore>, StoreError> {
        let rows: Vec<RestoreRow> = match after {
            Some(after) => {
                sqlx::query_as(concat!(
                    "SELECT ",
                    columns!(),
                    " FROM restores WHERE service_id = ? AND id < ? ORDER BY id DESC LIMIT ?"
                ))
                .bind(service_id.to_string())
                .bind(after.to_string())
                .bind(limit)
                .fetch_all(self.pool())
                .await?
            }
            None => {
                sqlx::query_as(concat!("SELECT ", columns!(), " FROM restores WHERE service_id = ? ORDER BY id DESC LIMIT ?"))
                    .bind(service_id.to_string())
                    .bind(limit)
                    .fetch_all(self.pool())
                    .await?
            }
        };
        rows.into_iter().map(Restore::try_from).collect()
    }

    async fn list_active_restores(&self) -> Result<Vec<Restore>, StoreError> {
        let rows: Vec<RestoreRow> =
            sqlx::query_as(concat!("SELECT ", columns!(), " FROM restores WHERE status IN ('queued', 'running') ORDER BY id"))
                .fetch_all(self.pool())
                .await?;
        rows.into_iter().map(Restore::try_from).collect()
    }

    async fn start_restore(&self, id: RestoreId, server: ServerId) -> Result<bool, StoreError> {
        let started = sqlx::query(
            "UPDATE restores SET status = 'running', server_id = ?, started_at = ? WHERE id = ? AND status = 'queued'",
        )
        .bind(server.to_string())
        .bind(format_timestamp(Timestamp::now()))
        .bind(id.to_string())
        .execute(self.pool())
        .await?
        .rows_affected();
        Ok(started > 0)
    }

    async fn finish_restore(&self, id: RestoreId, outcome: Result<(), String>) -> Result<bool, StoreError> {
        let (status, error) = match outcome {
            Ok(()) => (BackupStatus::Succeeded, None),
            Err(error) => (BackupStatus::Failed, Some(error)),
        };
        let finished = sqlx::query(
            "UPDATE restores SET status = ?, error = ?, finished_at = ? WHERE id = ? AND status IN ('queued', 'running')",
        )
        .bind(status.as_str())
        .bind(error)
        .bind(format_timestamp(Timestamp::now()))
        .bind(id.to_string())
        .execute(self.pool())
        .await?
        .rows_affected();
        Ok(finished > 0)
    }
}

#[cfg(test)]
mod tests {
    use spinikus_core::{
        backup::{BackupScheduleFields, BackupSource, NewBackup, StorageDestinationFields, object_key},
        database::Engine,
        environment::{EnvironmentFields, EnvironmentKind},
        id::{BackupId, BackupScheduleId, StorageDestinationId},
        project::ProjectFields,
        secret::Secret,
        server::ServerFields,
        service::{ServiceFields, ServiceKind},
    };

    use super::*;
    use crate::{
        BackupRepository, BackupScheduleRepository, EnvironmentRepository, ProjectRepository, ServerRepository,
        ServiceRepository, StorageDestinationRepository, test_support::store,
    };

    async fn database(store: &SqliteStore) -> (ServiceId, StorageDestinationId) {
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
        let service = store.create_service(environment.id, ServiceKind::Database, fields).await.unwrap().id;
        let destination = StorageDestinationFields {
            name: "S3".to_string(),
            endpoint: None,
            region: "us-east-1".to_string(),
            bucket: "backups".to_string(),
            path_prefix: String::new(),
            path_style: false,
            access_key_id: "AKIA".to_string(),
        };
        let destination = store.create_storage_destination(destination, &Secret::new("s")).await.unwrap().id;
        (service, destination)
    }

    async fn succeeded_backup(
        store: &SqliteStore,
        service: ServiceId,
        destination: StorageDestinationId,
        schedule: Option<BackupScheduleId>,
    ) -> BackupId {
        let id = BackupId::new();
        let new = NewBackup {
            id,
            service_id: service,
            destination_id: destination,
            schedule_id: schedule,
            source: BackupSource::Database(Engine::Postgres),
            object_key: object_key("", service, id, BackupSource::Database(Engine::Postgres)),
        };
        store.create_backup(new).await.unwrap();
        store.finish_backup(id, Ok(10)).await.unwrap();
        id
    }

    #[tokio::test]
    async fn one_restore_at_a_time_per_database() {
        let (_dir, store) = store().await;
        let (service, destination) = database(&store).await;
        let server = store.create_server(ServerFields { name: "db-1".to_string() }).await.unwrap().id;
        let backup = succeeded_backup(&store, service, destination, None).await;
        let new = NewRestore { service_id: service, backup_id: backup, safety_backup_id: None };

        let first = store.create_restore(new.clone()).await.unwrap();
        let second = store.create_restore(new.clone()).await;
        let started = store.start_restore(first.id, server).await.unwrap();
        let active = store.list_active_restores().await.unwrap();
        let finished = store.finish_restore(first.id, Err("pg_restore: error".to_string())).await.unwrap();
        let again = store.finish_restore(first.id, Ok(())).await.unwrap();
        let after = store.create_restore(new).await.unwrap();
        let listed = store.list_restores(service, None, 10).await.unwrap();
        let page = store.list_restores(service, Some(after.id), 10).await.unwrap();
        let failed = store.get_restore(first.id).await.unwrap();

        assert!(matches!(second, Err(StoreError::Conflict { .. })), "{second:?}");
        assert!(started && finished && !again);
        assert_eq!(active.iter().map(|restore| restore.id).collect::<Vec<_>>(), [first.id]);
        assert_eq!(listed.iter().map(|restore| restore.id).collect::<Vec<_>>(), [after.id, first.id]);
        assert_eq!(page.iter().map(|restore| restore.id).collect::<Vec<_>>(), [first.id]);
        assert_eq!(
            (failed.status, failed.error.as_deref(), failed.server_id),
            (BackupStatus::Failed, Some("pg_restore: error"), Some(server))
        );
        assert!(failed.started_at.is_some() && failed.finished_at.is_some());
    }

    #[tokio::test]
    async fn retention_keeps_backups_being_restored() {
        let (_dir, store) = store().await;
        let (service, destination) = database(&store).await;
        let fields =
            BackupScheduleFields { destination_id: destination, cron: "0 3 * * *".to_string(), keep_last: 1, enabled: true };
        let schedule = store.create_backup_schedule(service, fields, None).await.unwrap().id;
        let old = succeeded_backup(&store, service, destination, Some(schedule)).await;
        succeeded_backup(&store, service, destination, Some(schedule)).await;
        let restore =
            store.create_restore(NewRestore { service_id: service, backup_id: old, safety_backup_id: None }).await.unwrap();

        let while_restoring = store.list_backups_beyond(schedule, 1).await.unwrap();
        store.finish_restore(restore.id, Ok(())).await.unwrap();
        let after = store.list_backups_beyond(schedule, 1).await.unwrap();

        assert!(while_restoring.is_empty());
        assert_eq!(after.iter().map(|backup| backup.id).collect::<Vec<_>>(), [old]);
    }
}
