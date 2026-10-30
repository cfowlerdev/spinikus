use async_trait::async_trait;
use spinikus_core::{
    id::{DeploymentId, InstanceId, ServerId, ServiceId},
    server::{Instance, ObservedInstance}
};
use sqlx::SqliteConnection;

use crate::{SqliteStore, StoreError, db::format_timestamp, error::parse_column};

const ENTITY: &str = "instance";

/// Instances are observed state: only agent reports write them.
#[async_trait]
pub trait InstanceRepository: Send + Sync {
    /// Applies a full `StateReport`: the server's instances become exactly `instances`.
    /// Containers seen before keep their instance ID. Fails with `NotFound` for revoked servers.
    async fn replace_server_instances(
        &self,
        server_id: ServerId,
        instances: Vec<ObservedInstance>
    ) -> Result<Vec<Instance>, StoreError>;

    /// Applies one changed container from a `StateReport` delta.
    async fn upsert_instance(&self, server_id: ServerId, instance: ObservedInstance) -> Result<Instance, StoreError>;

    /// Applies a removed container from a `StateReport` delta.
    async fn remove_instance(&self, server_id: ServerId, container_id: &str) -> Result<(), StoreError>;

    async fn list_server_instances(&self, server_id: ServerId) -> Result<Vec<Instance>, StoreError>;

    async fn list_service_instances(&self, service_id: ServiceId) -> Result<Vec<Instance>, StoreError>;

    async fn list_deployment_instances(&self, deployment_id: DeploymentId) -> Result<Vec<Instance>, StoreError>;
}

macro_rules! columns {
    () => {
        "id, server_id, service_id, deployment_id, container_id, status, health, restart_count, started_at, reported_at"
    };
}

#[derive(sqlx::FromRow)]
struct InstanceRow {
    id: String,
    server_id: String,
    service_id: String,
    deployment_id: String,
    container_id: String,
    status: String,
    health: String,
    restart_count: u32,
    started_at: Option<String>,
    reported_at: String
}

impl TryFrom<InstanceRow> for Instance {
    type Error = StoreError;

    fn try_from(row: InstanceRow) -> Result<Self, Self::Error> {
        Ok(Instance {
            id: parse_column(ENTITY, "id", &row.id)?,
            server_id: parse_column(ENTITY, "server_id", &row.server_id)?,
            observed: ObservedInstance {
                service_id: parse_column(ENTITY, "service_id", &row.service_id)?,
                deployment_id: parse_column(ENTITY, "deployment_id", &row.deployment_id)?,
                container_id: row.container_id,
                status: parse_column(ENTITY, "status", &row.status)?,
                health: parse_column(ENTITY, "health", &row.health)?,
                restart_count: row.restart_count,
                started_at: row.started_at.map(|at| parse_column(ENTITY, "started_at", &at)).transpose()?
            },
            reported_at: parse_column(ENTITY, "reported_at", &row.reported_at)?
        })
    }
}

async fn check_server_active(conn: &mut SqliteConnection, server_id: ServerId) -> Result<(), StoreError> {
    let active: Option<i64> = sqlx::query_scalar("SELECT 1 FROM servers WHERE id = ? AND revoked_at IS NULL")
        .bind(server_id.to_string())
        .fetch_optional(conn)
        .await?;

    match active {
        Some(_) => Ok(()),
        None => Err(StoreError::NotFound { entity: "server", id: server_id.to_string() })
    }
}

