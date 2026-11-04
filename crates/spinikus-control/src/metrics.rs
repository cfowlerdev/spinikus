//! Resource metrics: agents push a point per container and per server each minute
//! ([`Control::record_stats`], from the agent listener's `PushStats`), which are stored
//! against the container's deployment and service; [`Control::run_metrics`] rolls them up
//! into hours and deletes old ones. A server whose disk fills up gets a notification.

use std::{collections::HashSet, future::Future, sync::Mutex, time::Duration};

use jiff::{SignedDuration, Timestamp};
use spinikus_core::{
    id::{ServerId, ServiceId},
    membership::Role,
    metrics::{self, HOURS_KEPT_FOR, MINUTES_KEPT_FOR, MetricPoint, MetricRange, Metrics, Resolution, disk_low, format_bytes},
    notification::{Message, NotificationEvent},
};
use spinikus_store::{InstanceRepository, MetricRepository, MetricSubject, ServerRepository};

use crate::{Control, ControlError, Principal, backups::require_instance_admin};

/// How often old points are rolled up and deleted.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// Points from further ahead than this are refused, as a clock gone wrong.
const MAX_CLOCK_SKEW: SignedDuration = SignedDuration::from_mins(5);
/// How many points one `PushStats` call may carry.
pub const MAX_POINTS_PER_PUSH: usize = 10_000;

/// What the control plane remembers between pushes.
#[derive(Default)]
pub(crate) struct MetricsState {
    /// Servers reported low on disk, until they have room again.
    low_disk: Mutex<HashSet<ServerId>>,
}

impl Control {
    /// Stores what the server's agent measured: `(container ID, point)` pairs, with an empty
    /// container ID for the server itself. Containers that aren't the server's managed ones
    /// are skipped, as are points from the future or too old to keep.
    pub async fn record_stats(&self, server_id: ServerId, points: Vec<(String, MetricPoint)>) -> Result<(), ControlError> {
        let now = Timestamp::now();
        let instances = self.store().list_server_instances(server_id).await?;
        let mut kept = Vec::with_capacity(points.len());
        let mut newest_disk: Option<MetricPoint> = None;
        for (container_id, mut point) in points.into_iter().take(MAX_POINTS_PER_PUSH) {
            point.at = metrics::period_end(point.at, SignedDuration::from_mins(1));
            if point.at > now + MAX_CLOCK_SKEW || point.at < now - MINUTES_KEPT_FOR {
                continue;
            }
            let subject = match container_id.as_str() {
                "" => {
                    point.cpu_cores = point.cpu_cores.filter(|cores| cores.is_finite());
                    if point.disk_percent().is_some() && newest_disk.is_none_or(|newest| newest.at < point.at) {
                        newest_disk = Some(point);
                    }
                    MetricSubject::Server
                }
                container => {
                    // A container's network and disk are the host's, so not its own.
                    point.disk_used_bytes = None;
                    point.disk_total_bytes = None;
                    let Some(instance) = instances.iter().find(|instance| instance.observed.container_id == container) else {
                        continue;
                    };
                    MetricSubject::Container {
                        service_id: instance.observed.service_id,
                        deployment_id: instance.observed.deployment_id,
                    }
                }
            };
            kept.push((subject, point));
        }
        self.store().record_metric_points(server_id, &kept).await?;
        if let Some(point) = newest_disk {
            self.watch_disk(server_id, &point).await;
        }
        Ok(())
    }

    /// Notifies once when the server's disk fills up past the line, and again only after it
    /// had room for a while.
    async fn watch_disk(&self, server_id: ServerId, point: &MetricPoint) {
        let Some(percent) = point.disk_percent() else { return };
        let newly_low = {
            let mut low = self.metrics.low_disk.lock().expect("not poisoned");
            let was_low = low.contains(&server_id);
            match disk_low(percent, was_low) {
                true => low.insert(server_id),
                false => {
                    low.remove(&server_id);
                    false
                }
            }
        };
        if !newly_low {
            return;
        }
        let name = match self.store().get_server(server_id).await {
            Ok(server) => server.name,
            Err(_) => server_id.to_string(),
        };
        let (used, total) = (point.disk_used_bytes.unwrap_or(0), point.disk_total_bytes.unwrap_or(0));
        self.notify(Message {
            event: NotificationEvent::ServerDiskLow,
            title: format!("Server {name} is low on disk"),
            text: format!(
                "Its disk is {percent:.0}% full ({} of {}). Images, build cache, volumes and logs are kept there; \
                 remove what isn't needed, or make the disk bigger, before deployments and databases fail.",
                format_bytes(used),
                format_bytes(total),
            ),
            path: Some(format!("/settings/servers/{server_id}")),
            details: [("server_id".to_string(), server_id.to_string())].into(),
        })
        .await;
    }

    /// Servers whose disk is low now, as of their latest points.
    pub fn servers_low_on_disk(&self) -> HashSet<ServerId> {
        self.metrics.low_disk.lock().expect("not poisoned").clone()
    }

    /// The service's resource use (the sum of its containers) over `range`. Viewers may
    /// read it.
    pub async fn service_metrics(
        &self,
        principal: &Principal,
        service_id: ServiceId,
        range: MetricRange,
    ) -> Result<Metrics, ControlError> {
        self.service_with_access(principal, service_id, Role::Viewer).await?;
        let until = Timestamp::now();
        let since = until - range.duration();
        let points = self.store().service_metric_points(service_id, range.resolution(), since).await?;
        Ok(graph(range, points, since, until))
    }

    /// The server's own resource use over `range`, for instance admins.
    pub async fn server_metrics(
        &self,
        principal: &Principal,
        server_id: ServerId,
        range: MetricRange,
    ) -> Result<Metrics, ControlError> {
        require_instance_admin(principal)?;
        self.store().get_server(server_id).await?;
        let until = Timestamp::now();
        let since = until - range.duration();
        let points = self.store().server_metric_points(server_id, range.resolution(), since).await?;
        Ok(graph(range, points, since, until))
    }

    /// Rolls up and deletes old points every few minutes until `shutdown`. Spawned by
    /// `server run`.
    pub async fn run_metrics(&self, shutdown: impl Future<Output = ()>) {
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            if let Err(error) = self.maintain_metrics_at(Timestamp::now()).await {
                tracing::error!(%error, "failed to roll up or delete old metrics");
            }
            tokio::select! {
                () = shutdown.as_mut() => return,
                () = tokio::time::sleep(MAINTENANCE_INTERVAL) => {}
            }
        }
    }

    /// Rolls the complete hours of the last day up (again, which replaces them) and deletes
    /// points older than they are kept. Tests call it directly.
    pub async fn maintain_metrics_at(&self, now: Timestamp) -> Result<(), ControlError> {
        let hour = SignedDuration::from_hours(1);
        let mut last_hour_end = metrics::period_end(now, hour);
        if last_hour_end > now {
            last_hour_end -= hour;
        }
        self.store().roll_up_metric_points(last_hour_end - MINUTES_KEPT_FOR - hour, last_hour_end).await?;
        self.store().prune_metric_points(Resolution::Minute, now - MINUTES_KEPT_FOR).await?;
        self.store().prune_metric_points(Resolution::Hour, now - HOURS_KEPT_FOR).await?;
        Ok(())
    }
}

/// The points bucketed to the range's step.
fn graph(range: MetricRange, points: Vec<MetricPoint>, since: Timestamp, until: Timestamp) -> Metrics {
    let step = range.step();
    let points = metrics::bucket(&points, step);
    Metrics { range, step_seconds: step.as_secs(), points, since, until }
}

#[cfg(test)]
mod tests;
