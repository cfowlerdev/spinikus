use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use spinikus_core::{
    actor::Actor,
    deployment::{Deployment, DeploymentEvent, DeploymentSnapshot, DeploymentStatus, EndReason, NewDeployment, Transition},
    id::{DataKeyId, DeploymentEventId, DeploymentId, ServerId, ServiceId, VariableId},
    secret::Secret,
    variable::{ResolvedVariable, ScopeLevel, VariableAvailability, VariableName},
};
use sqlx::SqliteConnection;
use zeroize::Zeroizing;

use crate::{Keyring, Sealed, SqliteStore, StoreError, error::parse_column};

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
        limit: u32,
    ) -> Result<Vec<Deployment>, StoreError>;

    /// Every status change of the deployment, oldest first.
    async fn list_deployment_events(&self, id: DeploymentId) -> Result<Vec<DeploymentEvent>, StoreError>;

    /// The deployment currently serving the service, if any.
    async fn live_deployment(&self, service_id: ServiceId) -> Result<Option<Deployment>, StoreError>;

    /// Every deployment that still has work to do (`queued` through `verifying`, and
    /// `draining`), oldest first.
    async fn list_active_deployments(&self) -> Result<Vec<Deployment>, StoreError>;

    /// Every live deployment, at most one per service, oldest first.
    async fn list_live_deployments(&self) -> Result<Vec<Deployment>, StoreError>;

    /// Every deployment not cleaned up yet, active ones included, oldest first.
    async fn list_uncleaned_deployments(&self) -> Result<Vec<Deployment>, StoreError>;

    /// Deleted services that still have deployments not cleaned up, so the runner can stop
    /// and remove them.
    async fn list_deleted_services_with_deployments(&self) -> Result<Vec<ServiceId>, StoreError>;

    /// The deployments on `server_id` that run `image`, of any service, cleaned or not.
    async fn list_deployments_with_image(&self, server_id: ServerId, image: &str) -> Result<Vec<Deployment>, StoreError>;

    /// Records that a finished deployment's containers and image are gone. Marking it again
    /// keeps the first time; a deployment that has not finished is a `Conflict`.
    async fn mark_deployment_cleaned(&self, id: DeploymentId) -> Result<Deployment, StoreError>;

    /// Moves the deployment to `to`, recording an event with an optional `message` (what
    /// went wrong, for a failure). Going `live` moves the service's previous live
    /// deployment to `draining` in the same transaction.
    async fn transition_deployment(
        &self,
        id: DeploymentId,
        to: DeploymentStatus,
        reason: Option<EndReason>,
        message: Option<&str>,
        actor: Actor,
    ) -> Result<Deployment, StoreError>;

    /// Records the commit and image once the agent knows them. Each can be set once;
    /// setting a different value later is a `Conflict`. A snapshot, given when the build
    /// read `spinikus.json`, replaces the deployment's.
    async fn record_build(
        &self,
        id: DeploymentId,
        commit_sha: Option<String>,
        image: Option<String>,
        snapshot: Option<DeploymentSnapshot>,
    ) -> Result<Deployment, StoreError>;

    /// The variables the deployment was created with, decrypted, secrets included.
    async fn deployment_variables(&self, id: DeploymentId) -> Result<Vec<ResolvedVariable>, StoreError>;
}

/// The deployment columns, in the order `DeploymentRow` reads them.
macro_rules! columns {
    () => {
        "id, service_id, server_id, trigger_type, strategy, status, reason, rollback_of, snapshot, commit_sha, image,
         cleaned_at, created_at, updated_at"
    };
}

#[derive(sqlx::FromRow)]
struct DeploymentRow {
    id: String,
    service_id: String,
    server_id: String,
    trigger_type: String,
    strategy: String,
    status: String,
    reason: Option<String>,
    rollback_of: Option<String>,
    snapshot: String,
    commit_sha: Option<String>,
    image: Option<String>,
    cleaned_at: Option<String>,
    created_at: String,
    updated_at: String,
}

