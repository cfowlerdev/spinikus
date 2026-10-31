//! The reporter: tells the control plane which containers Spinikus manages on this
//! server. After connecting it sends a full `StateReport` as soon as the runtime can be
//! listed, then one delta per change. When the runtime goes away it starts over with a
//! full report, since changes may have been missed in between.
//!
//! A full report replaces everything the control plane knows about this server, so one is
//! only sent from a successful listing, never while the runtime is unreachable.

use std::time::Duration;

use spinikus_core::server::{InstanceHealth, InstanceStatus, ObservedInstance};
use spinikus_proto::agent::v1::{self as pb, AgentMessage, agent_message};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use crate::runtime::{ContainerEvents, ContainerRuntime, RuntimeError};

/// Reports until `outbound` closes, retrying `resync` after the runtime fails.
pub(crate) async fn report(runtime: &dyn ContainerRuntime, outbound: &mpsc::Sender<AgentMessage>, resync: Duration) {
    let mut failing = false;
    loop {
        // Listen first, so a change between listing and listening is not missed.
        let mut events = runtime.events();
        let error = match runtime.list().await {
            Ok(instances) => {
                if failing {
                    tracing::info!("the container runtime is back");
                    failing = false;
                }
                let report = pb::StateReport {
                    full: true,
                    instances: instances.into_iter().map(instance).collect(),
                    removed_container_ids: Vec::new(),
                };
                if send(outbound, report).await.is_err() {
                    return;
                }
                match follow(runtime, &mut events, outbound).await {
                    Followed::Closed => return,
                    Followed::Failed(error) => error,
                }
            }
            Err(error) => error,
        };
        if failing {
            tracing::debug!(%error, "the container runtime is still failing");
        } else {
            tracing::warn!(%error, retry_in = ?resync, "cannot follow the containers on this server");
            failing = true;
        }
        tokio::time::sleep(resync).await;
    }
}

enum Followed {
    /// The connection closed.
    Closed,
    Failed(RuntimeError),
}

/// Sends a delta for each changed container until the runtime fails.
async fn follow(runtime: &dyn ContainerRuntime, events: &mut ContainerEvents, outbound: &mpsc::Sender<AgentMessage>) -> Followed {
    loop {
        let container = match events.next().await {
            Some(Ok(container)) => container,
            Some(Err(error)) => return Followed::Failed(error),
            None => return Followed::Failed(RuntimeError::Unavailable("the event stream ended".to_string())),
        };
        let report = match runtime.inspect(&container).await {
            Ok(Some(changed)) => {
                pb::StateReport { full: false, instances: vec![instance(changed)], removed_container_ids: Vec::new() }
            }
            // Gone, or no longer one Spinikus manages.
            Ok(None) => pb::StateReport { full: false, instances: Vec::new(), removed_container_ids: vec![container] },
            Err(error) => return Followed::Failed(error),
        };
        if send(outbound, report).await.is_err() {
            return Followed::Closed;
        }
    }
}

async fn send(outbound: &mpsc::Sender<AgentMessage>, report: pb::StateReport) -> Result<(), ()> {
    let message = AgentMessage { message: Some(agent_message::Message::StateReport(report)) };
    outbound.send(message).await.map_err(|_| ())
}

