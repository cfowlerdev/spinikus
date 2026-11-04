//! Crash loops of live deployments, as the control plane noticed them.

use async_trait::async_trait;
use jiff::Timestamp;
use spinikus_core::{
    crash::{CrashLoop, CrashReason},
    id::{CrashLoopId, DeploymentId, ServerId, ServiceId},
};

use crate::{SqliteStore, StoreError, db::format_timestamp, error::parse_column};

const ENTITY: &str = "crash loop";

macro_rules! columns {
    () => {
        "id, service_id, deployment_id, server_id, reason, restarts, started_at, updated_at, ended_at"
    };
}

/// A crash loop as it begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewCrashLoop {
    pub service_id: ServiceId,
    pub deployment_id: DeploymentId,
    pub server_id: ServerId,
    pub reason: CrashReason,
    pub restarts: u32,
}

#[async_trait]
pub trait CrashLoopRepository: Send + Sync {
    /// Records a crash loop that has begun. `Conflict` if the deployment has one lasting.
    async fn create_crash_loop(&self, new: NewCrashLoop) -> Result<CrashLoop, StoreError>;

    /// Updates a crash loop's reason and restarts as it goes on.
    async fn update_crash_loop(&self, id: CrashLoopId, reason: CrashReason, restarts: u32) -> Result<CrashLoop, StoreError>;

    /// Ends a crash loop; one that ended already keeps when.
    async fn end_crash_loop(&self, id: CrashLoopId) -> Result<CrashLoop, StoreError>;

    /// Every crash loop that hasn't ended, oldest first.
    async fn list_lasting_crash_loops(&self) -> Result<Vec<CrashLoop>, StoreError>;

    /// The service's crash loops, newest first, starting after `after`.
    async fn list_crash_loops(
        &self,
        service_id: ServiceId,
        after: Option<CrashLoopId>,
        limit: u32,
    ) -> Result<Vec<CrashLoop>, StoreError>;

    /// Deletes crash loops that ended before `before`, and says how many.
    async fn prune_crash_loops(&self, before: Timestamp) -> Result<u64, StoreError>;
}

#[derive(sqlx::FromRow)]
struct CrashLoopRow {
    id: String,
    service_id: String,
    deployment_id: String,
    server_id: String,
    reason: String,
    restarts: i64,
    started_at: String,
    updated_at: String,
    ended_at: Option<String>,
}

impl TryFrom<CrashLoopRow> for CrashLoop {
    type Error = StoreError;

    fn try_from(row: CrashLoopRow) -> Result<Self, Self::Error> {
        Ok(CrashLoop {
            id: parse_column(ENTITY, "id", &row.id)?,
            service_id: parse_column(ENTITY, "service_id", &row.service_id)?,
            deployment_id: parse_column(ENTITY, "deployment_id", &row.deployment_id)?,
            server_id: parse_column(ENTITY, "server_id", &row.server_id)?,
            reason: parse_column(ENTITY, "reason", &row.reason)?,
            restarts: u32::try_from(row.restarts).map_err(|error| StoreError::InvalidRow {
                entity: ENTITY,
                column: "restarts",
                problem: error.to_string(),
            })?,
            started_at: parse_column(ENTITY, "started_at", &row.started_at)?,
            updated_at: parse_column(ENTITY, "updated_at", &row.updated_at)?,
            ended_at: row.ended_at.map(|at| parse_column(ENTITY, "ended_at", &at)).transpose()?,
        })
    }
}

fn not_found(id: CrashLoopId) -> StoreError {
    StoreError::NotFound { entity: ENTITY, id: id.to_string() }
}

#[async_trait]
impl CrashLoopRepository for SqliteStore {
    async fn create_crash_loop(&self, new: NewCrashLoop) -> Result<CrashLoop, StoreError> {
        let deployment = new.deployment_id.to_string();
        let row: CrashLoopRow = sqlx::query_as(concat!(
            "INSERT INTO crash_loops (id, service_id, deployment_id, server_id, reason, restarts)
             VALUES (?, ?, ?, ?, ?, ?) RETURNING ",
            columns!()
        ))
        .bind(CrashLoopId::new().to_string())
        .bind(new.service_id.to_string())
        .bind(&deployment)
        .bind(new.server_id.to_string())
        .bind(new.reason.as_str())
        .bind(new.restarts)
        .fetch_one(self.pool())
        .await
        .map_err(|error| {
            StoreError::on_unique_violation(error, |_| StoreError::Conflict {
                entity: ENTITY,
                field: "deployment_id",
                value: deployment,
            })
        })?;
        row.try_into()
    }

    async fn update_crash_loop(&self, id: CrashLoopId, reason: CrashReason, restarts: u32) -> Result<CrashLoop, StoreError> {
        let row: Option<CrashLoopRow> = sqlx::query_as(concat!(
            "UPDATE crash_loops SET reason = ?, restarts = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ? RETURNING ",
            columns!()
        ))
        .bind(reason.as_str())
        .bind(restarts)
        .bind(id.to_string())
        .fetch_optional(self.pool())
        .await?;
        row.ok_or_else(|| not_found(id))?.try_into()
    }

    async fn end_crash_loop(&self, id: CrashLoopId) -> Result<CrashLoop, StoreError> {
        let row: Option<CrashLoopRow> = sqlx::query_as(concat!(
            "UPDATE crash_loops SET ended_at = COALESCE(ended_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             WHERE id = ? RETURNING ",
            columns!()
        ))
        .bind(id.to_string())
        .fetch_optional(self.pool())
        .await?;
        row.ok_or_else(|| not_found(id))?.try_into()
    }

