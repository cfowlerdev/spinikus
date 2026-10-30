use async_trait::async_trait;
use spinikus_core::{
    id::{ServiceId, SourceId},
    source::{GitSource, ImageSource, Source, SourceSpec},
};

use crate::{SqliteStore, StoreError, error::parse_column};

const ENTITY: &str = "source";

/// Sources are looked up by service, since each service has at most one.
#[async_trait]
pub trait SourceRepository: Send + Sync {
    /// Fails with `NotFound` if the service has no source, or the service is deleted.
    async fn get_source(&self, service_id: ServiceId) -> Result<Source, StoreError>;

    /// Fails with `Conflict` if the service already has a source, and `NotFound` if
    /// the service does not exist or is deleted.
    async fn create_source(&self, service_id: ServiceId, spec: SourceSpec) -> Result<Source, StoreError>;

    /// Replaces the source, which can switch between Git and image, if it is still at `revision`.
    async fn update_source(&self, service_id: ServiceId, revision: i64, spec: SourceSpec) -> Result<Source, StoreError>;

    /// Removes the source. Deployments keep their own snapshot, so nothing else refers to it.
    async fn delete_source(&self, service_id: ServiceId) -> Result<(), StoreError>;
}

#[derive(sqlx::FromRow)]
struct SourceRow {
    id: String,
    service_id: String,
    r#type: String,
    repo_url: Option<String>,
    branch: Option<String>,
    root_directory: Option<String>,
    image: Option<String>,
    revision: i64,
    created_at: String,
    updated_at: String,
}

impl TryFrom<SourceRow> for Source {
    type Error = StoreError;

    fn try_from(row: SourceRow) -> Result<Self, Self::Error> {
        let spec = match (row.r#type.as_str(), row.repo_url, row.branch, row.image) {
            ("git", Some(repo_url), Some(branch), None) => {
                SourceSpec::Git(GitSource { repo_url, branch, root_directory: row.root_directory })
            }
            ("image", None, None, Some(image)) => SourceSpec::Image(ImageSource { image }),
            (other, ..) => {
                return Err(StoreError::InvalidRow {
                    entity: ENTITY,
                    column: "type",
                    problem: format!("columns do not match type `{other}`"),
                });
            }
        };

        Ok(Source {
            id: parse_column(ENTITY, "id", &row.id)?,
            service_id: parse_column(ENTITY, "service_id", &row.service_id)?,
            spec,
            revision: row.revision,
            created_at: parse_column(ENTITY, "created_at", &row.created_at)?,
            updated_at: parse_column(ENTITY, "updated_at", &row.updated_at)?,
        })
    }
}

/// The values for the type-specific columns: type, repo_url, branch, root_directory, image.
fn columns(spec: &SourceSpec) -> (&'static str, Option<&str>, Option<&str>, Option<&str>, Option<&str>) {
    match spec {
        SourceSpec::Git(git) => ("git", Some(&git.repo_url), Some(&git.branch), git.root_directory.as_deref(), None),
        SourceSpec::Image(image) => ("image", None, None, None, Some(&image.image)),
    }
}

fn not_found(service_id: ServiceId) -> StoreError {
    StoreError::NotFound { entity: ENTITY, id: service_id.to_string() }
}

#[async_trait]
impl SourceRepository for SqliteStore {
    async fn get_source(&self, service_id: ServiceId) -> Result<Source, StoreError> {
        let row: Option<SourceRow> = sqlx::query_as(
            "SELECT sources.id, sources.service_id, sources.type, repo_url, branch, root_directory, image,
                    sources.revision, sources.created_at, sources.updated_at
             FROM sources JOIN services ON services.id = sources.service_id
             WHERE sources.service_id = ? AND services.deleted_at IS NULL",
        )
        .bind(service_id.to_string())
        .fetch_optional(self.pool())
        .await?;

        row.ok_or_else(|| not_found(service_id))?.try_into()
    }

    async fn create_source(&self, service_id: ServiceId, spec: SourceSpec) -> Result<Source, StoreError> {
        let (kind, repo_url, branch, root_directory, image) = columns(&spec);
        let row: Option<SourceRow> = sqlx::query_as(
            "INSERT INTO sources (id, service_id, type, repo_url, branch, root_directory, image)
             SELECT ?, id, ?, ?, ?, ?, ? FROM services WHERE id = ? AND deleted_at IS NULL
             RETURNING id, service_id, type, repo_url, branch, root_directory, image, revision, created_at, updated_at",
        )
        .bind(SourceId::new().to_string())
        .bind(kind)
        .bind(repo_url)
        .bind(branch)
        .bind(root_directory)
        .bind(image)
        .bind(service_id.to_string())
        .fetch_optional(self.pool())
        .await
        .map_err(|error| {
            StoreError::on_unique_violation(error, |_| StoreError::Conflict {
                entity: ENTITY,
                field: "service_id",
                value: service_id.to_string(),
            })
        })?;

        match row {
            Some(row) => row.try_into(),
            None => Err(StoreError::NotFound { entity: "service", id: service_id.to_string() }),
        }
    }

