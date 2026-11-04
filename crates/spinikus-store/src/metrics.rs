//! Resource metrics from agents: minute points as they arrive, and hourly means of them.

use async_trait::async_trait;
use jiff::Timestamp;
use spinikus_core::{
    id::{DeploymentId, ServerId, ServiceId},
    metrics::{MetricPoint, Resolution},
};

use crate::{SqliteStore, StoreError, db::format_timestamp, error::parse_column};

const ENTITY: &str = "metric point";

/// What a point measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricSubject {
    Server,
    Container { service_id: ServiceId, deployment_id: DeploymentId },
}

#[async_trait]
pub trait MetricRepository: Send + Sync {
    /// Stores minute points from the server's agent, replacing any for the same subject
    /// and minute.
    async fn record_metric_points(&self, server_id: ServerId, points: &[(MetricSubject, MetricPoint)]) -> Result<(), StoreError>;

    /// The service's points at `resolution` from `since` on, oldest first, summed over its
    /// containers (two run at once while a deployment replaces another).
    async fn service_metric_points(
        &self,
        service_id: ServiceId,
        resolution: Resolution,
        since: Timestamp,
    ) -> Result<Vec<MetricPoint>, StoreError>;

    /// The server's own points at `resolution` from `since` on, oldest first.
    async fn server_metric_points(
        &self,
        server_id: ServerId,
        resolution: Resolution,
        since: Timestamp,
    ) -> Result<Vec<MetricPoint>, StoreError>;

    /// Makes hourly means of the minute points in the hours ending after `from` and at or
    /// before `until` (both on the hour), replacing ones made before.
    async fn roll_up_metric_points(&self, from: Timestamp, until: Timestamp) -> Result<u64, StoreError>;

    /// Deletes points at `resolution` from before `before`, and says how many.
    async fn prune_metric_points(&self, resolution: Resolution, before: Timestamp) -> Result<u64, StoreError>;
}

#[derive(sqlx::FromRow)]
struct PointRow {
    at: String,
    cpu_cores: Option<f64>,
    memory_bytes: Option<i64>,
    memory_limit_bytes: Option<i64>,
    network_rx_rate: Option<f64>,
    network_tx_rate: Option<f64>,
    disk_used_bytes: Option<i64>,
    disk_total_bytes: Option<i64>,
}

impl TryFrom<PointRow> for MetricPoint {
    type Error = StoreError;

    fn try_from(row: PointRow) -> Result<Self, Self::Error> {
        let bytes = |value: Option<i64>| value.map(|value| value.max(0) as u64);
        Ok(MetricPoint {
            at: parse_column(ENTITY, "at", &row.at)?,
            cpu_cores: row.cpu_cores,
            memory_bytes: bytes(row.memory_bytes),
            memory_limit_bytes: bytes(row.memory_limit_bytes),
            network_rx_rate: row.network_rx_rate,
            network_tx_rate: row.network_tx_rate,
            disk_used_bytes: bytes(row.disk_used_bytes),
            disk_total_bytes: bytes(row.disk_total_bytes),
        })
    }
}

/// A byte count as SQLite stores it.
fn bytes(value: Option<u64>) -> Option<i64> {
    value.map(|value| i64::try_from(value).unwrap_or(i64::MAX))
}

/// A rate or core count, without the NaN or negative values the columns refuse.
fn amount(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite()).map(|value| value.max(0.0))
}

