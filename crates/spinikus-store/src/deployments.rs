use async_trait::async_trait;
use spinikus_core::{
    actor::Actor,
    deployment::{Deployment, DeploymentEvent, DeploymentSnapshot, DeploymentStatus, EndReason, NewDeployment, Transition},
    id::{DeploymentEventId, DeploymentId, ServiceId}
};
use sqlx::SqliteConnection;

use crate::{SqliteStore, StoreError, error::parse_column};

const ENTITY: &str = "deployment";

#[async_trait]
pub trait DeploymentRepository: Send + Sync {
    /// Creates a `queued` deployment. An older deployment still queued for the same
    /// service is canceled with `superseded_in_queue`, in the same transaction.
    async fn create_deployment(&self, new: NewDeployment, actor: Actor) -> Result<Deployment, StoreError>;

    async fn get_deployment(&self, id: DeploymentId) -> Result<Deployment, StoreError>;

    /// The service's deployments, newest first, starting before the `before` cursor.
    async fn list_deployments(
        &self,
        service_id: ServiceId,
        before: Option<DeploymentId>,
        limit: u32
    ) -> Result<Vec<Deployment>, StoreError>;

    /// Every status change of the deployment, oldest first.
    async fn list_deployment_events(&self, id: DeploymentId) -> Result<Vec<DeploymentEvent>, StoreError>;

    /// The deployment currently serving the service, if any.
    async fn live_deployment(&self, service_id: ServiceId) -> Result<Option<Deployment>, StoreError>;

    /// Moves the deployment to `to`, recording an event. Going `live` moves the service's
    /// previous live deployment to `draining` in the same transaction.
    async fn transition_deployment(
        &self,
        id: DeploymentId,
        to: DeploymentStatus,
        reason: Option<EndReason>,
        actor: Actor
    ) -> Result<Deployment, StoreError>;

    /// Records the commit and image once the agent knows them. Each can be set once;
    /// setting a different value later is a `Conflict`.
    async fn record_build(
        &self,
        id: DeploymentId,
        commit_sha: Option<String>,
        image: Option<String>
    ) -> Result<Deployment, StoreError>;
}

/// The deployment columns, in the order `DeploymentRow` reads them.
macro_rules! columns {
    () => {
        "id, service_id, trigger_type, strategy, status, reason, rollback_of, snapshot, commit_sha, image,
         created_at, updated_at"
    };
}

#[derive(sqlx::FromRow)]
struct DeploymentRow {
    id: String,
    service_id: String,
    trigger_type: String,
    strategy: String,
    status: String,
    reason: Option<String>,
    rollback_of: Option<String>,
    snapshot: String,
    commit_sha: Option<String>,
    image: Option<String>,
    created_at: String,
    updated_at: String
}

impl TryFrom<DeploymentRow> for Deployment {
    type Error = StoreError;

    fn try_from(row: DeploymentRow) -> Result<Self, Self::Error> {
        Ok(Deployment {
            id: parse_column(ENTITY, "id", &row.id)?,
            service_id: parse_column(ENTITY, "service_id", &row.service_id)?,
            trigger: parse_column(ENTITY, "trigger_type", &row.trigger_type)?,
            strategy: parse_column(ENTITY, "strategy", &row.strategy)?,
            status: parse_column(ENTITY, "status", &row.status)?,
            reason: row.reason.map(|reason| parse_column(ENTITY, "reason", &reason)).transpose()?,
            rollback_of: row.rollback_of.map(|id| parse_column(ENTITY, "rollback_of", &id)).transpose()?,
            snapshot: serde_json::from_str::<DeploymentSnapshot>(&row.snapshot).map_err(|error| {
                StoreError::InvalidRow { entity: ENTITY, column: "snapshot", problem: error.to_string() }
            })?,
            commit_sha: row.commit_sha,
            image: row.image,
            created_at: parse_column(ENTITY, "created_at", &row.created_at)?,
            updated_at: parse_column(ENTITY, "updated_at", &row.updated_at)?
        })
    }
}

#[derive(sqlx::FromRow)]
struct EventRow {
    id: String,
    deployment_id: String,
    from_status: Option<String>,
    to_status: String,
    reason: Option<String>,
    actor: String,
    created_at: String
}

impl TryFrom<EventRow> for DeploymentEvent {
    type Error = StoreError;

