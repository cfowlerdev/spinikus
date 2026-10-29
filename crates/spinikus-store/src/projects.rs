use async_trait::async_trait;
use spinikus_core::{
    id::ProjectId,
    project::{Project, ProjectFields}
};

use crate::{SqliteStore, StoreError, error::parse_column};

const ENTITY: &str = "project";

#[async_trait]
pub trait ProjectRepository: Send + Sync {
    async fn create_project(&self, fields: ProjectFields) -> Result<Project, StoreError>;

    /// Fails with `NotFound` for deleted projects too.
    async fn get_project(&self, id: ProjectId) -> Result<Project, StoreError>;

    /// Projects in creation order, starting after the `after` cursor.
    async fn list_projects(&self, after: Option<ProjectId>, limit: u32) -> Result<Vec<Project>, StoreError>;

    /// Replaces the project's fields if it is still at `revision`.
    async fn update_project(&self, id: ProjectId, revision: i64, fields: ProjectFields) -> Result<Project, StoreError>;

    /// Soft-deletes the project, which frees its slug. Refused while it has environments.
    async fn delete_project(&self, id: ProjectId) -> Result<(), StoreError>;
}

#[derive(sqlx::FromRow)]
struct ProjectRow {
    id: String,
    name: String,
    slug: String,
    revision: i64,
    created_at: String,
    updated_at: String
}

impl TryFrom<ProjectRow> for Project {
    type Error = StoreError;

    fn try_from(row: ProjectRow) -> Result<Self, Self::Error> {
        Ok(Project {
            id: parse_column(ENTITY, "id", &row.id)?,
            name: row.name,
            slug: parse_column(ENTITY, "slug", &row.slug)?,
            revision: row.revision,
            created_at: parse_column(ENTITY, "created_at", &row.created_at)?,
            updated_at: parse_column(ENTITY, "updated_at", &row.updated_at)?
        })
    }
}

fn slug_conflict(fields: &ProjectFields) -> impl FnOnce(&str) -> StoreError {
    let value = fields.slug.to_string();
    move |_| StoreError::Conflict { entity: ENTITY, field: "slug", value }
}

fn not_found(id: ProjectId) -> StoreError {
    StoreError::NotFound { entity: ENTITY, id: id.to_string() }
}

#[async_trait]
impl ProjectRepository for SqliteStore {
    async fn create_project(&self, fields: ProjectFields) -> Result<Project, StoreError> {
        let row: ProjectRow = sqlx::query_as(
            "INSERT INTO projects (id, name, slug) VALUES (?, ?, ?)
             RETURNING id, name, slug, revision, created_at, updated_at"
        )
        .bind(ProjectId::new().to_string())
        .bind(&fields.name)
        .bind(fields.slug.as_str())
        .fetch_one(self.pool())
        .await
        .map_err(|error| StoreError::on_unique_violation(error, slug_conflict(&fields)))?;

        row.try_into()
    }

    async fn get_project(&self, id: ProjectId) -> Result<Project, StoreError> {
        let row: Option<ProjectRow> = sqlx::query_as(
            "SELECT id, name, slug, revision, created_at, updated_at
             FROM projects WHERE id = ? AND deleted_at IS NULL"
        )
        .bind(id.to_string())
        .fetch_optional(self.pool())
        .await?;

        row.ok_or_else(|| not_found(id))?.try_into()
    }

    async fn list_projects(&self, after: Option<ProjectId>, limit: u32) -> Result<Vec<Project>, StoreError> {
        let rows: Vec<ProjectRow> = sqlx::query_as(
            "SELECT id, name, slug, revision, created_at, updated_at
             FROM projects WHERE deleted_at IS NULL AND id > ? ORDER BY id LIMIT ?"
        )
        .bind(after.map(|id| id.to_string()).unwrap_or_default())
        .bind(limit)
        .fetch_all(self.pool())
        .await?;

        rows.into_iter().map(Project::try_from).collect()
    }

    async fn update_project(&self, id: ProjectId, revision: i64, fields: ProjectFields) -> Result<Project, StoreError> {
        let row: Option<ProjectRow> = sqlx::query_as(
            "UPDATE projects
             SET name = ?, slug = ?, revision = revision + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ? AND revision = ? AND deleted_at IS NULL
             RETURNING id, name, slug, revision, created_at, updated_at"
        )
        .bind(&fields.name)
        .bind(fields.slug.as_str())
        .bind(id.to_string())
        .bind(revision)
        .fetch_optional(self.pool())
        .await
        .map_err(|error| StoreError::on_unique_violation(error, slug_conflict(&fields)))?;

        match row {
            Some(row) => row.try_into(),
            // Nothing matched: either the project is gone or it has moved on to a newer revision.
            None => {
                let current = self.get_project(id).await?;
                Err(StoreError::RevisionMismatch { entity: ENTITY, id: id.to_string(), expected: revision, actual: current.revision })
            }
        }
    }

