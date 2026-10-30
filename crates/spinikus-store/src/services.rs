use async_trait::async_trait;
use spinikus_core::{
    id::{EnvironmentId, ServiceId},
    service::{Service, ServiceConfig, ServiceFields, ServiceKind}
};

use crate::{EnvironmentRepository, SqliteStore, StoreError, error::parse_column};

const ENTITY: &str = "service";

#[async_trait]
pub trait ServiceRepository: Send + Sync {
    /// Fails with `NotFound` if the environment does not exist or is deleted.
    async fn create_service(
        &self,
        environment_id: EnvironmentId,
        kind: ServiceKind,
        fields: ServiceFields
    ) -> Result<Service, StoreError>;

    /// Fails with `NotFound` for deleted services too.
    async fn get_service(&self, id: ServiceId) -> Result<Service, StoreError>;

    /// The environment's services in creation order, starting after the `after` cursor.
    async fn list_services(
        &self,
        environment_id: EnvironmentId,
        after: Option<ServiceId>,
        limit: u32
    ) -> Result<Vec<Service>, StoreError>;

    /// Replaces the service's fields if it is still at `revision`.
    async fn update_service(&self, id: ServiceId, revision: i64, fields: ServiceFields) -> Result<Service, StoreError>;

    /// Soft-deletes the service, which frees its slug. Refused while it has domains, so a
    /// deleted service never keeps hostnames reserved.
    async fn delete_service(&self, id: ServiceId) -> Result<(), StoreError>;
}

#[derive(sqlx::FromRow)]
struct ServiceRow {
    id: String,
    environment_id: String,
    name: String,
    slug: String,
    kind: String,
    config: String,
    revision: i64,
    created_at: String,
    updated_at: String
}

impl TryFrom<ServiceRow> for Service {
    type Error = StoreError;

    fn try_from(row: ServiceRow) -> Result<Self, Self::Error> {
        Ok(Service {
            id: parse_column(ENTITY, "id", &row.id)?,
            environment_id: parse_column(ENTITY, "environment_id", &row.environment_id)?,
            name: row.name,
            slug: parse_column(ENTITY, "slug", &row.slug)?,
            kind: parse_column(ENTITY, "kind", &row.kind)?,
            config: serde_json::from_str::<ServiceConfig>(&row.config).map_err(|error| StoreError::InvalidRow {
                entity: ENTITY,
                column: "config",
                problem: error.to_string()
            })?,
            revision: row.revision,
            created_at: parse_column(ENTITY, "created_at", &row.created_at)?,
            updated_at: parse_column(ENTITY, "updated_at", &row.updated_at)?
        })
    }
}

fn slug_conflict(fields: &ServiceFields) -> impl FnOnce(&str) -> StoreError {
    let value = fields.slug.to_string();
    move |_| StoreError::Conflict { entity: ENTITY, field: "slug", value }
}

fn not_found(id: ServiceId) -> StoreError {
    StoreError::NotFound { entity: ENTITY, id: id.to_string() }
}

#[async_trait]
impl ServiceRepository for SqliteStore {
    async fn create_service(
        &self,
        environment_id: EnvironmentId,
        kind: ServiceKind,
        fields: ServiceFields
    ) -> Result<Service, StoreError> {
        let row: Option<ServiceRow> = sqlx::query_as(
            "INSERT INTO services (id, environment_id, name, slug, kind, config)
             SELECT ?, id, ?, ?, ?, ? FROM environments WHERE id = ? AND deleted_at IS NULL
             RETURNING id, environment_id, name, slug, kind, config, revision, created_at, updated_at"
        )
        .bind(ServiceId::new().to_string())
        .bind(&fields.name)
        .bind(fields.slug.as_str())
        .bind(kind.as_str())
        .bind(serde_json::Value::Object(fields.config.clone()).to_string())
        .bind(environment_id.to_string())
        .fetch_optional(self.pool())
        .await
        .map_err(|error| StoreError::on_unique_violation(error, slug_conflict(&fields)))?;

        match row {
            Some(row) => row.try_into(),
            None => Err(StoreError::NotFound { entity: "environment", id: environment_id.to_string() })
        }
    }

    async fn get_service(&self, id: ServiceId) -> Result<Service, StoreError> {
        let row: Option<ServiceRow> = sqlx::query_as(
            "SELECT id, environment_id, name, slug, kind, config, revision, created_at, updated_at
             FROM services WHERE id = ? AND deleted_at IS NULL"
        )
        .bind(id.to_string())
        .fetch_optional(self.pool())
        .await?;

        row.ok_or_else(|| not_found(id))?.try_into()
    }

    async fn list_services(
        &self,
        environment_id: EnvironmentId,
        after: Option<ServiceId>,
        limit: u32
    ) -> Result<Vec<Service>, StoreError> {
        let rows: Vec<ServiceRow> = sqlx::query_as(
            "SELECT id, environment_id, name, slug, kind, config, revision, created_at, updated_at
             FROM services WHERE environment_id = ? AND deleted_at IS NULL AND id > ? ORDER BY id LIMIT ?"
        )
        .bind(environment_id.to_string())
        .bind(after.map(|id| id.to_string()).unwrap_or_default())
        .bind(limit)
        .fetch_all(self.pool())
        .await?;

        if rows.is_empty() {
            // Tell an empty page apart from a missing environment.
            self.get_environment(environment_id).await?;
        }
        rows.into_iter().map(Service::try_from).collect()
    }

