//! The Docker driver, over the Engine API with bollard.

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use bollard::{
    API_DEFAULT_VERSION, Docker,
    errors::Error,
    models::{ContainerInspectResponse, ContainerStateStatusEnum, EventMessageTypeEnum, HealthStatusEnum},
    query_parameters::{
        EventsOptionsBuilder, ListContainersOptionsBuilder, RemoveContainerOptionsBuilder, RestartContainerOptionsBuilder,
        StopContainerOptionsBuilder,
    },
};
use jiff::Timestamp;
use spinikus_core::server::{InstanceHealth, InstanceStatus, ObservedInstance};
use tokio_stream::StreamExt;

use super::{ContainerEvents, ContainerRuntime, DEPLOYMENT_LABEL, RuntimeError, SERVICE_LABEL, owner};

/// How long to wait for the Engine API to answer, beyond any grace period a call waits for.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Container event actions that can change what the agent reports. Others, such as the
/// `exec_*` events every health check produces, are skipped.
const REPORTED_ACTIONS: [&str; 12] =
    ["create", "start", "restart", "die", "stop", "kill", "oom", "pause", "unpause", "destroy", "health_status", "update"];

/// Connects on first use, and again on every call until that works: bollard checks that a
/// Unix socket exists when its client is built, and Docker Desktop removes its socket while
/// stopped. A daemon that is down is [`RuntimeError::Unavailable`] until it starts.
#[derive(Clone, Default)]
pub struct DockerRuntime {
    docker: Arc<Mutex<Option<Docker>>>,
}

impl DockerRuntime {
    /// A runtime for `DOCKER_HOST` if set, otherwise the first local socket that exists:
    /// `/var/run/docker.sock`, then Docker Desktop's `~/.docker/run/docker.sock`.
    pub fn new() -> Self {
        Self::default()
    }

    /// The bollard client, for what the trait does not cover, such as tests that set up
    /// containers.
    pub fn client(&self) -> Result<Docker, RuntimeError> {
        let mut docker = self.docker.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(docker) = docker.as_ref() {
            return Ok(docker.clone());
        }
        let connected = match std::env::var("DOCKER_HOST") {
            Ok(host) => Docker::connect_with_host(&host),
            Err(_) => match local_socket() {
                Some(path) => Docker::connect_with_unix(&path, REQUEST_TIMEOUT.as_secs(), API_DEFAULT_VERSION),
                None => Docker::connect_with_local_defaults(),
            },
        };
        let connected = connected.map_err(|error| RuntimeError::Unavailable(error.to_string()))?.with_timeout(REQUEST_TIMEOUT);
        Ok(docker.insert(connected).clone())
    }

    /// A client whose requests may take `grace` longer, for calls that wait for a
    /// container to stop.
    fn waiting(&self, grace: Duration) -> Result<Docker, RuntimeError> {
        Ok(self.client()?.with_timeout(REQUEST_TIMEOUT + grace))
    }
}

fn local_socket() -> Option<String> {
    let home = std::env::var_os("HOME").map(|home| Path::new(&home).join(".docker/run/docker.sock"));
    [Some(Path::new("/var/run/docker.sock").to_path_buf()), home]
        .into_iter()
        .flatten()
        .find(|path| path.exists())
        .map(|path| path.to_string_lossy().into_owned())
}

#[async_trait]
impl ContainerRuntime for DockerRuntime {
    async fn list(&self) -> Result<Vec<ObservedInstance>, RuntimeError> {
        let filters = HashMap::from([("label", vec![SERVICE_LABEL, DEPLOYMENT_LABEL])]);
        let options = ListContainersOptionsBuilder::new().all(true).filters(&filters).build();
        let summaries = self.client()?.list_containers(Some(options)).await.map_err(runtime_error)?;
        let mut instances = Vec::with_capacity(summaries.len());
        // The summary lacks health details, restart counts and start times, so inspect
        // each one. A container removed in between is simply left out.
        for id in summaries.into_iter().filter_map(|summary| summary.id) {
            if let Some(instance) = self.inspect(&id).await? {
                instances.push(instance);
            }
        }
        Ok(instances)
    }