    async fn update_source(&self, service_id: ServiceId, revision: i64, spec: SourceSpec) -> Result<Source, StoreError> {
        let (kind, repo_url, branch, root_directory, image) = columns(&spec);
        let row: Option<SourceRow> = sqlx::query_as(
            "UPDATE sources
             SET type = ?, repo_url = ?, branch = ?, root_directory = ?, image = ?, revision = revision + 1,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE service_id = ? AND revision = ?
               AND EXISTS (SELECT 1 FROM services WHERE id = sources.service_id AND deleted_at IS NULL)
             RETURNING id, service_id, type, repo_url, branch, root_directory, image, revision, created_at, updated_at",
        )
        .bind(kind)
        .bind(repo_url)
        .bind(branch)
        .bind(root_directory)
        .bind(image)
        .bind(service_id.to_string())
        .bind(revision)
        .fetch_optional(self.pool())
        .await?;

        match row {
            Some(row) => row.try_into(),
            // Nothing matched: either there is no source or it has moved on to a newer revision.
            None => {
                let current = self.get_source(service_id).await?;
                Err(StoreError::RevisionMismatch {
                    entity: ENTITY,
                    id: service_id.to_string(),
                    expected: revision,
                    actual: current.revision,
                })
            }
        }
    }

    async fn delete_source(&self, service_id: ServiceId) -> Result<(), StoreError> {
        let result = sqlx::query(
            "DELETE FROM sources
             WHERE service_id = ?
               AND EXISTS (SELECT 1 FROM services WHERE id = sources.service_id AND deleted_at IS NULL)",
        )
        .bind(service_id.to_string())
        .execute(self.pool())
        .await?;

        if result.rows_affected() == 0 {
            return Err(not_found(service_id));
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
    use crate::{EnvironmentRepository, ProjectRepository, ServiceRepository, test_support::store};

    fn git(branch: &str, root_directory: Option<&str>) -> SourceSpec {
        SourceSpec::Git(GitSource {
            repo_url: "https://github.com/acme/web.git".to_string(),
            branch: branch.to_string(),
            root_directory: root_directory.map(str::to_string),
        })
    }

    fn image(image: &str) -> SourceSpec {
        SourceSpec::Image(ImageSource { image: image.to_string() })
    }

    async fn service(store: &SqliteStore) -> ServiceId {
        let project = ProjectFields { name: "Web".to_string(), slug: "web".parse().unwrap() };
        let project_id = store.create_project(project).await.unwrap().id;
        let env = EnvironmentFields { name: "Staging".to_string(), slug: "staging".parse().unwrap() };
        let env_id = store.create_environment(project_id, EnvironmentKind::Staging, env).await.unwrap().id;
        let service = ServiceFields { name: "Web".to_string(), slug: "web".parse().unwrap(), config: Map::new() };
        store.create_service(env_id, ServiceKind::App, service).await.unwrap().id
    }

    #[tokio::test]
    async fn creates_and_gets_a_git_source() {
        let (_dir, store) = store().await;
        let service_id = service(&store).await;

        let created = store.create_source(service_id, git("main", Some("apps/web"))).await.unwrap();

        assert_eq!(store.get_source(service_id).await.unwrap(), created);
        assert_eq!(created.service_id, service_id);
        assert_eq!(created.spec, git("main", Some("apps/web")));
        assert_eq!(created.revision, 1);
    }

    #[tokio::test]
    async fn allows_one_source_per_active_service() {
        let (_dir, store) = store().await;
        let service_id = service(&store).await;
        store.create_source(service_id, image("postgres:17")).await.unwrap();

        let second = store.create_source(service_id, image("postgres:18")).await;
        let missing = store.create_source(ServiceId::new(), image("postgres:17")).await;

        assert!(matches!(second, Err(StoreError::Conflict { field: "service_id", .. })), "{second:?}");
        assert!(matches!(missing, Err(StoreError::NotFound { entity: "service", .. })), "{missing:?}");
    }

    #[tokio::test]
    async fn switches_between_git_and_image() {
        let (_dir, store) = store().await;
        let service_id = service(&store).await;
        store.create_source(service_id, git("main", Some("apps/web"))).await.unwrap();

        let as_image = store.update_source(service_id, 1, image("ghcr.io/acme/web:1.4")).await.unwrap();
        let as_git = store.update_source(service_id, 2, git("release", None)).await.unwrap();
        let stale = store.update_source(service_id, 1, image("web:old")).await;

        assert_eq!(as_image.spec, image("ghcr.io/acme/web:1.4"));
        assert_eq!(as_git.spec, git("release", None));
        assert_eq!(as_git.revision, 3);
        assert!(matches!(stale, Err(StoreError::RevisionMismatch { actual: 3, .. })), "{stale:?}");
    }

    #[tokio::test]
    async fn deletes_the_source() {
        let (_dir, store) = store().await;
        let service_id = service(&store).await;
        store.create_source(service_id, image("postgres:17")).await.unwrap();

        store.delete_source(service_id).await.unwrap();

        assert!(matches!(store.get_source(service_id).await, Err(StoreError::NotFound { .. })));
        assert!(matches!(store.delete_source(service_id).await, Err(StoreError::NotFound { .. })));
        store.create_source(service_id, image("postgres:18")).await.unwrap();
    }

    #[tokio::test]
    async fn a_deleted_services_source_is_hidden() {
        let (_dir, store) = store().await;
        let service_id = service(&store).await;
        store.create_source(service_id, image("postgres:17")).await.unwrap();

        store.delete_service(service_id).await.unwrap();

        assert!(matches!(store.get_source(service_id).await, Err(StoreError::NotFound { .. })));
        assert!(matches!(store.update_source(service_id, 1, image("x:1")).await, Err(StoreError::NotFound { .. })));
    }

    #[tokio::test]
    async fn rejects_mismatched_columns() {
        let (_dir, store) = store().await;
        let service_id = service(&store).await;

        let result = sqlx::query("INSERT INTO sources (id, service_id, type, image) VALUES ('src_x', ?, 'git', 'web:1')")
            .bind(service_id.to_string())
            .execute(store.pool())
            .await;

        assert!(result.is_err());
    }
}