impl TryFrom<DeploymentRow> for Deployment {
    type Error = StoreError;

    fn try_from(row: DeploymentRow) -> Result<Self, Self::Error> {
        Ok(Deployment {
            id: parse_column(ENTITY, "id", &row.id)?,
            service_id: parse_column(ENTITY, "service_id", &row.service_id)?,
            server_id: parse_column(ENTITY, "server_id", &row.server_id)?,
            trigger: parse_column(ENTITY, "trigger_type", &row.trigger_type)?,
            strategy: parse_column(ENTITY, "strategy", &row.strategy)?,
            status: parse_column(ENTITY, "status", &row.status)?,
            reason: row.reason.map(|reason| parse_column(ENTITY, "reason", &reason)).transpose()?,
            rollback_of: row.rollback_of.map(|id| parse_column(ENTITY, "rollback_of", &id)).transpose()?,
            snapshot: serde_json::from_str::<DeploymentSnapshot>(&row.snapshot).map_err(|error| StoreError::InvalidRow {
                entity: ENTITY,
                column: "snapshot",
                problem: error.to_string(),
            })?,
            commit_sha: row.commit_sha,
            image: row.image,
            cleaned_at: row.cleaned_at.map(|at| parse_column(ENTITY, "cleaned_at", &at)).transpose()?,
            created_at: parse_column(ENTITY, "created_at", &row.created_at)?,
            updated_at: parse_column(ENTITY, "updated_at", &row.updated_at)?,
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
    message: Option<String>,
    actor: String,
    created_at: String,
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
            message: row.message,
            actor: parse_column(ENTITY, "actor", &row.actor)?,
            created_at: parse_column(ENTITY, "created_at", &row.created_at)?,
        })
    }
}

/// One resolved variable inside the sealed `deployment_variables` JSON.
#[derive(Serialize, Deserialize)]
struct StoredVariable<'a> {
    name: VariableName,
    #[serde(borrow)]
    value: std::borrow::Cow<'a, str>,
    availability: VariableAvailability,
    secret: bool,
    level: ScopeLevel,
    variable_id: VariableId,
}

fn seal_variables(keyring: &Keyring, id: DeploymentId, variables: &[ResolvedVariable]) -> Sealed {
    let stored: Vec<StoredVariable> = variables
        .iter()
        .map(|variable| StoredVariable {
            name: variable.name.clone(),
            value: variable.value.expose().into(),
            availability: variable.availability,
            secret: variable.secret,
            level: variable.level,
            variable_id: variable.variable_id,
        })
        .collect();
    let json = Zeroizing::new(serde_json::to_vec(&stored).expect("variables serialize to JSON"));
    keyring.seal(&json, id.to_string().as_bytes())
}

fn open_variables(keyring: &Keyring, id: DeploymentId, sealed: &Sealed) -> Result<Vec<ResolvedVariable>, StoreError> {
    let json = keyring.open(sealed, id.to_string().as_bytes())?;
    let stored: Vec<StoredVariable> = serde_json::from_slice(&json).map_err(|error| StoreError::InvalidRow {
        entity: "deployment variables",
        column: "variables",
        problem: error.to_string(),
    })?;
    Ok(stored
        .into_iter()
        .map(|variable| ResolvedVariable {
            name: variable.name,
            value: Secret::new(variable.value.into_owned()),
            availability: variable.availability,
            secret: variable.secret,
            level: variable.level,
            variable_id: variable.variable_id,
        })
        .collect())
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
    message: Option<&str>,
    actor: Actor,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO deployment_events (id, deployment_id, from_status, to_status, reason, message, actor)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(DeploymentEventId::new().to_string())
    .bind(deployment_id.to_string())
    .bind(from.map(DeploymentStatus::as_str))
    .bind(to.as_str())
    .bind(reason.map(EndReason::as_str))
    .bind(message)
    .bind(actor.to_string())
    .execute(conn)
    .await?;
    Ok(())
}