    fn try_from(row: EventRow) -> Result<Self, Self::Error> {
        const ENTITY: &str = "deployment event";
        Ok(DeploymentEvent {
            id: parse_column(ENTITY, "id", &row.id)?,
            deployment_id: parse_column(ENTITY, "deployment_id", &row.deployment_id)?,
            from: row.from_status.map(|status| parse_column(ENTITY, "from_status", &status)).transpose()?,
            to: parse_column(ENTITY, "to_status", &row.to_status)?,
            reason: row.reason.map(|reason| parse_column(ENTITY, "reason", &reason)).transpose()?,
            actor: parse_column(ENTITY, "actor", &row.actor)?,
            created_at: parse_column(ENTITY, "created_at", &row.created_at)?
        })
    }
}

fn not_found(id: DeploymentId) -> StoreError {
    StoreError::NotFound { entity: ENTITY, id: id.to_string() }
}

async fn fetch(conn: &mut SqliteConnection, id: DeploymentId) -> Result<Deployment, StoreError> {
    let row: Option<DeploymentRow> = sqlx::query_as(concat!("SELECT ", columns!(), " FROM deployments WHERE id = ?"))
        .bind(id.to_string())
        .fetch_optional(conn)
        .await?;

    row.ok_or_else(|| not_found(id))?.try_into()
}

async fn insert_event(
    conn: &mut SqliteConnection,
    deployment_id: DeploymentId,
    from: Option<DeploymentStatus>,
    to: DeploymentStatus,
    reason: Option<EndReason>,
    actor: Actor
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO deployment_events (id, deployment_id, from_status, to_status, reason, actor)
         VALUES (?, ?, ?, ?, ?, ?)"
    )
    .bind(DeploymentEventId::new().to_string())
    .bind(deployment_id.to_string())
    .bind(from.map(DeploymentStatus::as_str))
    .bind(to.as_str())
    .bind(reason.map(EndReason::as_str))
    .bind(actor.to_string())
    .execute(conn)
    .await?;
    Ok(())
}

/// Applies a checked transition to a deployment and records its event.
async fn apply(conn: &mut SqliteConnection, deployment: &Deployment, transition: Transition, actor: Actor) -> Result<(), StoreError> {
    sqlx::query(
        "UPDATE deployments SET status = ?, reason = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE id = ?"
    )
    .bind(transition.to.as_str())
    .bind(transition.reason.map(EndReason::as_str))
    .bind(deployment.id.to_string())
    .execute(&mut *conn)
    .await
    .map_err(|error| {
        // Only the one-per-service status indexes are unique here.
        StoreError::on_unique_violation(error, |_| StoreError::Conflict {
            entity: ENTITY,
            field: "status",
            value: format!("{} (service {} already has one)", transition.to, deployment.service_id)
        })
    })?;

    insert_event(conn, deployment.id, Some(transition.from), transition.to, transition.reason, actor).await
}

/// The service's deployment in `status`, which the unique indexes make at most one.
async fn find_by_status(
    conn: &mut SqliteConnection,
    service_id: ServiceId,
    status: DeploymentStatus
) -> Result<Option<Deployment>, StoreError> {
    let row: Option<DeploymentRow> =
        sqlx::query_as(concat!("SELECT ", columns!(), " FROM deployments WHERE service_id = ? AND status = ?"))
            .bind(service_id.to_string())
            .bind(status.as_str())
            .fetch_optional(conn)
            .await?;

    row.map(Deployment::try_from).transpose()
}

