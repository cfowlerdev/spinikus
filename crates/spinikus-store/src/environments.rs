use async_trait::async_trait;
use spinikus_core::{
    environment::{Environment, EnvironmentFields, EnvironmentKind},
    id::{EnvironmentId, ProjectId}
};

use crate::{ProjectRepository, SqliteStore, StoreError, error::parse_column};

const ENTITY: &str = "environment";

#[async_trait]
pub trait EnvironmentRepository: Send + Sync {
    /// Fails with `NotFound` if the project does not exist or is deleted.
    async fn create_environment(
        &self,
        project_id: ProjectId,
        kind: EnvironmentKind,
        fields: EnvironmentFields
    ) -> Result<Environment, StoreError>;

    /// Fails with `NotFound` for deleted environments too.
    async fn get_environment(&self, id: EnvironmentId) -> Result<Environment, StoreError>;

    /// The project's environments in creation order, starting after the `after` cursor.
    async fn list_environments(
        &self,
        project_id: ProjectId,
        after: Option<EnvironmentId>,
        limit: u32
    ) -> Result<Vec<Environment>, StoreError>;

    /// Replaces the environment's fields if it is still at `revision`.
    async fn update_environment(
        &self,
        id: EnvironmentId,
        revision: i64,
        fields: EnvironmentFields
    ) -> Result<Environment, StoreError>;

    /// Soft-deletes the environment, which frees its slug. Refused while it has services.
    async fn delete_environment(&self, id: EnvironmentId) -> Result<(), StoreError>;
}

#[derive(sqlx::FromRow)]
struct EnvironmentRow {
    id: String,
    project_id: String,
    name: String,
    slug: String,
    kind: String,
    pr_number: Option<u32>,
    revision: i64,
    created_at: String,
    updated_at: String
}

impl TryFrom<EnvironmentRow> for Environment {
    type Error = StoreError;

    fn try_from(row: EnvironmentRow) -> Result<Self, Self::Error> {
        Ok(Environment {
            id: parse_column(ENTITY, "id", &row.id)?,
            project_id: parse_column(ENTITY, "project_id", &row.project_id)?,
            name: row.name,
            slug: parse_column(ENTITY, "slug", &row.slug)?,
            kind: EnvironmentKind::from_parts(&row.kind, row.pr_number).map_err(|error| StoreError::InvalidRow {
                entity: ENTITY,
                column: "kind",
                problem: error.to_string()
            })?,
            revision: row.revision,
            created_at: parse_column(ENTITY, "created_at", &row.created_at)?,
            updated_at: parse_column(ENTITY, "updated_at", &row.updated_at)?
        })
    }
}

/// Works out which unique index a write violated from SQLite's message.
fn conflict(kind: Option<EnvironmentKind>, fields: &EnvironmentFields) -> impl FnOnce(&str) -> StoreError {
    let slug = fields.slug.to_string();
    move |message| match kind {
        Some(EnvironmentKind::Preview { pr_number }) if message.contains("pr_number") => {
            StoreError::Conflict { entity: ENTITY, field: "pr_number", value: pr_number.to_string() }
        }
        Some(EnvironmentKind::Production) if !message.contains("slug") => {
            StoreError::Conflict { entity: ENTITY, field: "kind", value: "production".to_string() }
        }
        _ => StoreError::Conflict { entity: ENTITY, field: "slug", value: slug }
    }
}

fn not_found(id: EnvironmentId) -> StoreError {
    StoreError::NotFound { entity: ENTITY, id: id.to_string() }
}

#[async_trait]
impl EnvironmentRepository for SqliteStore {
    async fn create_environment(
        &self,
        project_id: ProjectId,
        kind: EnvironmentKind,
        fields: EnvironmentFields
    ) -> Result<Environment, StoreError> {
        let row: Option<EnvironmentRow> = sqlx::query_as(
            "INSERT INTO environments (id, project_id, name, slug, kind, pr_number)
             SELECT ?, id, ?, ?, ?, ? FROM projects WHERE id = ? AND deleted_at IS NULL
             RETURNING id, project_id, name, slug, kind, pr_number, revision, created_at, updated_at"
        )
        .bind(EnvironmentId::new().to_string())
        .bind(&fields.name)
        .bind(fields.slug.as_str())
        .bind(kind.as_str())
        .bind(kind.pr_number())
        .bind(project_id.to_string())
        .fetch_optional(self.pool())
        .await
        .map_err(|error| StoreError::on_unique_violation(error, conflict(Some(kind), &fields)))?;

        match row {
            Some(row) => row.try_into(),
            None => Err(StoreError::NotFound { entity: "project", id: project_id.to_string() })
        }
    }