async fn upsert(conn: &mut SqliteConnection, server_id: ServerId, instance: &ObservedInstance) -> Result<Instance, StoreError> {
    let row: InstanceRow = sqlx::query_as(concat!(
        "INSERT INTO instances
             (id, server_id, service_id, deployment_id, container_id, status, health, restart_count, started_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (server_id, container_id) DO UPDATE SET
             service_id = excluded.service_id, deployment_id = excluded.deployment_id, status = excluded.status,
             health = excluded.health, restart_count = excluded.restart_count, started_at = excluded.started_at,
             reported_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         RETURNING ",
        columns!()
    ))
    .bind(InstanceId::new().to_string())
    .bind(server_id.to_string())
    .bind(instance.service_id.to_string())
    .bind(instance.deployment_id.to_string())
    .bind(&instance.container_id)
    .bind(instance.status.as_str())
    .bind(instance.health.as_str())
    .bind(instance.restart_count)
    .bind(instance.started_at.map(format_timestamp))
    .fetch_one(conn)
    .await?;

    row.try_into()
}

async fn list_where(store: &SqliteStore, query: &'static str, value: String) -> Result<Vec<Instance>, StoreError> {
    let rows: Vec<InstanceRow> = sqlx::query_as(query).bind(value).fetch_all(store.pool()).await?;
    rows.into_iter().map(Instance::try_from).collect()
}

#[async_trait]
impl InstanceRepository for SqliteStore {
    async fn replace_server_instances(
        &self,
        server_id: ServerId,
        instances: Vec<ObservedInstance>
    ) -> Result<Vec<Instance>, StoreError> {
        let container_ids: Vec<&str> = instances.iter().map(|instance| instance.container_id.as_str()).collect();
        let container_ids = serde_json::to_string(&container_ids).expect("a list of strings serializes");
        let mut tx = self.begin_write().await?;
        check_server_active(&mut tx, server_id).await?;

        sqlx::query(
            "DELETE FROM instances WHERE server_id = ? AND container_id NOT IN (SELECT value FROM json_each(?))"
        )
        .bind(server_id.to_string())
        .bind(container_ids)
        .execute(&mut *tx)
        .await?;

        let mut stored = Vec::with_capacity(instances.len());
        for instance in &instances {
            stored.push(upsert(&mut tx, server_id, instance).await?);
        }

        tx.commit().await?;
        Ok(stored)
    }

    async fn upsert_instance(&self, server_id: ServerId, instance: ObservedInstance) -> Result<Instance, StoreError> {
        let mut tx = self.begin_write().await?;
        check_server_active(&mut tx, server_id).await?;
        let stored = upsert(&mut tx, server_id, &instance).await?;
        tx.commit().await?;
        Ok(stored)
    }

    async fn remove_instance(&self, server_id: ServerId, container_id: &str) -> Result<(), StoreError> {
        let result = sqlx::query("DELETE FROM instances WHERE server_id = ? AND container_id = ?")
            .bind(server_id.to_string())
            .bind(container_id)
            .execute(self.pool())
            .await?;

        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound { entity: ENTITY, id: format!("{server_id}/{container_id}") });
        }
        Ok(())
    }

    async fn list_server_instances(&self, server_id: ServerId) -> Result<Vec<Instance>, StoreError> {
        list_where(self, concat!("SELECT ", columns!(), " FROM instances WHERE server_id = ? ORDER BY id"), server_id.to_string())
            .await
    }

    async fn list_service_instances(&self, service_id: ServiceId) -> Result<Vec<Instance>, StoreError> {
        list_where(self, concat!("SELECT ", columns!(), " FROM instances WHERE service_id = ? ORDER BY id"), service_id.to_string())
            .await
    }

    async fn list_deployment_instances(&self, deployment_id: DeploymentId) -> Result<Vec<Instance>, StoreError> {
        let query = concat!("SELECT ", columns!(), " FROM instances WHERE deployment_id = ? ORDER BY id");
        list_where(self, query, deployment_id.to_string()).await
    }
}

#[cfg(test)]
mod tests {
    use spinikus_core::server::{InstanceHealth, InstanceStatus, ServerFields};

    use super::*;
    use crate::{ServerRepository, test_support::store};

    async fn server(store: &SqliteStore) -> ServerId {
        store.create_server(ServerFields { name: "web-1".to_string() }).await.unwrap().id
    }

    fn observed(container_id: &str, service_id: ServiceId, deployment_id: DeploymentId) -> ObservedInstance {
        ObservedInstance {
            service_id,
            deployment_id,
            container_id: container_id.to_string(),
            status: InstanceStatus::Running,
            health: InstanceHealth::Healthy,
            restart_count: 0,
            started_at: Some("2026-10-30T09:00:00.000Z".parse().unwrap())
        }
    }

