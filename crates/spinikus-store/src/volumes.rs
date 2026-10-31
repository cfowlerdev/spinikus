use async_trait::async_trait;
use spinikus_core::{
    id::{ServiceId, VolumeId},
    volume::{Volume, VolumeFields},
};

use crate::{ServiceRepository, SqliteStore, StoreError, error::parse_column};

const ENTITY: &str = "volume";

#[async_trait]
pub trait VolumeRepository: Send + Sync {
    /// Fails with `NotFound` if the service does not exist or is deleted, and `Conflict` if
    /// the service already has a volume with the name or mount path.
    async fn create_volume(&self, service_id: ServiceId, fields: VolumeFields) -> Result<Volume, StoreError>;

    async fn get_volume(&self, id: VolumeId) -> Result<Volume, StoreError>;

    /// The service's volumes, by name.
    async fn list_volumes(&self, service_id: ServiceId) -> Result<Vec<Volume>, StoreError>;

    /// Replaces the volume's fields if it is still at `revision`. The data stays where it is.
    async fn update_volume(&self, id: VolumeId, revision: i64, fields: VolumeFields) -> Result<Volume, StoreError>;

    /// Deletes the volume's row. Removing its data from the server is a separate cleanup job.
    async fn delete_volume(&self, id: VolumeId) -> Result<(), StoreError>;
}

macro_rules! columns {
    () => {
        "id, service_id, name, mount_path, revision, created_at, updated_at"
    };
}

#[derive(sqlx::FromRow)]
struct VolumeRow {
    id: String,
    service_id: String,
    name: String,
    mount_path: String,
    revision: i64,
    created_at: String,
    updated_at: String,
}

impl TryFrom<VolumeRow> for Volume {
    type Error = StoreError;

    fn try_from(row: VolumeRow) -> Result<Self, Self::Error> {
        Ok(Volume {
            id: parse_column(ENTITY, "id", &row.id)?,
            service_id: parse_column(ENTITY, "service_id", &row.service_id)?,
            name: parse_column(ENTITY, "name", &row.name)?,
            mount_path: parse_column(ENTITY, "mount_path", &row.mount_path)?,
            revision: row.revision,
            created_at: parse_column(ENTITY, "created_at", &row.created_at)?,
            updated_at: parse_column(ENTITY, "updated_at", &row.updated_at)?,
        })
    }
}

/// SQLite's message names the violated columns, such as `volumes.service_id, volumes.mount_path`.
fn conflict(fields: &VolumeFields) -> impl FnOnce(&str) -> StoreError {
    let (name, mount_path) = (fields.name.to_string(), fields.mount_path.to_string());
    move |message| {
        if message.contains("mount_path") {
            StoreError::Conflict { entity: ENTITY, field: "mount_path", value: mount_path }
        } else {
            StoreError::Conflict { entity: ENTITY, field: "name", value: name }
        }
    }
}

fn not_found(id: VolumeId) -> StoreError {
    StoreError::NotFound { entity: ENTITY, id: id.to_string() }
}

#[async_trait]
impl VolumeRepository for SqliteStore {
    async fn create_volume(&self, service_id: ServiceId, fields: VolumeFields) -> Result<Volume, StoreError> {
        let row: Option<VolumeRow> = sqlx::query_as(concat!(
            "INSERT INTO volumes (id, service_id, name, mount_path)
             SELECT ?, id, ?, ? FROM services WHERE id = ? AND deleted_at IS NULL
             RETURNING ",
            columns!()
        ))
        .bind(VolumeId::new().to_string())
        .bind(fields.name.as_str())
        .bind(fields.mount_path.as_str())
        .bind(service_id.to_string())
        .fetch_optional(self.pool())
        .await
        .map_err(|error| StoreError::on_unique_violation(error, conflict(&fields)))?;

        match row {
            Some(row) => row.try_into(),
            None => Err(StoreError::NotFound { entity: "service", id: service_id.to_string() }),
        }
    }

    async fn get_volume(&self, id: VolumeId) -> Result<Volume, StoreError> {
        let row: Option<VolumeRow> = sqlx::query_as(concat!("SELECT ", columns!(), " FROM volumes WHERE id = ?"))
            .bind(id.to_string())
            .fetch_optional(self.pool())
            .await?;
        row.ok_or_else(|| not_found(id))?.try_into()
    }

    async fn list_volumes(&self, service_id: ServiceId) -> Result<Vec<Volume>, StoreError> {
        let rows: Vec<VolumeRow> =
            sqlx::query_as(concat!("SELECT ", columns!(), " FROM volumes WHERE service_id = ? ORDER BY name"))
                .bind(service_id.to_string())
                .fetch_all(self.pool())
                .await?;

        if rows.is_empty() {
            // Tell an empty list apart from a missing or deleted service.
            self.get_service(service_id).await?;
        }
        rows.into_iter().map(Volume::try_from).collect()
    }