fn instance(instance: ObservedInstance) -> pb::Instance {
    let status = match instance.status {
        InstanceStatus::Created => pb::InstanceStatus::Created,
        InstanceStatus::Running => pb::InstanceStatus::Running,
        InstanceStatus::Paused => pb::InstanceStatus::Paused,
        InstanceStatus::Restarting => pb::InstanceStatus::Restarting,
        InstanceStatus::Removing => pb::InstanceStatus::Removing,
        InstanceStatus::Exited => pb::InstanceStatus::Exited,
        InstanceStatus::Dead => pb::InstanceStatus::Dead,
    };
    let health = match instance.health {
        InstanceHealth::None => pb::InstanceHealth::None,
        InstanceHealth::Starting => pb::InstanceHealth::Starting,
        InstanceHealth::Healthy => pb::InstanceHealth::Healthy,
        InstanceHealth::Unhealthy => pb::InstanceHealth::Unhealthy,
    };
    pb::Instance {
        container_id: instance.container_id,
        service_id: instance.service_id.to_string(),
        deployment_id: instance.deployment_id.to_string(),
        status: status.into(),
        health: health.into(),
        restart_count: instance.restart_count,
        started_at: instance
            .started_at
            .map(|at| prost_types::Timestamp { seconds: at.as_second(), nanos: at.subsec_nanosecond() }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use spinikus_core::id::{DeploymentId, ServiceId};
    use tokio::{task::JoinHandle, time::timeout};

    use super::*;
    use crate::runtime::MemoryRuntime;

    const RESYNC: Duration = Duration::from_millis(20);

    fn container(id: &str) -> ObservedInstance {
        ObservedInstance {
            service_id: ServiceId::new(),
            deployment_id: DeploymentId::new(),
            container_id: id.to_string(),
            status: InstanceStatus::Running,
            health: InstanceHealth::None,
            restart_count: 0,
            started_at: Some(jiff::Timestamp::now()),
        }
    }

    fn start(runtime: &Arc<MemoryRuntime>) -> (mpsc::Receiver<AgentMessage>, JoinHandle<()>) {
        let (outbound, receiver) = mpsc::channel(8);
        let runtime = runtime.clone();
        let reporter = tokio::spawn(async move { report(&*runtime, &outbound, RESYNC).await });
        (receiver, reporter)
    }

    async fn next_report(receiver: &mut mpsc::Receiver<AgentMessage>) -> pb::StateReport {
        match timeout(Duration::from_secs(5), receiver.recv()).await.unwrap().unwrap().message {
            Some(agent_message::Message::StateReport(report)) => report,
            other => panic!("expected a state report, got {other:?}"),
        }
    }

    fn ids(instances: &[pb::Instance]) -> Vec<&str> {
        instances.iter().map(|instance| instance.container_id.as_str()).collect()
    }

    #[tokio::test]
    async fn sends_a_full_report_then_changes() {
        let runtime = Arc::new(MemoryRuntime::new());
        let first = container("a");
        runtime.put(first.clone());
        runtime.put(container("b"));
        let (mut receiver, reporter) = start(&runtime);

        let full = next_report(&mut receiver).await;
        assert!(full.full);
        assert_eq!(ids(&full.instances), ["a", "b"]);
        let reported = &full.instances[0];
        assert_eq!(reported.service_id, first.service_id.to_string());
        assert_eq!(reported.deployment_id, first.deployment_id.to_string());
        assert_eq!(reported.status, pb::InstanceStatus::Running as i32);
        assert_eq!(reported.health, pb::InstanceHealth::None as i32);
        assert_eq!(reported.started_at.unwrap().seconds, first.started_at.unwrap().as_second());

        runtime.stop("a", Duration::ZERO).await.unwrap();
        let stopped = next_report(&mut receiver).await;
        assert!(!stopped.full);
        assert_eq!(ids(&stopped.instances), ["a"]);
        assert_eq!(stopped.instances[0].status, pb::InstanceStatus::Exited as i32);

        runtime.remove("a").await.unwrap();
        let removed = next_report(&mut receiver).await;
        assert!(!removed.full);
        assert!(removed.instances.is_empty());
        assert_eq!(removed.removed_container_ids, ["a"]);

        reporter.abort();
    }

    #[tokio::test]
    async fn waits_for_the_runtime_and_resyncs_after_it_fails() {
        let runtime = Arc::new(MemoryRuntime::new());
        runtime.put(container("a"));
        runtime.set_available(false);
        let (mut receiver, reporter) = start(&runtime);

        // No full report while the runtime cannot be listed: it would wipe the server.
        tokio::time::sleep(RESYNC * 5).await;
        assert!(receiver.try_recv().is_err());

        runtime.set_available(true);
        let full = next_report(&mut receiver).await;
        assert!(full.full);
        assert_eq!(ids(&full.instances), ["a"]);

        runtime.set_available(false);
        runtime.set_available(true);
        runtime.put(container("b"));
        // Whatever deltas came first, a new full report follows the failure.
        let resynced = loop {
            let report = next_report(&mut receiver).await;
            if report.full {
                break report;
            }
        };
        assert_eq!(ids(&resynced.instances), ["a", "b"]);

        reporter.abort();
    }

    #[tokio::test]
    async fn stops_when_the_connection_closes() {
        let runtime = Arc::new(MemoryRuntime::new());
        let (receiver, reporter) = start(&runtime);
        drop(receiver);

        timeout(Duration::from_secs(5), reporter).await.unwrap().unwrap();
    }
}