    fn container_ids(instances: &[Instance]) -> Vec<&str> {
        instances.iter().map(|instance| instance.observed.container_id.as_str()).collect()
    }

    #[tokio::test]
    async fn a_full_report_replaces_the_servers_instances() {
        let (_dir, store) = store().await;
        let server_id = server(&store).await;
        let (service, deployment) = (ServiceId::new(), DeploymentId::new());
        let first = store
            .replace_server_instances(server_id, vec![observed("a", service, deployment), observed("b", service, deployment)])
            .await
            .unwrap();

        let mut b_restarted = observed("b", service, deployment);
        b_restarted.restart_count = 3;
        let second = store
            .replace_server_instances(server_id, vec![b_restarted, observed("c", service, deployment)])
            .await
            .unwrap();

        assert_eq!(container_ids(&store.list_server_instances(server_id).await.unwrap()), vec!["b", "c"]);
        assert_eq!(second[0].id, first[1].id, "a container seen before keeps its instance ID");
        assert_eq!(second[0].observed.restart_count, 3);
        let stored_started_at: String = sqlx::query_scalar("SELECT started_at FROM instances WHERE container_id = 'c'")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(stored_started_at, "2026-10-30T09:00:00.000Z");
    }

    #[tokio::test]
    async fn an_empty_report_clears_the_server() {
        let (_dir, store) = store().await;
        let server_id = server(&store).await;
        store.replace_server_instances(server_id, vec![observed("a", ServiceId::new(), DeploymentId::new())]).await.unwrap();

        store.replace_server_instances(server_id, vec![]).await.unwrap();

        assert!(store.list_server_instances(server_id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn applies_deltas() {
        let (_dir, store) = store().await;
        let server_id = server(&store).await;
        let (service, deployment) = (ServiceId::new(), DeploymentId::new());
        let created = store.upsert_instance(server_id, observed("a", service, deployment)).await.unwrap();

        let mut unhealthy = observed("a", service, deployment);
        unhealthy.health = InstanceHealth::Unhealthy;
        let updated = store.upsert_instance(server_id, unhealthy).await.unwrap();
        store.remove_instance(server_id, "a").await.unwrap();

        assert_eq!(updated.id, created.id);
        assert_eq!(updated.observed.health, InstanceHealth::Unhealthy);
        assert!(store.list_server_instances(server_id).await.unwrap().is_empty());
        assert!(matches!(store.remove_instance(server_id, "a").await, Err(StoreError::NotFound { .. })));
    }

    #[tokio::test]
    async fn lists_by_service_and_deployment_across_servers() {
        let (_dir, store) = store().await;
        let (one, two) = (server(&store).await, server(&store).await);
        let service = ServiceId::new();
        let (old, new) = (DeploymentId::new(), DeploymentId::new());
        store.replace_server_instances(one, vec![observed("a", service, old), observed("b", service, new)]).await.unwrap();
        store.replace_server_instances(two, vec![observed("a", service, new), observed("x", ServiceId::new(), DeploymentId::new())]).await.unwrap();

        let by_service = store.list_service_instances(service).await.unwrap();
        let by_deployment = store.list_deployment_instances(new).await.unwrap();

        assert_eq!(by_service.len(), 3);
        assert_eq!(by_deployment.iter().map(|i| (i.server_id, i.observed.container_id.as_str())).collect::<Vec<_>>(), vec![(one, "b"), (two, "a")]);
    }

    #[tokio::test]
    async fn revoked_servers_cannot_report() {
        let (_dir, store) = store().await;
        let server_id = server(&store).await;
        store.revoke_server(server_id).await.unwrap();

        let full = store.replace_server_instances(server_id, vec![]).await;
        let delta = store.upsert_instance(server_id, observed("a", ServiceId::new(), DeploymentId::new())).await;

        assert!(matches!(full, Err(StoreError::NotFound { entity: "server", .. })), "{full:?}");
        assert!(matches!(delta, Err(StoreError::NotFound { entity: "server", .. })), "{delta:?}");
    }
}