#[async_trait]
impl DeploymentRepository for SqliteStore {
    async fn create_deployment(&self, new: NewDeployment, actor: Actor) -> Result<Deployment, StoreError> {
        let snapshot = serde_json::to_string(&new.snapshot).map_err(|error| StoreError::InvalidRow {
            entity: ENTITY,
            column: "snapshot",
            problem: error.to_string()
        })?;
        let mut tx = self.begin_write().await?;

        let service_exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM services WHERE id = ? AND deleted_at IS NULL")
            .bind(new.service_id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        if service_exists.is_none() {
            return Err(StoreError::NotFound { entity: "service", id: new.service_id.to_string() });
        }

        if let Some(queued) = find_by_status(&mut tx, new.service_id, DeploymentStatus::Queued).await? {
            let transition = queued.status.transition(DeploymentStatus::Canceled, Some(EndReason::SupersededInQueue))?;
            apply(&mut tx, &queued, transition, actor).await?;
        }

        let id = DeploymentId::new();
        sqlx::query(
            "INSERT INTO deployments
                 (id, service_id, trigger_type, strategy, status, rollback_of, snapshot, commit_sha, image)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(id.to_string())
        .bind(new.service_id.to_string())
        .bind(new.trigger.as_str())
        .bind(new.strategy.as_str())
        .bind(DeploymentStatus::Queued.as_str())
        .bind(new.rollback_of.map(|id| id.to_string()))
        .bind(snapshot)
        .bind(new.commit_sha)
        .bind(new.image)
        .execute(&mut *tx)
        .await?;
        insert_event(&mut tx, id, None, DeploymentStatus::Queued, None, actor).await?;

        let deployment = fetch(&mut tx, id).await?;
        tx.commit().await?;
        Ok(deployment)
    }

    async fn get_deployment(&self, id: DeploymentId) -> Result<Deployment, StoreError> {
        fetch(&mut *self.pool().acquire().await?, id).await
    }

    async fn list_deployments(
        &self,
        service_id: ServiceId,
        before: Option<DeploymentId>,
        limit: u32
    ) -> Result<Vec<Deployment>, StoreError> {
        // `~` sorts after every character used in IDs, so no cursor means "from the newest".
        let rows: Vec<DeploymentRow> = sqlx::query_as(concat!(
            "SELECT ",
            columns!(),
            " FROM deployments WHERE service_id = ? AND id < ? ORDER BY id DESC LIMIT ?"
        ))
        .bind(service_id.to_string())
        .bind(before.map(|id| id.to_string()).unwrap_or_else(|| "~".to_string()))
        .bind(limit)
        .fetch_all(self.pool())
        .await?;

        rows.into_iter().map(Deployment::try_from).collect()
    }

    async fn list_deployment_events(&self, id: DeploymentId) -> Result<Vec<DeploymentEvent>, StoreError> {
        let rows: Vec<EventRow> = sqlx::query_as(
            "SELECT id, deployment_id, from_status, to_status, reason, actor, created_at
             FROM deployment_events WHERE deployment_id = ? ORDER BY id"
        )
        .bind(id.to_string())
        .fetch_all(self.pool())
        .await?;

        if rows.is_empty() {
            // Every deployment has a creation event, so this only happens for unknown IDs.
            return Err(not_found(id));
        }
        rows.into_iter().map(DeploymentEvent::try_from).collect()
    }

    async fn live_deployment(&self, service_id: ServiceId) -> Result<Option<Deployment>, StoreError> {
        find_by_status(&mut *self.pool().acquire().await?, service_id, DeploymentStatus::Live).await
    }

    async fn transition_deployment(
        &self,
        id: DeploymentId,
        to: DeploymentStatus,
        reason: Option<EndReason>,
        actor: Actor
    ) -> Result<Deployment, StoreError> {
        let mut tx = self.begin_write().await?;
        let deployment = fetch(&mut tx, id).await?;
        let transition = deployment.status.transition(to, reason)?;

        if to == DeploymentStatus::Live
            && let Some(previous) = find_by_status(&mut tx, deployment.service_id, DeploymentStatus::Live).await?
        {
            let draining = previous.status.transition(DeploymentStatus::Draining, None)?;
            apply(&mut tx, &previous, draining, actor).await?;
        }
        apply(&mut tx, &deployment, transition, actor).await?;

        let deployment = fetch(&mut tx, id).await?;
        tx.commit().await?;
        Ok(deployment)
    }

    async fn record_build(
        &self,
        id: DeploymentId,
        commit_sha: Option<String>,
        image: Option<String>
    ) -> Result<Deployment, StoreError> {
        let mut tx = self.begin_write().await?;
        let deployment = fetch(&mut tx, id).await?;

        for (field, stored, given) in [("commit_sha", &deployment.commit_sha, &commit_sha), ("image", &deployment.image, &image)] {
            if let (Some(stored), Some(given)) = (stored, given)
                && stored != given
            {
                return Err(StoreError::Conflict { entity: ENTITY, field, value: given.clone() });
            }
        }

        sqlx::query(
            "UPDATE deployments
             SET commit_sha = coalesce(commit_sha, ?), image = coalesce(image, ?),
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?"
        )
        .bind(commit_sha)
        .bind(image)
        .bind(id.to_string())
        .execute(&mut *tx)
        .await?;

        let deployment = fetch(&mut tx, id).await?;
        tx.commit().await?;
        Ok(deployment)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Map;
    use spinikus_core::{
        deployment::{DeploymentStatus::*, DeploymentStrategy, DeploymentTrigger, TransitionError},
        environment::{EnvironmentFields, EnvironmentKind},
        id::UserId,
        project::ProjectFields,
        service::{ServiceFields, ServiceKind},
        source::{ImageSource, SourceSpec}
    };

    use super::*;
    use crate::{EnvironmentRepository, ProjectRepository, ServiceRepository, test_support::store};

    async fn service(store: &SqliteStore) -> ServiceId {
        let project = ProjectFields { name: "Web".to_string(), slug: "web".parse().unwrap() };
        let project_id = store.create_project(project).await.unwrap().id;
        let env = EnvironmentFields { name: "Staging".to_string(), slug: "staging".parse().unwrap() };
        let env_id = store.create_environment(project_id, EnvironmentKind::Staging, env).await.unwrap().id;
        let service = ServiceFields { name: "Web".to_string(), slug: "web".parse().unwrap(), config: Map::new() };
        store.create_service(env_id, ServiceKind::App, service).await.unwrap().id
    }

    fn new_deployment(service_id: ServiceId) -> NewDeployment {
        NewDeployment {
            service_id,
            trigger: DeploymentTrigger::Manual,
            strategy: DeploymentStrategy::BlueGreen,
            rollback_of: None,
            snapshot: DeploymentSnapshot {
                source: SourceSpec::Image(ImageSource { image: "ghcr.io/acme/web:1".to_string() }),
                config: Map::new()
            },
            commit_sha: None,
            image: None
        }
    }

    /// Creates a deployment and walks it to `live`.
    async fn deploy_live(store: &SqliteStore, service_id: ServiceId) -> Deployment {
        let deployment = store.create_deployment(new_deployment(service_id), Actor::System).await.unwrap();
        for status in [Preparing, Starting, Verifying, Live] {
            store.transition_deployment(deployment.id, status, None, Actor::System).await.unwrap();
        }
        store.get_deployment(deployment.id).await.unwrap()
    }

    fn statuses(events: &[DeploymentEvent]) -> Vec<(Option<DeploymentStatus>, DeploymentStatus)> {
        events.iter().map(|event| (event.from, event.to)).collect()
    }

    #[tokio::test]
    async fn creates_a_queued_deployment_with_an_event() {
        let (_dir, store) = store().await;
        let service_id = service(&store).await;
        let user = Actor::User(UserId::new());

        let deployment = store.create_deployment(new_deployment(service_id), user).await.unwrap();
        let events = store.list_deployment_events(deployment.id).await.unwrap();

        assert_eq!(store.get_deployment(deployment.id).await.unwrap(), deployment);
        assert_eq!(deployment.status, Queued);
        assert_eq!(deployment.snapshot, new_deployment(service_id).snapshot);
        assert_eq!(statuses(&events), vec![(None, Queued)]);
        assert_eq!(events[0].actor, user);
    }

    #[tokio::test]
    async fn requires_an_active_service() {
        let (_dir, store) = store().await;

        let result = store.create_deployment(new_deployment(ServiceId::new()), Actor::System).await;

        assert!(matches!(result, Err(StoreError::NotFound { entity: "service", .. })), "{result:?}");
    }

    #[tokio::test]
    async fn a_new_deployment_supersedes_the_queued_one() {
        let (_dir, store) = store().await;
        let service_id = service(&store).await;
        let first = store.create_deployment(new_deployment(service_id), Actor::System).await.unwrap();

        let second = store.create_deployment(new_deployment(service_id), Actor::System).await.unwrap();
        let first = store.get_deployment(first.id).await.unwrap();

        assert_eq!((first.status, first.reason), (Canceled, Some(EndReason::SupersededInQueue)));
        assert_eq!(second.status, Queued);
        assert_eq!(statuses(&store.list_deployment_events(first.id).await.unwrap()), vec![(None, Queued), (Some(Queued), Canceled)]);
    }

    #[tokio::test]
    async fn walks_the_state_machine_and_records_each_step() {
        let (_dir, store) = store().await;
        let service_id = service(&store).await;

        let deployment = deploy_live(&store, service_id).await;
        let events = store.list_deployment_events(deployment.id).await.unwrap();

        assert_eq!(deployment.status, Live);
        assert_eq!(
            statuses(&events),
            vec![(None, Queued), (Some(Queued), Preparing), (Some(Preparing), Starting), (Some(Starting), Verifying), (Some(Verifying), Live)]
        );
        assert_eq!(store.live_deployment(service_id).await.unwrap(), Some(deployment));
    }

    #[tokio::test]
    async fn rejects_transitions_the_state_machine_forbids() {
        let (_dir, store) = store().await;
        let service_id = service(&store).await;
        let deployment = store.create_deployment(new_deployment(service_id), Actor::System).await.unwrap();

        let skip = store.transition_deployment(deployment.id, Live, None, Actor::System).await;
        let no_reason = store.transition_deployment(deployment.id, Canceled, None, Actor::System).await;

        assert!(matches!(skip, Err(StoreError::InvalidTransition(TransitionError::NotAllowed { .. }))), "{skip:?}");
        assert!(matches!(no_reason, Err(StoreError::InvalidTransition(TransitionError::MissingReason { .. }))), "{no_reason:?}");
        assert_eq!(store.list_deployment_events(deployment.id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn going_live_drains_the_previous_deployment() {
        let (_dir, store) = store().await;
        let service_id = service(&store).await;
        let first = deploy_live(&store, service_id).await;

        let second = deploy_live(&store, service_id).await;
        let first = store.get_deployment(first.id).await.unwrap();

        assert_eq!(first.status, Draining);
        assert_eq!(store.live_deployment(service_id).await.unwrap(), Some(second));
        store.transition_deployment(first.id, Superseded, None, Actor::System).await.unwrap();
    }

    #[tokio::test]
    async fn allows_one_deployment_in_progress_per_service() {
        let (_dir, store) = store().await;
        let service_id = service(&store).await;
        let first = store.create_deployment(new_deployment(service_id), Actor::System).await.unwrap();
        store.transition_deployment(first.id, Preparing, None, Actor::System).await.unwrap();
        let second = store.create_deployment(new_deployment(service_id), Actor::System).await.unwrap();

        let blocked = store.transition_deployment(second.id, Preparing, None, Actor::System).await;
        store.transition_deployment(first.id, Failed, Some(EndReason::BuildFailed), Actor::System).await.unwrap();
        let unblocked = store.transition_deployment(second.id, Preparing, None, Actor::System).await;

        assert!(matches!(blocked, Err(StoreError::Conflict { field: "status", .. })), "{blocked:?}");
        assert_eq!(store.get_deployment(second.id).await.unwrap().status, Preparing);
        assert!(unblocked.is_ok());
    }

    #[tokio::test]
    async fn lists_newest_first_with_a_cursor() {
        let (_dir, store) = store().await;
        let service_id = service(&store).await;
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(store.create_deployment(new_deployment(service_id), Actor::System).await.unwrap().id);
        }
        ids.reverse();

        let first_page = store.list_deployments(service_id, None, 2).await.unwrap();
        let second_page = store.list_deployments(service_id, Some(first_page[1].id), 2).await.unwrap();

        assert_eq!(first_page.iter().map(|d| d.id).collect::<Vec<_>>(), ids[..2]);
        assert_eq!(second_page.iter().map(|d| d.id).collect::<Vec<_>>(), ids[2..]);
    }

    #[tokio::test]
    async fn records_build_outputs_once() {
        let (_dir, store) = store().await;
        let service_id = service(&store).await;
        let deployment = store.create_deployment(new_deployment(service_id), Actor::System).await.unwrap();

        let with_commit = store.record_build(deployment.id, Some("abc123".to_string()), None).await.unwrap();
        let with_image = store.record_build(deployment.id, Some("abc123".to_string()), Some("web@sha256:1".to_string())).await.unwrap();
        let changed = store.record_build(deployment.id, Some("def456".to_string()), None).await;

        assert_eq!((with_commit.commit_sha.as_deref(), with_commit.image.as_deref()), (Some("abc123"), None));
        assert_eq!(with_image.image.as_deref(), Some("web@sha256:1"));
        assert!(matches!(changed, Err(StoreError::Conflict { field: "commit_sha", .. })), "{changed:?}");
    }

    #[tokio::test]
    async fn a_rollback_reuses_an_earlier_deployment() {
        let (_dir, store) = store().await;
        let service_id = service(&store).await;
        let first = deploy_live(&store, service_id).await;
        store.record_build(first.id, None, Some("web@sha256:1".to_string())).await.unwrap();
        deploy_live(&store, service_id).await;

        let rollback = NewDeployment {
            trigger: DeploymentTrigger::Rollback,
            rollback_of: Some(first.id),
            image: Some("web@sha256:1".to_string()),
            ..new_deployment(service_id)
        };
        let rollback = store.create_deployment(rollback, Actor::System).await.unwrap();
        for status in [Starting, Verifying, Live] {
            store.transition_deployment(rollback.id, status, None, Actor::System).await.unwrap();
        }

        let live = store.live_deployment(service_id).await.unwrap().unwrap();
        assert_eq!((live.id, live.rollback_of, live.image.as_deref()), (rollback.id, Some(first.id), Some("web@sha256:1")));
    }
}