#[async_trait]
impl MetricRepository for SqliteStore {
    async fn record_metric_points(&self, server_id: ServerId, points: &[(MetricSubject, MetricPoint)]) -> Result<(), StoreError> {
        let mut tx = self.begin_write().await?;
        for (subject, point) in points {
            let (subject, service) = match subject {
                MetricSubject::Server => (server_id.to_string(), None),
                MetricSubject::Container { service_id, deployment_id } => {
                    (deployment_id.to_string(), Some(service_id.to_string()))
                }
            };
            sqlx::query(
                "INSERT OR REPLACE INTO metric_points (subject, server_id, service_id, resolution, at, cpu_cores,
                     memory_bytes, memory_limit_bytes, network_rx_rate, network_tx_rate, disk_used_bytes, disk_total_bytes)
                 VALUES (?, ?, ?, 'minute', ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(subject)
            .bind(server_id.to_string())
            .bind(service)
            .bind(format_timestamp(point.at))
            .bind(amount(point.cpu_cores))
            .bind(bytes(point.memory_bytes))
            .bind(bytes(point.memory_limit_bytes))
            .bind(amount(point.network_rx_rate))
            .bind(amount(point.network_tx_rate))
            .bind(bytes(point.disk_used_bytes))
            .bind(bytes(point.disk_total_bytes))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn service_metric_points(
        &self,
        service_id: ServiceId,
        resolution: Resolution,
        since: Timestamp,
    ) -> Result<Vec<MetricPoint>, StoreError> {
        let rows: Vec<PointRow> = sqlx::query_as(
            "SELECT at, SUM(cpu_cores) AS cpu_cores, SUM(memory_bytes) AS memory_bytes,
                 SUM(memory_limit_bytes) AS memory_limit_bytes, SUM(network_rx_rate) AS network_rx_rate,
                 SUM(network_tx_rate) AS network_tx_rate, NULL AS disk_used_bytes, NULL AS disk_total_bytes
             FROM metric_points WHERE service_id = ? AND resolution = ? AND at >= ?
             GROUP BY at ORDER BY at",
        )
        .bind(service_id.to_string())
        .bind(resolution.as_str())
        .bind(format_timestamp(since))
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(MetricPoint::try_from).collect()
    }

    async fn server_metric_points(
        &self,
        server_id: ServerId,
        resolution: Resolution,
        since: Timestamp,
    ) -> Result<Vec<MetricPoint>, StoreError> {
        let rows: Vec<PointRow> = sqlx::query_as(
            "SELECT at, cpu_cores, memory_bytes, memory_limit_bytes, network_rx_rate, network_tx_rate,
                 disk_used_bytes, disk_total_bytes
             FROM metric_points WHERE subject = ? AND resolution = ? AND at >= ? ORDER BY at",
        )
        .bind(server_id.to_string())
        .bind(resolution.as_str())
        .bind(format_timestamp(since))
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(MetricPoint::try_from).collect()
    }

    async fn roll_up_metric_points(&self, from: Timestamp, until: Timestamp) -> Result<u64, StoreError> {
        // A minute point ending at 10:01 through 11:00 is in the hour ending at 11:00.
        let result = sqlx::query(
            "INSERT OR REPLACE INTO metric_points (subject, server_id, service_id, resolution, at, cpu_cores,
                 memory_bytes, memory_limit_bytes, network_rx_rate, network_tx_rate, disk_used_bytes, disk_total_bytes)
             SELECT subject, server_id, service_id, 'hour', hour, AVG(cpu_cores), CAST(AVG(memory_bytes) AS INTEGER),
                 MAX(memory_limit_bytes), AVG(network_rx_rate), AVG(network_tx_rate), MAX(disk_used_bytes),
                 MAX(disk_total_bytes)
             FROM (
                 SELECT *, strftime('%Y-%m-%dT%H:00:00.000Z', at, '-1 second', '+1 hour') AS hour
                 FROM metric_points WHERE resolution = 'minute' AND at > ? AND at <= ?
             )
             GROUP BY subject, hour",
        )
        .bind(format_timestamp(from))
        .bind(format_timestamp(until))
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected())
    }

    async fn prune_metric_points(&self, resolution: Resolution, before: Timestamp) -> Result<u64, StoreError> {
        let result = sqlx::query("DELETE FROM metric_points WHERE resolution = ? AND at < ?")
            .bind(resolution.as_str())
            .bind(format_timestamp(before))
            .execute(self.pool())
            .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use spinikus_core::{
        environment::{EnvironmentFields, EnvironmentKind},
        project::ProjectFields,
        server::ServerFields,
        service::{ServiceFields, ServiceKind},
    };

    use super::*;
    use crate::{EnvironmentRepository, ProjectRepository, ServerRepository, ServiceRepository, test_support::store};

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn point(time: &str, cores: f64, memory: u64) -> MetricPoint {
        MetricPoint { at: at(time), cpu_cores: Some(cores), memory_bytes: Some(memory), ..MetricPoint::default() }
    }

    async fn service(store: &SqliteStore) -> ServiceId {
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
        store.create_service(env_id, ServiceKind::App, fields).await.unwrap().id
    }

    #[tokio::test]
    async fn sums_a_services_containers_and_rolls_up_hours() {
        let (_dir, store) = store().await;
        let server = store.create_server(ServerFields { name: "web-1".to_string() }).await.unwrap().id;
        let service_id = service(&store).await;
        let (old, new) = (DeploymentId::new(), DeploymentId::new());
        let container = |deployment_id| MetricSubject::Container { service_id, deployment_id };
        let disk =
            MetricPoint { disk_used_bytes: Some(50), disk_total_bytes: Some(100), ..point("2026-11-04T10:01:00Z", 2.0, 4000) };
        let points = [
            (container(old), point("2026-11-04T10:01:00Z", 0.5, 100)),
            (container(new), point("2026-11-04T10:01:00Z", 0.25, 50)),
            (container(new), point("2026-11-04T10:02:00Z", 0.75, 70)),
            (container(new), point("2026-11-04T11:01:00Z", 1.0, 90)),
            (MetricSubject::Server, disk),
        ];
        store.record_metric_points(server, &points).await.unwrap();
        // Sent again: replaced, not added.
        store.record_metric_points(server, &points[..1]).await.unwrap();

        let since = at("2026-11-04T10:00:00Z");
        let minutes = store.service_metric_points(service_id, Resolution::Minute, since).await.unwrap();
        let summary: Vec<_> = minutes.iter().map(|point| (point.at, point.cpu_cores, point.memory_bytes)).collect();
        assert_eq!(
            summary,
            [
                (at("2026-11-04T10:01:00Z"), Some(0.75), Some(150)),
                (at("2026-11-04T10:02:00Z"), Some(0.75), Some(70)),
                (at("2026-11-04T11:01:00Z"), Some(1.0), Some(90)),
            ]
        );
        let server_points = store.server_metric_points(server, Resolution::Minute, since).await.unwrap();
        assert_eq!(server_points, [disk]);

        // Only the complete hour, 10:00 to 11:00.
        store.roll_up_metric_points(at("2026-11-04T09:00:00Z"), at("2026-11-04T11:00:00Z")).await.unwrap();
        let hours = store.service_metric_points(service_id, Resolution::Hour, since).await.unwrap();
        let summary: Vec<_> = hours.iter().map(|point| (point.at, point.cpu_cores, point.memory_bytes)).collect();
        // Each container's hourly mean, then summed: 0.5 + mean(0.25, 0.75).
        assert_eq!(summary, [(at("2026-11-04T11:00:00Z"), Some(1.0), Some(160))]);
        let server_hours = store.server_metric_points(server, Resolution::Hour, since).await.unwrap();
        assert_eq!(server_hours[0].disk_percent(), Some(50.0));

        assert_eq!(store.prune_metric_points(Resolution::Minute, at("2026-11-04T11:00:00Z")).await.unwrap(), 4);
        assert_eq!(store.service_metric_points(service_id, Resolution::Minute, since).await.unwrap().len(), 1);
        assert_eq!(store.service_metric_points(service_id, Resolution::Hour, since).await.unwrap().len(), 1);
    }
}