    async fn get_environment(&self, id: EnvironmentId) -> Result<Environment, StoreError> {
        let row: Option<EnvironmentRow> = sqlx::query_as(
            "SELECT id, project_id, name, slug, kind, pr_number, revision, created_at, updated_at
             FROM environments WHERE id = ? AND deleted_at IS NULL"
        )
        .bind(id.to_string())
        .fetch_optional(self.pool())
        .await?;

        row.ok_or_else(|| not_found(id))?.try_into()
    }

    async fn list_environments(
        &self,
        project_id: ProjectId,
        after: Option<EnvironmentId>,
        limit: u32
    ) -> Result<Vec<Environment>, StoreError> {
        let rows: Vec<EnvironmentRow> = sqlx::query_as(
            "SELECT id, project_id, name, slug, kind, pr_number, revision, created_at, updated_at
             FROM environments WHERE project_id = ? AND deleted_at IS NULL AND id > ? ORDER BY id LIMIT ?"
        )
        .bind(project_id.to_string())
        .bind(after.map(|id| id.to_string()).unwrap_or_default())
        .bind(limit)
        .fetch_all(self.pool())
        .await?;

        if rows.is_empty() {
            // Tell an empty page apart from a missing project.
            self.get_project(project_id).await?;
        }
        rows.into_iter().map(Environment::try_from).collect()
    }

    async fn update_environment(
        &self,
        id: EnvironmentId,
        revision: i64,
        fields: EnvironmentFields
    ) -> Result<Environment, StoreError> {
        let row: Option<EnvironmentRow> = sqlx::query_as(
            "UPDATE environments
             SET name = ?, slug = ?, revision = revision + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ? AND revision = ? AND deleted_at IS NULL
             RETURNING id, project_id, name, slug, kind, pr_number, revision, created_at, updated_at"
        )
        .bind(&fields.name)
        .bind(fields.slug.as_str())
        .bind(id.to_string())
        .bind(revision)
        .fetch_optional(self.pool())
        .await
        .map_err(|error| StoreError::on_unique_violation(error, conflict(None, &fields)))?;

        match row {
            Some(row) => row.try_into(),
            // Nothing matched: either the environment is gone or it has moved on to a newer revision.
            None => {
                let current = self.get_environment(id).await?;
                Err(StoreError::RevisionMismatch { entity: ENTITY, id: id.to_string(), expected: revision, actual: current.revision })
            }
        }
    }