    async fn update_volume(&self, id: VolumeId, revision: i64, fields: VolumeFields) -> Result<Volume, StoreError> {
        let row: Option<VolumeRow> = sqlx::query_as(concat!(
            "UPDATE volumes
             SET name = ?, mount_path = ?, revision = revision + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ? AND revision = ?
             RETURNING ",
            columns!()
        ))
        .bind(fields.name.as_str())
        .bind(fields.mount_path.as_str())
        .bind(id.to_string())
        .bind(revision)
        .fetch_optional(self.pool())
        .await
        .map_err(|error| StoreError::on_unique_violation(error, conflict(&fields)))?;

        match row {
            Some(row) => row.try_into(),
            // Nothing matched: either the volume is gone or it has moved on to a newer revision.
            None => {
                let current = self.get_volume(id).await?;
                Err(StoreError::RevisionMismatch {
                    entity: ENTITY,
                    id: id.to_string(),
                    expected: revision,
                    actual: current.revision,
                })
            }
        }
    }

    async fn delete_volume(&self, id: VolumeId) -> Result<(), StoreError> {
        let result = sqlx::query("DELETE FROM volumes WHERE id = ?").bind(id.to_string()).execute(self.pool()).await?;

        if result.rows_affected() == 0 {
            return Err(not_found(id));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Map;
    use spinikus_core::{
        environment::{EnvironmentFields, EnvironmentKind},
        project::ProjectFields,
        service::{ServiceFields, ServiceKind},
    };

    use super::*;
    use crate::{EnvironmentRepository, ProjectRepository, test_support::store};

    async fn service(store: &SqliteStore) -> ServiceId {
        let project = ProjectFields { name: "Web".to_string(), slug: "web".parse().unwrap() };
        let project_id = store.create_project(project).await.unwrap().id;
        let env = EnvironmentFields { name: "Production".to_string(), slug: "production".parse().unwrap() };
        let env_id = store.create_environment(project_id, EnvironmentKind::Production, env).await.unwrap().id;
        let service = ServiceFields { name: "DB".to_string(), slug: "db".parse().unwrap(), config: Map::new(), server_id: None };
        store.create_service(env_id, ServiceKind::Database, service).await.unwrap().id
    }

    fn fields(name: &str, mount_path: &str) -> VolumeFields {
        VolumeFields { name: name.parse().unwrap(), mount_path: mount_path.parse().unwrap() }
    }

    #[tokio::test]
    async fn creates_gets_and_lists_volumes() {
        let (_dir, store) = store().await;
        let service_id = service(&store).await;

        let data = store.create_volume(service_id, fields("data", "/var/lib/postgresql/data")).await.unwrap();
        let backups = store.create_volume(service_id, fields("backups", "/backups/")).await.unwrap();

        assert_eq!(store.get_volume(data.id).await.unwrap(), data);
        assert_eq!(backups.mount_path.as_str(), "/backups");
        let listed: Vec<_> = store.list_volumes(service_id).await.unwrap().into_iter().map(|v| v.id).collect();
        assert_eq!(listed, vec![backups.id, data.id]);
    }

    #[tokio::test]
    async fn names_and_mount_paths_are_unique_per_service() {
        let (_dir, store) = store().await;
        let service_id = service(&store).await;
        store.create_volume(service_id, fields("data", "/data")).await.unwrap();

        let same_name = store.create_volume(service_id, fields("data", "/other")).await;
        let same_path = store.create_volume(service_id, fields("other", "/data")).await;

        assert!(matches!(same_name, Err(StoreError::Conflict { field: "name", .. })), "{same_name:?}");
        assert!(matches!(same_path, Err(StoreError::Conflict { field: "mount_path", .. })), "{same_path:?}");
    }

    #[tokio::test]
    async fn requires_an_active_service() {
        let (_dir, store) = store().await;
        let service_id = service(&store).await;
        store.delete_service(service_id).await.unwrap();

        let created = store.create_volume(service_id, fields("data", "/data")).await;
        let listed = store.list_volumes(ServiceId::new()).await;

        assert!(matches!(created, Err(StoreError::NotFound { entity: "service", .. })), "{created:?}");
        assert!(matches!(listed, Err(StoreError::NotFound { entity: "service", .. })), "{listed:?}");
    }

    #[tokio::test]
    async fn updates_at_the_current_revision() {
        let (_dir, store) = store().await;
        let service_id = service(&store).await;
        let volume = store.create_volume(service_id, fields("data", "/data")).await.unwrap();

        let updated = store.update_volume(volume.id, 1, fields("pgdata", "/var/lib/postgresql/data")).await.unwrap();
        let stale = store.update_volume(volume.id, 1, fields("x", "/x")).await;
        let missing = store.update_volume(VolumeId::new(), 1, fields("x", "/x")).await;

        assert_eq!(
            (updated.name.as_str(), updated.mount_path.as_str(), updated.revision),
            ("pgdata", "/var/lib/postgresql/data", 2)
        );
        assert!(matches!(stale, Err(StoreError::RevisionMismatch { expected: 1, actual: 2, .. })), "{stale:?}");
        assert!(matches!(missing, Err(StoreError::NotFound { .. })), "{missing:?}");
    }

    #[tokio::test]
    async fn deletes_a_volume() {
        let (_dir, store) = store().await;
        let service_id = service(&store).await;
        let volume = store.create_volume(service_id, fields("data", "/data")).await.unwrap();

        store.delete_volume(volume.id).await.unwrap();

        assert!(matches!(store.get_volume(volume.id).await, Err(StoreError::NotFound { .. })));
        assert!(matches!(store.delete_volume(volume.id).await, Err(StoreError::NotFound { .. })));
        assert!(store.list_volumes(service_id).await.unwrap().is_empty());
    }
}