    async fn update_service(&self, id: ServiceId, revision: i64, fields: ServiceFields) -> Result<Service, StoreError> {
        let row: Option<ServiceRow> = sqlx::query_as(
            "UPDATE services
             SET name = ?, slug = ?, config = ?, revision = revision + 1,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ? AND revision = ? AND deleted_at IS NULL
             RETURNING id, environment_id, name, slug, kind, config, revision, created_at, updated_at"
        )
        .bind(&fields.name)
        .bind(fields.slug.as_str())
        .bind(serde_json::Value::Object(fields.config.clone()).to_string())
        .bind(id.to_string())
        .bind(revision)
        .fetch_optional(self.pool())
        .await
        .map_err(|error| StoreError::on_unique_violation(error, slug_conflict(&fields)))?;

        match row {
            Some(row) => row.try_into(),
            // Nothing matched: either the service is gone or it has moved on to a newer revision.
            None => {
                let current = self.get_service(id).await?;
                Err(StoreError::RevisionMismatch { entity: ENTITY, id: id.to_string(), expected: revision, actual: current.revision })
            }
        }
    }

    async fn delete_service(&self, id: ServiceId) -> Result<(), StoreError> {
        let result = sqlx::query(
            "UPDATE services
             SET deleted_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                 revision = revision + 1
             WHERE id = ? AND deleted_at IS NULL
               AND NOT EXISTS (SELECT 1 FROM domains WHERE service_id = services.id AND deleted_at IS NULL)"
        )
        .bind(id.to_string())
        .execute(self.pool())
        .await?;

        if result.rows_affected() == 0 {
            // Either the service is gone, or it still has domains.
            self.get_service(id).await?;
            return Err(StoreError::HasChildren { entity: ENTITY, id: id.to_string(), children: "domains" });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use spinikus_core::{
        environment::{EnvironmentFields, EnvironmentKind},
        project::ProjectFields
    };

    use super::*;
    use crate::{ProjectRepository, test_support::store};

    fn fields(name: &str, slug: &str, config: serde_json::Value) -> ServiceFields {
        let serde_json::Value::Object(config) = config else { panic!("config must be an object") };
        ServiceFields { name: name.to_string(), slug: slug.parse().unwrap(), config }
    }

    async fn environment(store: &SqliteStore, slug: &str) -> EnvironmentId {
        let project = ProjectFields { name: slug.to_string(), slug: slug.parse().unwrap() };
        let project_id = store.create_project(project).await.unwrap().id;
        let env = EnvironmentFields { name: "Staging".to_string(), slug: "staging".parse().unwrap() };
        store.create_environment(project_id, EnvironmentKind::Staging, env).await.unwrap().id
    }

    #[tokio::test]
    async fn creates_and_gets_a_service_with_config() {
        let (_dir, store) = store().await;
        let env = environment(&store, "web").await;

        let created = store
            .create_service(env, ServiceKind::Database, fields("Postgres", "postgres", json!({ "engine": "postgres", "version": 17 })))
            .await
            .unwrap();

        assert_eq!(store.get_service(created.id).await.unwrap(), created);
        assert_eq!(created.kind, ServiceKind::Database);
        assert_eq!(serde_json::Value::Object(created.config), json!({ "engine": "postgres", "version": 17 }));
    }

    #[tokio::test]
    async fn requires_an_active_environment_and_a_free_slug() {
        let (_dir, store) = store().await;
        let env = environment(&store, "web").await;
        store.create_service(env, ServiceKind::App, fields("Web", "web", json!({}))).await.unwrap();

        let missing = store.create_service(EnvironmentId::new(), ServiceKind::App, fields("X", "x", json!({}))).await;
        let taken = store.create_service(env, ServiceKind::App, fields("Web 2", "web", json!({}))).await;

        assert!(matches!(missing, Err(StoreError::NotFound { entity: "environment", .. })), "{missing:?}");
        assert!(matches!(taken, Err(StoreError::Conflict { field: "slug", .. })), "{taken:?}");
    }

    #[tokio::test]
    async fn updates_config_at_the_current_revision() {
        let (_dir, store) = store().await;
        let env = environment(&store, "web").await;
        let service = store.create_service(env, ServiceKind::App, fields("Web", "web", json!({}))).await.unwrap();

        let updated = store.update_service(service.id, 1, fields("Web", "web", json!({ "replicas": 2 }))).await.unwrap();
        let stale = store.update_service(service.id, 1, fields("Web", "web", json!({}))).await;

        assert_eq!(serde_json::Value::Object(updated.config), json!({ "replicas": 2 }));
        assert_eq!(updated.revision, 2);
        assert!(matches!(stale, Err(StoreError::RevisionMismatch { actual: 2, .. })), "{stale:?}");
    }

    #[tokio::test]
    async fn lists_services_and_guards_environment_deletion() {
        let (_dir, store) = store().await;
        let env = environment(&store, "web").await;
        let service = store.create_service(env, ServiceKind::App, fields("Web", "web", json!({}))).await.unwrap();

        assert_eq!(store.list_services(env, None, 10).await.unwrap(), vec![service.clone()]);
        let refused = store.delete_environment(env).await;
        store.delete_service(service.id).await.unwrap();

        assert!(matches!(refused, Err(StoreError::HasChildren { children: "services", .. })), "{refused:?}");
        assert!(store.list_services(env, None, 10).await.unwrap().is_empty());
        store.delete_environment(env).await.unwrap();
    }
}
