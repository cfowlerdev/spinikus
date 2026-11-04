use async_trait::async_trait;
use spinikus_core::{
    id::{EnvironmentId, ServiceId},
    route::ServiceNames,
    service::{Service, ServiceConfig, ServiceFields, ServiceKind},
    slug::Slug,
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
        fields: ServiceFields,
    ) -> Result<Service, StoreError>;

    /// Fails with `NotFound` for deleted services too.
    async fn get_service(&self, id: ServiceId) -> Result<Service, StoreError>;

    /// The environment's service with this slug, unless it is deleted.
    async fn find_service_by_slug(&self, environment_id: EnvironmentId, slug: &Slug) -> Result<Option<Service>, StoreError>;

    /// The slugs of every app service that is not deleted, with its environment's and
    /// project's, for generated hostnames.
    async fn list_app_service_names(&self) -> Result<Vec<ServiceNames>, StoreError>;

    /// The environment's services in creation order, starting after the `after` cursor.
    async fn list_services(
        &self,
        environment_id: EnvironmentId,
        after: Option<ServiceId>,
        limit: u32,
    ) -> Result<Vec<Service>, StoreError>;

    /// Replaces the service's fields if it is still at `revision`.
    async fn update_service(&self, id: ServiceId, revision: i64, fields: ServiceFields) -> Result<Service, StoreError>;

    /// Marks the service stopped (keeping when it was, if it already is) or not. This is
    /// state, not settings, so the revision stays.
    async fn set_service_stopped(&self, id: ServiceId, stopped: bool) -> Result<Service, StoreError>;

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
    server_id: Option<String>,
    stopped_at: Option<String>,
    revision: i64,
    created_at: String,
    updated_at: String,
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
                problem: error.to_string(),
            })?,
            server_id: row.server_id.map(|id| parse_column(ENTITY, "server_id", &id)).transpose()?,
            stopped_at: row.stopped_at.map(|at| parse_column(ENTITY, "stopped_at", &at)).transpose()?,
            revision: row.revision,
            created_at: parse_column(ENTITY, "created_at", &row.created_at)?,
            updated_at: parse_column(ENTITY, "updated_at", &row.updated_at)?,
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
        fields: ServiceFields,
    ) -> Result<Service, StoreError> {
        let row: Option<ServiceRow> = sqlx::query_as(
            "INSERT INTO services (id, environment_id, name, slug, kind, config, server_id)
             SELECT ?, id, ?, ?, ?, ?, ? FROM environments WHERE id = ? AND deleted_at IS NULL
             RETURNING id, environment_id, name, slug, kind, config, server_id, stopped_at, revision, created_at, updated_at",
        )
        .bind(ServiceId::new().to_string())
        .bind(&fields.name)
        .bind(fields.slug.as_str())
        .bind(kind.as_str())
        .bind(serde_json::Value::Object(fields.config.clone()).to_string())
        .bind(fields.server_id.map(|id| id.to_string()))
        .bind(environment_id.to_string())
        .fetch_optional(self.pool())
        .await
        .map_err(|error| StoreError::on_unique_violation(error, slug_conflict(&fields)))?;

        match row {
            Some(row) => row.try_into(),
            None => Err(StoreError::NotFound { entity: "environment", id: environment_id.to_string() }),
        }
    }

    async fn list_app_service_names(&self) -> Result<Vec<ServiceNames>, StoreError> {
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT services.id, services.slug, environments.slug, projects.slug
             FROM services
             JOIN environments ON environments.id = services.environment_id
             JOIN projects ON projects.id = environments.project_id
             WHERE services.kind = 'app' AND services.deleted_at IS NULL
             ORDER BY services.id",
        )
        .fetch_all(self.pool())
        .await?;
        rows.into_iter()
            .map(|(id, service, environment, project)| {
                Ok(ServiceNames {
                    service_id: parse_column(ENTITY, "id", &id)?,
                    service: parse_column(ENTITY, "slug", &service)?,
                    environment: parse_column("environment", "slug", &environment)?,
                    project: parse_column("project", "slug", &project)?,
                })
            })
            .collect()
    }

    async fn get_service(&self, id: ServiceId) -> Result<Service, StoreError> {
        let row: Option<ServiceRow> = sqlx::query_as(
            "SELECT id, environment_id, name, slug, kind, config, server_id, stopped_at, revision, created_at, updated_at
             FROM services WHERE id = ? AND deleted_at IS NULL",
        )
        .bind(id.to_string())
        .fetch_optional(self.pool())
        .await?;

        row.ok_or_else(|| not_found(id))?.try_into()
    }

    async fn find_service_by_slug(&self, environment_id: EnvironmentId, slug: &Slug) -> Result<Option<Service>, StoreError> {
        let row: Option<ServiceRow> = sqlx::query_as(
            "SELECT id, environment_id, name, slug, kind, config, server_id, stopped_at, revision, created_at, updated_at
             FROM services WHERE environment_id = ? AND slug = ? AND deleted_at IS NULL",
        )
        .bind(environment_id.to_string())
        .bind(slug.as_str())
        .fetch_optional(self.pool())
        .await?;

        row.map(Service::try_from).transpose()
    }

    async fn list_services(
        &self,
        environment_id: EnvironmentId,
        after: Option<ServiceId>,
        limit: u32,
    ) -> Result<Vec<Service>, StoreError> {
        let rows: Vec<ServiceRow> = sqlx::query_as(
            "SELECT id, environment_id, name, slug, kind, config, server_id, stopped_at, revision, created_at, updated_at
             FROM services WHERE environment_id = ? AND deleted_at IS NULL AND id > ? ORDER BY id LIMIT ?",
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
             SET name = ?, slug = ?, config = ?, server_id = ?, revision = revision + 1,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ? AND revision = ? AND deleted_at IS NULL
             RETURNING id, environment_id, name, slug, kind, config, server_id, stopped_at, revision, created_at, updated_at",
        )
        .bind(&fields.name)
        .bind(fields.slug.as_str())
        .bind(serde_json::Value::Object(fields.config.clone()).to_string())
        .bind(fields.server_id.map(|id| id.to_string()))
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
                Err(StoreError::RevisionMismatch {
                    entity: ENTITY,
                    id: id.to_string(),
                    expected: revision,
                    actual: current.revision,
                })
            }
        }
    }

    async fn set_service_stopped(&self, id: ServiceId, stopped: bool) -> Result<Service, StoreError> {
        let row: Option<ServiceRow> = sqlx::query_as(
            "UPDATE services
             SET stopped_at = CASE WHEN ? THEN COALESCE(stopped_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) END
             WHERE id = ? AND deleted_at IS NULL
             RETURNING id, environment_id, name, slug, kind, config, server_id, stopped_at, revision, created_at, updated_at",
        )
        .bind(stopped)
        .bind(id.to_string())
        .fetch_optional(self.pool())
        .await?;

        row.ok_or_else(|| not_found(id))?.try_into()
    }

    async fn delete_service(&self, id: ServiceId) -> Result<(), StoreError> {
        let result = sqlx::query(
            "UPDATE services
             SET deleted_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                 revision = revision + 1
             WHERE id = ? AND deleted_at IS NULL
               AND NOT EXISTS (SELECT 1 FROM domains WHERE service_id = services.id AND deleted_at IS NULL)",
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
        project::ProjectFields,
        server::ServerFields,
    };

    use super::*;
    use crate::{ProjectRepository, ServerRepository, test_support::store};

    fn fields(name: &str, slug: &str, config: serde_json::Value) -> ServiceFields {
        let serde_json::Value::Object(config) = config else { panic!("config must be an object") };
        ServiceFields { name: name.to_string(), slug: slug.parse().unwrap(), config, server_id: None }
    }

    async fn environment(store: &SqliteStore, slug: &str) -> EnvironmentId {
        let project = ProjectFields { name: slug.to_string(), slug: slug.parse().unwrap() };
        let project_id = store.create_project(project).await.unwrap().id;
        let env = EnvironmentFields { name: "Staging".to_string(), slug: "staging".parse().unwrap(), previews: false };
        store.create_environment(project_id, EnvironmentKind::Staging, env).await.unwrap().id
    }

    #[tokio::test]
    async fn lists_the_names_of_app_services() {
        let (_dir, store) = store().await;
        let environment = environment(&store, "shop").await;
        let web = store.create_service(environment, ServiceKind::App, fields("Web", "web", json!({}))).await.unwrap();
        store.create_service(environment, ServiceKind::Database, fields("DB", "db", json!({}))).await.unwrap();
        let old = store.create_service(environment, ServiceKind::App, fields("Old", "old", json!({}))).await.unwrap();
        store.delete_service(old.id).await.unwrap();

        let names = store.list_app_service_names().await.unwrap();

        assert_eq!(names.len(), 1);
        assert_eq!(names[0].service_id, web.id);
        let slugs = (names[0].service.as_str(), names[0].environment.as_str(), names[0].project.as_str());
        assert_eq!(slugs, ("web", "staging", "shop"));
    }

    #[tokio::test]
    async fn finds_a_service_by_its_slug_in_its_environment() {
        let (_dir, store) = store().await;
        let (staging, other) = (environment(&store, "shop").await, environment(&store, "blog").await);
        let db = store.create_service(staging, ServiceKind::Database, fields("DB", "db", json!({}))).await.unwrap();
        let old = store.create_service(staging, ServiceKind::App, fields("Old", "old", json!({}))).await.unwrap();
        store.delete_service(old.id).await.unwrap();
        let slug = |slug: &str| slug.parse::<Slug>().unwrap();

        assert_eq!(store.find_service_by_slug(staging, &slug("db")).await.unwrap().map(|found| found.id), Some(db.id));
        assert_eq!(store.find_service_by_slug(other, &slug("db")).await.unwrap(), None);
        assert_eq!(store.find_service_by_slug(staging, &slug("old")).await.unwrap(), None);
    }

    #[tokio::test]
    async fn stores_the_placement() {
        let (_dir, store) = store().await;
        let environment = environment(&store, "staging").await;
        let server = store.create_server(ServerFields { name: "web-1".to_string() }).await.unwrap().id;

        let placed = ServiceFields { server_id: Some(server), ..fields("Web", "web", json!({})) };
        let service = store.create_service(environment, ServiceKind::App, placed).await.unwrap();
        let read = store.get_service(service.id).await.unwrap();
        let moved = store.update_service(service.id, service.revision, fields("Web", "web", json!({}))).await.unwrap();

        assert_eq!(service.server_id, Some(server));
        assert_eq!(read.server_id, Some(server));
        assert_eq!(moved.server_id, None);
    }

    #[tokio::test]
    async fn creates_and_gets_a_service_with_config() {
        let (_dir, store) = store().await;
        let env = environment(&store, "web").await;

        let created = store
            .create_service(
                env,
                ServiceKind::Database,
                fields("Postgres", "postgres", json!({ "engine": "postgres", "version": 17 })),
            )
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