    async fn delete_environment(&self, id: EnvironmentId) -> Result<(), StoreError> {
        let result = sqlx::query(
            "UPDATE environments
             SET deleted_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                 revision = revision + 1
             WHERE id = ? AND deleted_at IS NULL
               AND NOT EXISTS (SELECT 1 FROM services WHERE environment_id = environments.id AND deleted_at IS NULL)"
        )
        .bind(id.to_string())
        .execute(self.pool())
        .await?;

        if result.rows_affected() == 0 {
            // Either the environment is gone, or it still has services.
            self.get_environment(id).await?;
            return Err(StoreError::HasChildren { entity: ENTITY, id: id.to_string(), children: "services" });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use spinikus_core::project::ProjectFields;

    use super::*;
    use crate::test_support::store;

    fn fields(name: &str, slug: &str) -> EnvironmentFields {
        EnvironmentFields { name: name.to_string(), slug: slug.parse().unwrap() }
    }

    async fn project(store: &SqliteStore, slug: &str) -> ProjectId {
        let fields = ProjectFields { name: slug.to_string(), slug: slug.parse().unwrap() };
        store.create_project(fields).await.unwrap().id
    }

    #[tokio::test]
    async fn creates_and_gets_an_environment() {
        let (_dir, store) = store().await;
        let project_id = project(&store, "web").await;

        let created = store
            .create_environment(project_id, EnvironmentKind::Preview { pr_number: 12 }, fields("PR 12", "pr-12"))
            .await
            .unwrap();

        assert_eq!(store.get_environment(created.id).await.unwrap(), created);
        assert_eq!(created.project_id, project_id);
        assert_eq!(created.kind, EnvironmentKind::Preview { pr_number: 12 });
        assert_eq!(created.revision, 1);
    }

    #[tokio::test]
    async fn requires_an_active_project() {
        let (_dir, store) = store().await;
        let deleted = project(&store, "old").await;
        store.delete_project(deleted).await.unwrap();

        for project_id in [ProjectId::new(), deleted] {
            let result = store.create_environment(project_id, EnvironmentKind::Staging, fields("Staging", "staging")).await;

            assert!(matches!(result, Err(StoreError::NotFound { entity: "project", .. })), "{result:?}");
        }
    }

    #[tokio::test]
    async fn slugs_are_unique_per_project() {
        let (_dir, store) = store().await;
        let web = project(&store, "web").await;
        let api = project(&store, "api").await;
        store.create_environment(web, EnvironmentKind::Staging, fields("Staging", "staging")).await.unwrap();

        let same_project = store.create_environment(web, EnvironmentKind::Staging, fields("Again", "staging")).await;
        let other_project = store.create_environment(api, EnvironmentKind::Staging, fields("Staging", "staging")).await;

        assert!(matches!(same_project, Err(StoreError::Conflict { field: "slug", .. })), "{same_project:?}");
        assert!(other_project.is_ok());
    }

    #[tokio::test]
    async fn allows_one_production_and_one_preview_per_pull_request() {
        let (_dir, store) = store().await;
        let web = project(&store, "web").await;
        let preview = EnvironmentKind::Preview { pr_number: 3 };
        store.create_environment(web, EnvironmentKind::Production, fields("Production", "production")).await.unwrap();
        store.create_environment(web, preview, fields("PR 3", "pr-3")).await.unwrap();

        let production = store.create_environment(web, EnvironmentKind::Production, fields("Prod 2", "prod-2")).await;
        let same_pr = store.create_environment(web, preview, fields("PR 3 again", "pr-3-again")).await;
        let other_pr = store.create_environment(web, EnvironmentKind::Preview { pr_number: 4 }, fields("PR 4", "pr-4")).await;

        assert!(matches!(production, Err(StoreError::Conflict { field: "kind", .. })), "{production:?}");
        assert!(matches!(same_pr, Err(StoreError::Conflict { field: "pr_number", .. })), "{same_pr:?}");
        assert!(other_pr.is_ok());
    }

    #[tokio::test]
    async fn lists_a_projects_environments() {
        let (_dir, store) = store().await;
        let web = project(&store, "web").await;
        let api = project(&store, "api").await;
        let staging = store.create_environment(web, EnvironmentKind::Staging, fields("Staging", "staging")).await.unwrap();
        store.create_environment(api, EnvironmentKind::Staging, fields("Staging", "staging")).await.unwrap();

        let listed = store.list_environments(web, None, 10).await.unwrap();
        let missing = store.list_environments(ProjectId::new(), None, 10).await;

        assert_eq!(listed, vec![staging]);
        assert!(matches!(missing, Err(StoreError::NotFound { entity: "project", .. })), "{missing:?}");
    }

    #[tokio::test]
    async fn updates_name_and_slug_but_not_kind() {
        let (_dir, store) = store().await;
        let web = project(&store, "web").await;
        let env = store.create_environment(web, EnvironmentKind::Staging, fields("Staging", "staging")).await.unwrap();

        let updated = store.update_environment(env.id, 1, fields("QA", "qa")).await.unwrap();
        let stale = store.update_environment(env.id, 1, fields("Stale", "stale")).await;

        assert_eq!((updated.name.as_str(), updated.slug.as_str(), updated.revision), ("QA", "qa", 2));
        assert_eq!(updated.kind, EnvironmentKind::Staging);
        assert!(matches!(stale, Err(StoreError::RevisionMismatch { actual: 2, .. })), "{stale:?}");
    }

    #[tokio::test]
    async fn a_project_with_environments_cannot_be_deleted() {
        let (_dir, store) = store().await;
        let web = project(&store, "web").await;
        let env = store.create_environment(web, EnvironmentKind::Staging, fields("Staging", "staging")).await.unwrap();

        let refused = store.delete_project(web).await;
        store.delete_environment(env.id).await.unwrap();

        assert!(matches!(refused, Err(StoreError::HasChildren { children: "environments", .. })), "{refused:?}");
        assert!(matches!(store.get_environment(env.id).await, Err(StoreError::NotFound { .. })));
        store.delete_project(web).await.unwrap();
    }
}