/// Applies a checked transition to a deployment and records its event.
async fn apply(
    conn: &mut SqliteConnection,
    deployment: &Deployment,
    transition: Transition,
    message: Option<&str>,
    actor: Actor,
) -> Result<(), StoreError> {
    sqlx::query(
        "UPDATE deployments SET status = ?, reason = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE id = ?",
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
            value: format!("{} (service {} already has one)", transition.to, deployment.service_id),
        })
    })?;

    insert_event(conn, deployment.id, Some(transition.from), transition.to, transition.reason, message, actor).await
}

/// The service's deployment in `status`, which the unique indexes make at most one.
async fn find_by_status(
    conn: &mut SqliteConnection,
    service_id: ServiceId,
    status: DeploymentStatus,
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
            problem: error.to_string(),
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
            apply(&mut tx, &queued, transition, None, actor).await?;
        }

        let id = DeploymentId::new();
        sqlx::query(
            "INSERT INTO deployments
                 (id, service_id, server_id, trigger_type, strategy, status, rollback_of, snapshot, commit_sha, image)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(new.service_id.to_string())
        .bind(new.server_id.to_string())
        .bind(new.trigger.as_str())
        .bind(new.strategy.as_str())
        .bind(DeploymentStatus::Queued.as_str())
        .bind(new.rollback_of.map(|id| id.to_string()))
        .bind(snapshot)
        .bind(new.commit_sha)
        .bind(new.image)
        .execute(&mut *tx)
        .await?;
        insert_event(&mut tx, id, None, DeploymentStatus::Queued, None, None, actor).await?;

        let sealed = seal_variables(self.keyring(), id, &new.variables);
        sqlx::query("INSERT INTO deployment_variables (deployment_id, variables, data_key_id) VALUES (?, ?, ?)")
            .bind(id.to_string())
            .bind(sealed.ciphertext)
            .bind(sealed.key_id.to_string())
            .execute(&mut *tx)
            .await?;

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
        limit: u32,
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
            "SELECT id, deployment_id, from_status, to_status, reason, message, actor, created_at
             FROM deployment_events WHERE deployment_id = ? ORDER BY id",
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

    async fn list_active_deployments(&self) -> Result<Vec<Deployment>, StoreError> {
        let rows: Vec<DeploymentRow> = sqlx::query_as(concat!(
            "SELECT ",
            columns!(),
            " FROM deployments
             WHERE status IN ('queued', 'preparing', 'releasing', 'starting', 'verifying', 'draining') ORDER BY id"
        ))
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(Deployment::try_from).collect()
    }

    async fn list_live_deployments(&self) -> Result<Vec<Deployment>, StoreError> {
        let rows: Vec<DeploymentRow> =
            sqlx::query_as(concat!("SELECT ", columns!(), " FROM deployments WHERE status = 'live' ORDER BY id"))
                .fetch_all(self.pool())
                .await?;
        rows.into_iter().map(Deployment::try_from).collect()
    }

    async fn list_uncleaned_deployments(&self) -> Result<Vec<Deployment>, StoreError> {
        let rows: Vec<DeploymentRow> =
            sqlx::query_as(concat!("SELECT ", columns!(), " FROM deployments WHERE cleaned_at IS NULL ORDER BY id"))
                .fetch_all(self.pool())
                .await?;
        rows.into_iter().map(Deployment::try_from).collect()
    }

    async fn list_deleted_services_with_deployments(&self) -> Result<Vec<ServiceId>, StoreError> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT deployments.service_id FROM deployments
             JOIN services ON services.id = deployments.service_id
             WHERE deployments.cleaned_at IS NULL AND services.deleted_at IS NOT NULL
             ORDER BY deployments.service_id",
        )
        .fetch_all(self.pool())
        .await?;
        ids.iter().map(|id| parse_column(ENTITY, "service_id", id)).collect()
    }

    async fn list_deployments_with_image(&self, server_id: ServerId, image: &str) -> Result<Vec<Deployment>, StoreError> {
        let rows: Vec<DeploymentRow> =
            sqlx::query_as(concat!("SELECT ", columns!(), " FROM deployments WHERE server_id = ? AND image = ? ORDER BY id"))
                .bind(server_id.to_string())
                .bind(image)
                .fetch_all(self.pool())
                .await?;
        rows.into_iter().map(Deployment::try_from).collect()
    }

    async fn mark_deployment_cleaned(&self, id: DeploymentId) -> Result<Deployment, StoreError> {
        let mut tx = self.begin_write().await?;
        let deployment = fetch(&mut tx, id).await?;
        if !deployment.status.is_terminal() {
            return Err(StoreError::Conflict { entity: ENTITY, field: "status", value: deployment.status.to_string() });
        }
        sqlx::query("UPDATE deployments SET cleaned_at = coalesce(cleaned_at, ?) WHERE id = ?")
            .bind(crate::db::format_timestamp(jiff::Timestamp::now()))
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        let deployment = fetch(&mut tx, id).await?;
        tx.commit().await?;
        Ok(deployment)
    }

    async fn transition_deployment(
        &self,
        id: DeploymentId,
        to: DeploymentStatus,
        reason: Option<EndReason>,
        message: Option<&str>,
        actor: Actor,
    ) -> Result<Deployment, StoreError> {
        let mut tx = self.begin_write().await?;
        let deployment = fetch(&mut tx, id).await?;
        let transition = deployment.status.transition(to, reason)?;

        if to == DeploymentStatus::Live
            && let Some(previous) = find_by_status(&mut tx, deployment.service_id, DeploymentStatus::Live).await?
        {
            let draining = previous.status.transition(DeploymentStatus::Draining, None)?;
            apply(&mut tx, &previous, draining, None, actor).await?;
        }
        apply(&mut tx, &deployment, transition, message, actor).await?;

        let deployment = fetch(&mut tx, id).await?;
        tx.commit().await?;
        Ok(deployment)
    }

    async fn record_build(
        &self,
        id: DeploymentId,
        commit_sha: Option<String>,
        image: Option<String>,
        snapshot: Option<DeploymentSnapshot>,
    ) -> Result<Deployment, StoreError> {
        let mut tx = self.begin_write().await?;
        let deployment = fetch(&mut tx, id).await?;

        for (field, stored, given) in [("commit_sha", &deployment.commit_sha, &commit_sha), ("image", &deployment.image, &image)]
        {
            if let (Some(stored), Some(given)) = (stored, given)
                && stored != given
            {
                return Err(StoreError::Conflict { entity: ENTITY, field, value: given.clone() });
            }
        }

        sqlx::query(
            "UPDATE deployments
             SET commit_sha = coalesce(commit_sha, ?), image = coalesce(image, ?), snapshot = coalesce(?, snapshot),
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?",
        )
        .bind(commit_sha)
        .bind(image)
        .bind(snapshot.map(|snapshot| serde_json::to_string(&snapshot).expect("snapshots serialize")))
        .bind(id.to_string())
        .execute(&mut *tx)
        .await?;

        let deployment = fetch(&mut tx, id).await?;
        tx.commit().await?;
        Ok(deployment)
    }

    async fn deployment_variables(&self, id: DeploymentId) -> Result<Vec<ResolvedVariable>, StoreError> {
        let row: Option<(Vec<u8>, String)> =
            sqlx::query_as("SELECT variables, data_key_id FROM deployment_variables WHERE deployment_id = ?")
                .bind(id.to_string())
                .fetch_optional(self.pool())
                .await?;
        let (ciphertext, key_id) = row.ok_or_else(|| not_found(id))?;
        let key_id: DataKeyId = parse_column("deployment variables", "data_key_id", &key_id)?;

        open_variables(self.keyring(), id, &Sealed { key_id, ciphertext })
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
        server::ServerFields,
        service::{ServiceFields, ServiceKind},
        source::{ImageSource, SourceSpec},
    };

    use spinikus_core::variable::{VariableFields, VariableScope};

    use super::*;
    use crate::{
        EnvironmentRepository, ProjectRepository, ServerRepository, ServiceRepository, VariableRepository, test_support::store,
    };

    async fn service(store: &SqliteStore) -> (ServiceId, ServerId) {
        let server = store.create_server(ServerFields { name: "web-1".to_string() }).await.unwrap().id;
        let project = ProjectFields { name: "Web".to_string(), slug: "web".parse().unwrap() };
        let project_id = store.create_project(project).await.unwrap().id;
        let env = EnvironmentFields { name: "Staging".to_string(), slug: "staging".parse().unwrap(), previews: false };
        let env_id = store.create_environment(project_id, EnvironmentKind::Staging, env).await.unwrap().id;
        let service =
            ServiceFields { name: "Web".to_string(), slug: "web".parse().unwrap(), config: Map::new(), server_id: None };
        (store.create_service(env_id, ServiceKind::App, service).await.unwrap().id, server)
    }

    fn new_deployment(service_id: ServiceId, server_id: ServerId) -> NewDeployment {
        NewDeployment {
            service_id,
            server_id,
            trigger: DeploymentTrigger::Manual,
            strategy: DeploymentStrategy::BlueGreen,
            rollback_of: None,
            snapshot: DeploymentSnapshot {
                source: SourceSpec::Image(ImageSource { image: "ghcr.io/acme/web:1".to_string() }),
                config: Map::new(),
                from_file: Vec::new(),
            },
            commit_sha: None,
            image: None,
            variables: Vec::new(),
        }
    }

    /// Creates a deployment and walks it to `live`.
    async fn deploy_live(store: &SqliteStore, service_id: ServiceId, server_id: ServerId) -> Deployment {
        let deployment = store.create_deployment(new_deployment(service_id, server_id), Actor::System).await.unwrap();
        for status in [Preparing, Starting, Verifying, Live] {
            store.transition_deployment(deployment.id, status, None, None, Actor::System).await.unwrap();
        }
        store.get_deployment(deployment.id).await.unwrap()
    }

    fn statuses(events: &[DeploymentEvent]) -> Vec<(Option<DeploymentStatus>, DeploymentStatus)> {
        events.iter().map(|event| (event.from, event.to)).collect()
    }

    #[tokio::test]
    async fn creates_a_queued_deployment_with_an_event() {
        let (_dir, store) = store().await;
        let (service_id, server_id) = service(&store).await;
        let user = Actor::User(UserId::new());

        let deployment = store.create_deployment(new_deployment(service_id, server_id), user).await.unwrap();
        let events = store.list_deployment_events(deployment.id).await.unwrap();

        assert_eq!(store.get_deployment(deployment.id).await.unwrap(), deployment);
        assert_eq!(deployment.status, Queued);
        assert_eq!(deployment.snapshot, new_deployment(service_id, server_id).snapshot);
        assert_eq!(statuses(&events), vec![(None, Queued)]);
        assert_eq!(events[0].actor, user);
    }

    #[tokio::test]
    async fn records_the_server_and_lists_active_deployments() {
        let (_dir, store) = store().await;
        let (service_id, server_id) = service(&store).await;
        let live = deploy_live(&store, service_id, server_id).await;
        let failed = store.create_deployment(new_deployment(service_id, server_id), Actor::System).await.unwrap();
        store.transition_deployment(failed.id, Preparing, None, None, Actor::System).await.unwrap();
        store
            .transition_deployment(failed.id, Failed, Some(EndReason::BuildFailed), Some("cannot pull"), Actor::Server(server_id))
            .await
            .unwrap();
        let queued = store.create_deployment(new_deployment(service_id, server_id), Actor::System).await.unwrap();
        let second_live = deploy_live(&store, service_id, server_id).await;

        let active: Vec<_> = store.list_active_deployments().await.unwrap().into_iter().map(|d| (d.id, d.status)).collect();
        let events = store.list_deployment_events(failed.id).await.unwrap();

        assert_eq!(queued.server_id, server_id);
        // The first live one is draining now; the queued one was superseded by the second.
        assert_eq!(active, vec![(live.id, Draining)]);
        assert_eq!(second_live.status, Live);
        let live: Vec<_> = store.list_live_deployments().await.unwrap().into_iter().map(|d| d.id).collect();
        assert_eq!(live, vec![second_live.id]);
        let last = events.last().unwrap();
        assert_eq!((last.to, last.reason, last.message.as_deref()), (Failed, Some(EndReason::BuildFailed), Some("cannot pull")));
        assert_eq!(last.actor, Actor::Server(server_id));
        assert_eq!(events[0].message, None);
    }

    #[tokio::test]
    async fn lists_deleted_services_until_their_deployments_are_cleaned_up() {
        let (_dir, store) = store().await;
        let (service_id, server_id) = service(&store).await;
        let deployment = deploy_live(&store, service_id, server_id).await;
        assert!(store.list_deleted_services_with_deployments().await.unwrap().is_empty(), "not deleted");

        store.delete_service(service_id).await.unwrap();
        assert_eq!(store.list_deleted_services_with_deployments().await.unwrap(), [service_id]);
        store.transition_deployment(deployment.id, Draining, None, None, Actor::System).await.unwrap();
        store.transition_deployment(deployment.id, Superseded, None, None, Actor::System).await.unwrap();
        store.mark_deployment_cleaned(deployment.id).await.unwrap();
        assert!(store.list_deleted_services_with_deployments().await.unwrap().is_empty(), "all cleaned up");
    }

    #[tokio::test]
    async fn tracks_what_is_cleaned_up_and_who_shares_an_image() {
        let (_dir, store) = store().await;
        let (service_id, server_id) = service(&store).await;
        let first = deploy_live(&store, service_id, server_id).await;
        store.record_build(first.id, None, Some("sha256:01".to_string()), None).await.unwrap();
        let second = deploy_live(&store, service_id, server_id).await;
        store.record_build(second.id, None, Some("sha256:01".to_string()), None).await.unwrap();
        store.transition_deployment(first.id, Superseded, None, None, Actor::System).await.unwrap();

        let live = store.mark_deployment_cleaned(second.id).await;
        let cleaned = store.mark_deployment_cleaned(first.id).await.unwrap();
        let again = store.mark_deployment_cleaned(first.id).await.unwrap();
        let uncleaned: Vec<_> = store.list_uncleaned_deployments().await.unwrap().into_iter().map(|d| d.id).collect();
        let sharing: Vec<_> =
            store.list_deployments_with_image(server_id, "sha256:01").await.unwrap().into_iter().map(|d| d.id).collect();

        assert!(matches!(live, Err(StoreError::Conflict { field: "status", .. })), "{live:?}");
        assert!(cleaned.cleaned_at.is_some());
        assert_eq!(again.cleaned_at, cleaned.cleaned_at);
        assert_eq!(uncleaned, [second.id]);
        assert_eq!(sharing, [first.id, second.id]);
        assert!(store.list_deployments_with_image(server_id, "sha256:02").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn requires_an_active_service() {
        let (_dir, store) = store().await;

        let (_, server_id) = service(&store).await;

        let result = store.create_deployment(new_deployment(ServiceId::new(), server_id), Actor::System).await;

        assert!(matches!(result, Err(StoreError::NotFound { entity: "service", .. })), "{result:?}");
    }

    #[tokio::test]
    async fn a_new_deployment_supersedes_the_queued_one() {
        let (_dir, store) = store().await;
        let (service_id, server_id) = service(&store).await;
        let first = store.create_deployment(new_deployment(service_id, server_id), Actor::System).await.unwrap();

        let second = store.create_deployment(new_deployment(service_id, server_id), Actor::System).await.unwrap();
        let first = store.get_deployment(first.id).await.unwrap();

        assert_eq!((first.status, first.reason), (Canceled, Some(EndReason::SupersededInQueue)));
        assert_eq!(second.status, Queued);
        assert_eq!(
            statuses(&store.list_deployment_events(first.id).await.unwrap()),
            vec![(None, Queued), (Some(Queued), Canceled)]
        );
    }

    #[tokio::test]
    async fn walks_the_state_machine_and_records_each_step() {
        let (_dir, store) = store().await;
        let (service_id, server_id) = service(&store).await;

        let deployment = deploy_live(&store, service_id, server_id).await;
        let events = store.list_deployment_events(deployment.id).await.unwrap();

        assert_eq!(deployment.status, Live);
        assert_eq!(
            statuses(&events),
            vec![
                (None, Queued),
                (Some(Queued), Preparing),
                (Some(Preparing), Starting),
                (Some(Starting), Verifying),
                (Some(Verifying), Live)
            ]
        );
        assert_eq!(store.live_deployment(service_id).await.unwrap(), Some(deployment));
    }

    #[tokio::test]
    async fn rejects_transitions_the_state_machine_forbids() {
        let (_dir, store) = store().await;
        let (service_id, server_id) = service(&store).await;
        let deployment = store.create_deployment(new_deployment(service_id, server_id), Actor::System).await.unwrap();

        let skip = store.transition_deployment(deployment.id, Live, None, None, Actor::System).await;
        let no_reason = store.transition_deployment(deployment.id, Canceled, None, None, Actor::System).await;

        assert!(matches!(skip, Err(StoreError::InvalidTransition(TransitionError::NotAllowed { .. }))), "{skip:?}");
        assert!(matches!(no_reason, Err(StoreError::InvalidTransition(TransitionError::MissingReason { .. }))), "{no_reason:?}");
        assert_eq!(store.list_deployment_events(deployment.id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn going_live_drains_the_previous_deployment() {
        let (_dir, store) = store().await;
        let (service_id, server_id) = service(&store).await;
        let first = deploy_live(&store, service_id, server_id).await;

        let second = deploy_live(&store, service_id, server_id).await;
        let first = store.get_deployment(first.id).await.unwrap();

        assert_eq!(first.status, Draining);
        assert_eq!(store.live_deployment(service_id).await.unwrap(), Some(second));
        store.transition_deployment(first.id, Superseded, None, None, Actor::System).await.unwrap();
    }

    #[tokio::test]
    async fn allows_one_deployment_in_progress_per_service() {
        let (_dir, store) = store().await;
        let (service_id, server_id) = service(&store).await;
        let first = store.create_deployment(new_deployment(service_id, server_id), Actor::System).await.unwrap();
        store.transition_deployment(first.id, Preparing, None, None, Actor::System).await.unwrap();
        let second = store.create_deployment(new_deployment(service_id, server_id), Actor::System).await.unwrap();

        let blocked = store.transition_deployment(second.id, Preparing, None, None, Actor::System).await;
        store.transition_deployment(first.id, Failed, Some(EndReason::BuildFailed), None, Actor::System).await.unwrap();
        let unblocked = store.transition_deployment(second.id, Preparing, None, None, Actor::System).await;

        assert!(matches!(blocked, Err(StoreError::Conflict { field: "status", .. })), "{blocked:?}");
        assert_eq!(store.get_deployment(second.id).await.unwrap().status, Preparing);
        assert!(unblocked.is_ok());
    }

    #[tokio::test]
    async fn lists_newest_first_with_a_cursor() {
        let (_dir, store) = store().await;
        let (service_id, server_id) = service(&store).await;
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(store.create_deployment(new_deployment(service_id, server_id), Actor::System).await.unwrap().id);
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
        let (service_id, server_id) = service(&store).await;
        let deployment = store.create_deployment(new_deployment(service_id, server_id), Actor::System).await.unwrap();

        let with_commit = store.record_build(deployment.id, Some("abc123".to_string()), None, None).await.unwrap();
        let mut snapshot = deployment.snapshot.clone();
        snapshot.config.insert("runtime".to_string(), serde_json::json!({ "port": 3000 }));
        snapshot.from_file = vec!["runtime.port".to_string()];
        let with_image = store
            .record_build(deployment.id, Some("abc123".to_string()), Some("web@sha256:1".to_string()), Some(snapshot.clone()))
            .await
            .unwrap();
        let changed = store.record_build(deployment.id, Some("def456".to_string()), None, None).await;

        assert_eq!((with_commit.commit_sha.as_deref(), with_commit.image.as_deref()), (Some("abc123"), None));
        assert_eq!(with_commit.snapshot, deployment.snapshot);
        assert_eq!(with_image.image.as_deref(), Some("web@sha256:1"));
        assert_eq!(with_image.snapshot, snapshot);
        assert!(matches!(changed, Err(StoreError::Conflict { field: "commit_sha", .. })), "{changed:?}");
    }

    #[tokio::test]
    async fn a_rollback_reuses_an_earlier_deployment() {
        let (_dir, store) = store().await;
        let (service_id, server_id) = service(&store).await;
        let first = deploy_live(&store, service_id, server_id).await;
        store.record_build(first.id, None, Some("web@sha256:1".to_string()), None).await.unwrap();
        deploy_live(&store, service_id, server_id).await;

        let rollback = NewDeployment {
            trigger: DeploymentTrigger::Rollback,
            rollback_of: Some(first.id),
            image: Some("web@sha256:1".to_string()),
            ..new_deployment(service_id, server_id)
        };
        let rollback = store.create_deployment(rollback, Actor::System).await.unwrap();
        for status in [Starting, Verifying, Live] {
            store.transition_deployment(rollback.id, status, None, None, Actor::System).await.unwrap();
        }

        let live = store.live_deployment(service_id).await.unwrap().unwrap();
        assert_eq!((live.id, live.rollback_of, live.image.as_deref()), (rollback.id, Some(first.id), Some("web@sha256:1")));
    }

    #[tokio::test]
    async fn keeps_the_variables_resolved_at_creation() {
        let (_dir, store) = store().await;
        let (service_id, server_id) = service(&store).await;
        let scope = VariableScope::Service(service_id);
        let fields =
            |name: &str, secret| VariableFields { name: name.parse().unwrap(), availability: VariableAvailability::Both, secret };
        let api_key = store.create_variable(scope, fields("API_KEY", true), Secret::new("hunter\"2")).await.unwrap();
        store.create_variable(scope, fields("PORT", false), Secret::new("8080")).await.unwrap();

        let resolved = store.resolve_service_variables(service_id).await.unwrap();
        let deployment = store
            .create_deployment(
                NewDeployment { variables: resolved.clone(), ..new_deployment(service_id, server_id) },
                Actor::System,
            )
            .await
            .unwrap();
        // Later edits do not reach the existing deployment.
        store.update_variable(api_key.id, 1, fields("API_KEY", true), Some(Secret::new("rotated"))).await.unwrap();

        let stored = store.deployment_variables(deployment.id).await.unwrap();
        assert_eq!(stored, resolved);
        assert_eq!(stored[0].value.expose(), "hunter\"2");
        let ciphertext: Vec<u8> =
            sqlx::query_scalar("SELECT variables FROM deployment_variables").fetch_one(store.pool()).await.unwrap();
        assert!(!ciphertext.windows(4).any(|window| window == b"8080"));
    }

    #[tokio::test]
    async fn deployment_variables_of_an_unknown_deployment() {
        let (_dir, store) = store().await;

        let result = store.deployment_variables(DeploymentId::new()).await;

        assert!(matches!(result, Err(StoreError::NotFound { .. })), "{result:?}");
    }
}