    async fn delete_project(&self, id: ProjectId) -> Result<(), StoreError> {
        let result = sqlx::query(
            "UPDATE projects
             SET deleted_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                 revision = revision + 1
             WHERE id = ? AND deleted_at IS NULL
               AND NOT EXISTS (SELECT 1 FROM environments WHERE project_id = projects.id AND deleted_at IS NULL)"
        )
        .bind(id.to_string())
        .execute(self.pool())
        .await?;

        if result.rows_affected() == 0 {
            // Either the project is gone, or it still has environments.
            self.get_project(id).await?;
            return Err(StoreError::HasChildren { entity: ENTITY, id: id.to_string(), children: "environments" });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::store;

    fn fields(name: &str, slug: &str) -> ProjectFields {
        ProjectFields { name: name.to_string(), slug: slug.parse().unwrap() }
    }

    #[tokio::test]
    async fn creates_and_gets_a_project() {
        let (_dir, store) = store().await;

        let created = store.create_project(fields("Web", "web")).await.unwrap();
        let fetched = store.get_project(created.id).await.unwrap();

        assert_eq!(fetched, created);
        assert_eq!(created.name, "Web");
        assert_eq!(created.slug.as_str(), "web");
        assert_eq!(created.revision, 1);
        assert_eq!(created.created_at, created.updated_at);
    }

    #[tokio::test]
    async fn rejects_a_taken_slug() {
        let (_dir, store) = store().await;
        store.create_project(fields("Web", "web")).await.unwrap();

        let result = store.create_project(fields("Other", "web")).await;

        assert!(matches!(result, Err(StoreError::Conflict { field: "slug", .. })), "{result:?}");
    }

    #[tokio::test]
    async fn deleting_hides_the_project_and_frees_its_slug() {
        let (_dir, store) = store().await;
        let project = store.create_project(fields("Web", "web")).await.unwrap();

        store.delete_project(project.id).await.unwrap();

        assert!(matches!(store.get_project(project.id).await, Err(StoreError::NotFound { .. })));
        assert!(matches!(store.delete_project(project.id).await, Err(StoreError::NotFound { .. })));
        assert!(store.list_projects(None, 10).await.unwrap().is_empty());
        store.create_project(fields("Web again", "web")).await.unwrap();
    }

    #[tokio::test]
    async fn lists_in_creation_order_with_a_cursor() {
        let (_dir, store) = store().await;
        let mut ids = Vec::new();
        for slug in ["a", "b", "c"] {
            ids.push(store.create_project(fields(slug, slug)).await.unwrap().id);
        }

        let first_page = store.list_projects(None, 2).await.unwrap();
        let second_page = store.list_projects(Some(first_page[1].id), 2).await.unwrap();

        assert_eq!(first_page.iter().map(|p| p.id).collect::<Vec<_>>(), ids[..2]);
        assert_eq!(second_page.iter().map(|p| p.id).collect::<Vec<_>>(), ids[2..]);
    }

    #[tokio::test]
    async fn updates_at_the_current_revision() {
        let (_dir, store) = store().await;
        let project = store.create_project(fields("Web", "web")).await.unwrap();
        // Timestamps have millisecond precision.
        std::thread::sleep(std::time::Duration::from_millis(2));

        let updated = store.update_project(project.id, 1, fields("Website", "website")).await.unwrap();

        assert_eq!(updated.name, "Website");
        assert_eq!(updated.slug.as_str(), "website");
        assert_eq!(updated.revision, 2);
        assert!(updated.updated_at > project.updated_at);
        assert_eq!(updated.created_at, project.created_at);
    }

    #[tokio::test]
    async fn rejects_a_stale_revision() {
        let (_dir, store) = store().await;
        let project = store.create_project(fields("Web", "web")).await.unwrap();
        store.update_project(project.id, 1, fields("Website", "web")).await.unwrap();

        let result = store.update_project(project.id, 1, fields("Stale", "web")).await;

        assert!(
            matches!(result, Err(StoreError::RevisionMismatch { expected: 1, actual: 2, .. })),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn update_reports_missing_projects_and_slug_conflicts() {
        let (_dir, store) = store().await;
        store.create_project(fields("Web", "web")).await.unwrap();
        let api = store.create_project(fields("API", "api")).await.unwrap();

        let missing = store.update_project(ProjectId::new(), 1, fields("X", "x")).await;
        let conflict = store.update_project(api.id, 1, fields("API", "web")).await;

        assert!(matches!(missing, Err(StoreError::NotFound { .. })), "{missing:?}");
        assert!(matches!(conflict, Err(StoreError::Conflict { field: "slug", .. })), "{conflict:?}");
    }
}