    async fn inspect(&self, container_id: &str) -> Result<Option<ObservedInstance>, RuntimeError> {
        match self.client()?.inspect_container(container_id, None).await.map_err(runtime_error) {
            Ok(response) => Ok(observed(response)),
            Err(RuntimeError::NotFound) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn stop(&self, container_id: &str, grace: Duration) -> Result<(), RuntimeError> {
        let options = StopContainerOptionsBuilder::new().t(seconds(grace)).build();
        self.waiting(grace)?.stop_container(container_id, Some(options)).await.map_err(runtime_error)
    }

    async fn remove(&self, container_id: &str) -> Result<(), RuntimeError> {
        // Anonymous volumes go with the container; named volumes are kept.
        let options = RemoveContainerOptionsBuilder::new().v(true).build();
        self.client()?.remove_container(container_id, Some(options)).await.map_err(runtime_error)
    }

    async fn restart(&self, container_id: &str, grace: Duration) -> Result<(), RuntimeError> {
        let options = RestartContainerOptionsBuilder::new().t(seconds(grace)).build();
        self.waiting(grace)?.restart_container(container_id, Some(options)).await.map_err(runtime_error)
    }

    fn events(&self) -> ContainerEvents {
        // Start a second early, so a change made while the request is on its way is not
        // missed; repeats are harmless.
        let since = (Timestamp::now().as_second() - 1).to_string();
        let filters = HashMap::from([("type", vec!["container"]), ("label", vec![SERVICE_LABEL, DEPLOYMENT_LABEL])]);
        let options = EventsOptionsBuilder::new().since(&since).filters(&filters).build();
        let docker = match self.client() {
            Ok(docker) => docker,
            Err(error) => return Box::pin(tokio_stream::once(Err(error))),
        };
        let events = docker.events(Some(options)).filter_map(|event| match event {
            Ok(event) => {
                let action = event.action.as_deref().unwrap_or_default();
                // Health changes come as `health_status: healthy` and the like.
                let action = action.split(':').next().unwrap_or_default();
                let reported = event.typ == Some(EventMessageTypeEnum::CONTAINER) && REPORTED_ACTIONS.contains(&action);
                reported.then_some(()).and_then(|()| event.actor?.id).map(Ok)
            }
            Err(error) => Some(Err(runtime_error(error))),
        });
        Box::pin(events)
    }
}

fn seconds(duration: Duration) -> i32 {
    i32::try_from(duration.as_secs()).unwrap_or(i32::MAX)
}

fn runtime_error(error: Error) -> RuntimeError {
    match error {
        Error::DockerResponseServerError { status_code: 404, .. } => RuntimeError::NotFound,
        Error::DockerResponseServerError { message, .. } => RuntimeError::Failed(message),
        error => RuntimeError::Unavailable(error.to_string()),
    }
}

/// What the agent reports for a container, or `None` if Spinikus does not manage it.
fn observed(response: ContainerInspectResponse) -> Option<ObservedInstance> {
    let (service_id, deployment_id) = owner(response.config.as_ref().and_then(|config| config.labels.as_ref()))?;
    let state = response.state.unwrap_or_default();
    let status = match state.status {
        Some(ContainerStateStatusEnum::RUNNING | ContainerStateStatusEnum::STOPPING) => InstanceStatus::Running,
        Some(ContainerStateStatusEnum::PAUSED) => InstanceStatus::Paused,
        Some(ContainerStateStatusEnum::RESTARTING) => InstanceStatus::Restarting,
        Some(ContainerStateStatusEnum::REMOVING) => InstanceStatus::Removing,
        Some(ContainerStateStatusEnum::EXITED) => InstanceStatus::Exited,
        Some(ContainerStateStatusEnum::DEAD) => InstanceStatus::Dead,
        Some(ContainerStateStatusEnum::CREATED | ContainerStateStatusEnum::EMPTY) | None => InstanceStatus::Created,
    };
    let health = match state.health.and_then(|health| health.status) {
        Some(HealthStatusEnum::STARTING) => InstanceHealth::Starting,
        Some(HealthStatusEnum::HEALTHY) => InstanceHealth::Healthy,
        Some(HealthStatusEnum::UNHEALTHY) => InstanceHealth::Unhealthy,
        Some(HealthStatusEnum::NONE | HealthStatusEnum::EMPTY) | None => InstanceHealth::None,
    };
    // Docker reports `0001-01-01T00:00:00Z` for containers that never started.
    let started_at = state
        .started_at
        .and_then(|at| at.parse::<Timestamp>().ok())
        .filter(|at| at.as_second() > 0 && status != InstanceStatus::Created);
    Some(ObservedInstance {
        service_id,
        deployment_id,
        container_id: response.id?,
        status,
        health,
        restart_count: response.restart_count.and_then(|count| u32::try_from(count).ok()).unwrap_or(0),
        started_at,
    })
}

#[cfg(test)]
mod tests {
    use bollard::models::{ContainerConfig, ContainerState, Health};
    use spinikus_core::id::{DeploymentId, ServiceId};

    use super::*;

    fn inspected(labels: Option<HashMap<String, String>>, state: ContainerState) -> ContainerInspectResponse {
        ContainerInspectResponse {
            id: Some("c0ffee".to_string()),
            config: Some(ContainerConfig { labels, ..Default::default() }),
            state: Some(state),
            restart_count: Some(3),
            ..Default::default()
        }
    }

    fn labels(service: ServiceId, deployment: DeploymentId) -> Option<HashMap<String, String>> {
        Some(HashMap::from([
            (SERVICE_LABEL.to_string(), service.to_string()),
            (DEPLOYMENT_LABEL.to_string(), deployment.to_string()),
            ("other".to_string(), "label".to_string()),
        ]))
    }

    #[test]
    fn reads_a_managed_container() {
        let (service, deployment) = (ServiceId::new(), DeploymentId::new());
        let state = ContainerState {
            status: Some(ContainerStateStatusEnum::RUNNING),
            started_at: Some("2026-10-31T12:00:00.123456789Z".to_string()),
            health: Some(Health { status: Some(HealthStatusEnum::HEALTHY), ..Default::default() }),
            ..Default::default()
        };

        let instance = observed(inspected(labels(service, deployment), state)).unwrap();

        assert_eq!(instance.service_id, service);
        assert_eq!(instance.deployment_id, deployment);
        assert_eq!(instance.container_id, "c0ffee");
        assert_eq!(instance.status, InstanceStatus::Running);
        assert_eq!(instance.health, InstanceHealth::Healthy);
        assert_eq!(instance.restart_count, 3);
        assert_eq!(instance.started_at.unwrap().as_second(), "2026-10-31T12:00:00Z".parse::<Timestamp>().unwrap().as_second());
    }

    #[test]
    fn a_container_that_never_started_has_no_start_time() {
        let state = ContainerState {
            status: Some(ContainerStateStatusEnum::CREATED),
            started_at: Some("0001-01-01T00:00:00Z".to_string()),
            ..Default::default()
        };

        let instance = observed(inspected(labels(ServiceId::new(), DeploymentId::new()), state)).unwrap();

        assert_eq!(instance.status, InstanceStatus::Created);
        assert_eq!(instance.health, InstanceHealth::None);
        assert_eq!(instance.started_at, None);
    }

    #[test]
    fn skips_containers_spinikus_does_not_manage() {
        let running = || ContainerState { status: Some(ContainerStateStatusEnum::RUNNING), ..Default::default() };
        let invalid = HashMap::from([
            (SERVICE_LABEL.to_string(), "web".to_string()),
            (DEPLOYMENT_LABEL.to_string(), DeploymentId::new().to_string()),
        ]);

        assert_eq!(observed(inspected(None, running())), None);
        assert_eq!(observed(inspected(Some(HashMap::new()), running())), None);
        assert_eq!(observed(inspected(Some(invalid), running())), None);
    }

    #[test]
    fn maps_daemon_errors() {
        let server = |status_code| Error::DockerResponseServerError { status_code, message: "no".to_string() };

        assert_eq!(runtime_error(server(404)), RuntimeError::NotFound);
        assert_eq!(runtime_error(server(500)), RuntimeError::Failed("no".to_string()));
        assert!(matches!(runtime_error(Error::RequestTimeoutError), RuntimeError::Unavailable(_)));
    }
}