    async fn list_lasting_crash_loops(&self) -> Result<Vec<CrashLoop>, StoreError> {
        let rows: Vec<CrashLoopRow> =
            sqlx::query_as(concat!("SELECT ", columns!(), " FROM crash_loops WHERE ended_at IS NULL ORDER BY id"))
                .fetch_all(self.pool())
                .await?;
        rows.into_iter().map(CrashLoop::try_from).collect()
    }

    async fn list_crash_loops(
        &self,
        service_id: ServiceId,
        after: Option<CrashLoopId>,
        limit: u32,
    ) -> Result<Vec<CrashLoop>, StoreError> {
        let rows: Vec<CrashLoopRow> = sqlx::query_as(concat!(
            "SELECT ",
            columns!(),
            " FROM crash_loops WHERE service_id = ? AND (? IS NULL OR id < ?) ORDER BY id DESC LIMIT ?"
        ))
        .bind(service_id.to_string())
        .bind(after.map(|id| id.to_string()))
        .bind(after.map(|id| id.to_string()))
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(CrashLoop::try_from).collect()
    }

    async fn prune_crash_loops(&self, before: Timestamp) -> Result<u64, StoreError> {
        let result = sqlx::query("DELETE FROM crash_loops WHERE ended_at IS NOT NULL AND ended_at < ?")
            .bind(format_timestamp(before))
            .execute(self.pool())
            .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use spinikus_core::{
        deployment::{DeploymentSnapshot, DeploymentStrategy, DeploymentTrigger, NewDeployment},
        environment::{EnvironmentFields, EnvironmentKind},
        project::ProjectFields,
        server::ServerFields,
        service::{ServiceFields, ServiceKind},
        source::{ImageSource, SourceSpec},
    };

    use super::*;
    use crate::{
        DeploymentRepository, EnvironmentRepository, ProjectRepository, ServerRepository, ServiceRepository, test_support::store,
    };

    async fn new_loop(store: &SqliteStore) -> NewCrashLoop {
        let server_id = store.create_server(ServerFields { name: "web-1".to_string() }).await.unwrap().id;
        let project = ProjectFields { name: "Web".to_string(), slug: "web".parse().unwrap() };
        let project_id = store.create_project(project).await.unwrap().id;
        let env = EnvironmentFields { name: "Staging".to_string(), slug: "staging".parse().unwrap(), previews: false };
        let env_id = store.create_environment(project_id, EnvironmentKind::Staging, env).await.unwrap().id;
        let fields = ServiceFields {
            name: "Web".to_string(),
            slug: "web".parse().unwrap(),
            config: serde_json::Map::new(),
            server_id: None,
        };
        let service_id = store.create_service(env_id, ServiceKind::App, fields).await.unwrap().id;
        let new = NewDeployment {
            service_id,
            server_id,
            trigger: DeploymentTrigger::Manual,
            strategy: DeploymentStrategy::BlueGreen,
            rollback_of: None,
            snapshot: DeploymentSnapshot {
                source: SourceSpec::Image(ImageSource { image: "nginx".to_string() }),
                config: serde_json::Map::new(),
                from_file: Vec::new(),
            },
            commit_sha: None,
            image: None,
            variables: Vec::new(),
        };
        let deployment_id = store.create_deployment(new, spinikus_core::actor::Actor::System).await.unwrap().id;
        NewCrashLoop { service_id, deployment_id, server_id, reason: CrashReason::Restarting, restarts: 3 }
    }

    #[tokio::test]
    async fn one_lasting_loop_per_deployment_until_it_ends() {
        let (_dir, store) = store().await;
        let new = new_loop(&store).await;

        let first = store.create_crash_loop(new).await.unwrap();
        let second = store.create_crash_loop(new).await;
        let updated = store.update_crash_loop(first.id, CrashReason::NotRunning, 5).await.unwrap();
        let lasting = store.list_lasting_crash_loops().await.unwrap();
        let ended = store.end_crash_loop(first.id).await.unwrap();
        let again = store.end_crash_loop(first.id).await.unwrap();
        let next = store.create_crash_loop(new).await.unwrap();
        let listed = store.list_crash_loops(new.service_id, None, 10).await.unwrap();
        let after = store.list_crash_loops(new.service_id, Some(next.id), 10).await.unwrap();

        assert!(matches!(second, Err(StoreError::Conflict { field: "deployment_id", .. })), "{second:?}");
        assert_eq!((updated.reason, updated.restarts), (CrashReason::NotRunning, 5));
        assert_eq!(lasting, [updated]);
        assert!(ended.ended_at.is_some());
        assert_eq!(again.ended_at, ended.ended_at);
        assert_eq!(listed.iter().map(|crash| crash.id).collect::<Vec<_>>(), [next.id, first.id]);
        assert_eq!(after.iter().map(|crash| crash.id).collect::<Vec<_>>(), [first.id]);

        let pruned = store.prune_crash_loops(Timestamp::now() + jiff::SignedDuration::from_secs(1)).await.unwrap();
        assert_eq!(pruned, 1, "only the ended one");
        assert_eq!(store.list_lasting_crash_loops().await.unwrap(), [next]);
    }
}
