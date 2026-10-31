//! The Docker driver, over the Engine API with bollard.

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use bollard::{
    API_DEFAULT_VERSION, Docker,
    errors::Error,
    models::{
        ContainerCreateBody, ContainerInspectResponse, ContainerStateStatusEnum, EventMessageTypeEnum, HealthStatusEnum,
        HostConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum,
    },
    query_parameters::{
        CreateContainerOptionsBuilder, CreateImageOptionsBuilder, EventsOptionsBuilder, ListContainersOptionsBuilder,
        RemoveContainerOptionsBuilder, RestartContainerOptionsBuilder, StopContainerOptionsBuilder,
    },
};
use jiff::Timestamp;
use spinikus_core::server::{InstanceHealth, InstanceStatus, ObservedInstance};
use tokio_stream::StreamExt;

use super::{ContainerEvents, ContainerRuntime, ContainerSpec, DEPLOYMENT_LABEL, PORT_LABEL, RuntimeError, SERVICE_LABEL, owner};

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

    async fn pull(&self, image: &str, progress: &(dyn Fn(String) + Send + Sync)) -> Result<String, RuntimeError> {
        let docker = self.client()?;
        // Without a tag, Docker would pull every tag of the repository.
        let reference = with_tag(image);
        let options = CreateImageOptionsBuilder::new().from_image(&reference).build();
        let mut pulling = docker.create_image(Some(options), None, None);
        while let Some(step) = pulling.next().await {
            let step = step.map_err(|error| match runtime_error(error) {
                // The registry's message, such as an unknown image or a denied pull.
                RuntimeError::NotFound => RuntimeError::Failed(format!("cannot pull {reference}: not found")),
                error => error,
            })?;
            // Skip the byte counts of layers being downloaded and extracted.
            if let Some(status) = step.status.filter(|_| step.progress_detail.is_none()) {
                progress(match step.id {
                    Some(id) => format!("{id}: {status}"),
                    None => status,
                });
            }
        }
        let inspected = docker.inspect_image(&reference).await.map_err(runtime_error)?;
        pinned(&reference, inspected.repo_digests.unwrap_or_default(), inspected.id)
            .ok_or_else(|| RuntimeError::Failed(format!("{reference} has no digest")))
    }

    async fn create(&self, spec: &ContainerSpec) -> Result<String, RuntimeError> {
        let mut labels = HashMap::from([
            (SERVICE_LABEL.to_string(), spec.service_id.to_string()),
            (DEPLOYMENT_LABEL.to_string(), spec.deployment_id.to_string()),
        ]);
        let port = spec.port.map(|port| format!("{port}/tcp"));
        if let Some(app_port) = spec.port {
            labels.insert(PORT_LABEL.to_string(), app_port.to_string());
        }
        // An empty host port lets Docker pick a free one.
        let loopback = PortBinding { host_ip: Some(Ipv4Addr::LOCALHOST.to_string()), host_port: Some(String::new()) };
        let host_config = HostConfig {
            port_bindings: port.clone().map(|port| HashMap::from([(port, Some(vec![loopback]))])),
            binds: Some(spec.volumes.iter().map(|mount| format!("{}:{}", mount.volume, mount.path)).collect()),
            // Containers come back after a reboot or a daemon restart, without the agent.
            restart_policy: Some(RestartPolicy { name: Some(RestartPolicyNameEnum::UNLESS_STOPPED), ..Default::default() }),
            ..Default::default()
        };
        let body = ContainerCreateBody {
            image: Some(spec.image.clone()),
            env: Some(spec.env.iter().map(|(name, value)| format!("{name}={value}")).collect()),
            labels: Some(labels),
            exposed_ports: port.map(|port| vec![port]),
            host_config: Some(host_config),
            ..Default::default()
        };
        let options = CreateContainerOptionsBuilder::new().name(&spec.name).build();
        match self.client()?.create_container(Some(options), body).await {
            Ok(created) => Ok(created.id),
            Err(Error::DockerResponseServerError { status_code: 409, .. }) => Err(RuntimeError::Conflict),
            Err(Error::DockerResponseServerError { status_code: 404, message }) => Err(RuntimeError::Failed(message)),
            Err(error) => Err(runtime_error(error)),
        }
    }

    async fn start(&self, container_id: &str) -> Result<(), RuntimeError> {
        self.client()?.start_container(container_id, None).await.map_err(runtime_error)
    }

    async fn endpoint(&self, container_id: &str) -> Result<Option<SocketAddr>, RuntimeError> {
        let inspected = match self.client()?.inspect_container(container_id, None).await.map_err(runtime_error) {
            Ok(inspected) => inspected,
            Err(RuntimeError::NotFound) => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(published(&inspected))
    }
}

/// `image` with `:latest` added when it names neither a tag nor a digest.
fn with_tag(image: &str) -> String {
    let name = image.rsplit('/').next().unwrap_or(image);
    if image.contains('@') || name.contains(':') { image.to_string() } else { format!("{image}:latest") }
}

/// The digest reference for `reference`'s repository among `repo_digests`, else the first
/// one, else the image ID (for images that were never pushed or pulled by digest).
fn pinned(reference: &str, repo_digests: Vec<String>, id: Option<String>) -> Option<String> {
    let repository = match reference.split_once('@') {
        Some((repository, _)) => repository,
        None => reference.rsplit_once(':').filter(|(_, tag)| !tag.contains('/')).map_or(reference, |(repository, _)| repository),
    };
    let matching = repo_digests.iter().find(|digest| digest.split_once('@').is_some_and(|(repo, _)| repo == repository));
    matching.or(repo_digests.first()).cloned().or(id)
}

/// The loopback address the container's app port is published on, while it runs.
fn published(inspected: &ContainerInspectResponse) -> Option<SocketAddr> {
    let running = inspected.state.as_ref().and_then(|state| state.running).unwrap_or(false);
    let labels = inspected.config.as_ref()?.labels.as_ref()?;
    let port: u16 = labels.get(PORT_LABEL)?.parse().ok()?;
    let bindings = inspected.network_settings.as_ref()?.ports.as_ref()?.get(&format!("{port}/tcp"))?.as_ref()?;
    let host_port = bindings.iter().find_map(|binding| binding.host_port.as_deref()?.parse::<u16>().ok())?;
    running.then_some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), host_port))
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
    fn tags_and_pins_images() {
        let digest = "sha256:0123";

        assert_eq!(with_tag("busybox"), "busybox:latest");
        assert_eq!(with_tag("busybox:1.37"), "busybox:1.37");
        assert_eq!(with_tag("localhost:5000/app"), "localhost:5000/app:latest");
        assert_eq!(with_tag("ghcr.io/acme/app@sha256:0123"), "ghcr.io/acme/app@sha256:0123");

        let digests = vec![format!("mirror/app@{digest}"), format!("localhost:5000/app@{digest}")];
        assert_eq!(pinned("localhost:5000/app:v2", digests.clone(), None).unwrap(), format!("localhost:5000/app@{digest}"));
        assert_eq!(pinned("other:latest", digests, None).unwrap(), format!("mirror/app@{digest}"));
        assert_eq!(pinned("local:dev", Vec::new(), Some("sha256:abcd".to_string())).unwrap(), "sha256:abcd");
    }

    #[test]
    fn finds_the_published_loopback_port() {
        use bollard::models::NetworkSettings;
        let inspected = |running: bool, label: Option<&str>| ContainerInspectResponse {
            config: Some(ContainerConfig {
                labels: Some(label.map(|port| (PORT_LABEL.to_string(), port.to_string())).into_iter().collect()),
                ..Default::default()
            }),
            state: Some(ContainerState { running: Some(running), ..Default::default() }),
            network_settings: Some(NetworkSettings {
                ports: Some(HashMap::from([(
                    "3000/tcp".to_string(),
                    Some(vec![PortBinding { host_ip: Some("127.0.0.1".to_string()), host_port: Some("49153".to_string()) }]),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        };

        assert_eq!(published(&inspected(true, Some("3000"))), Some("127.0.0.1:49153".parse().unwrap()));
        assert_eq!(published(&inspected(false, Some("3000"))), None);
        assert_eq!(published(&inspected(true, Some("8080"))), None);
        assert_eq!(published(&inspected(true, None)), None);
    }

    #[test]
    fn maps_daemon_errors() {
        let server = |status_code| Error::DockerResponseServerError { status_code, message: "no".to_string() };

        assert_eq!(runtime_error(server(404)), RuntimeError::NotFound);
        assert_eq!(runtime_error(server(500)), RuntimeError::Failed("no".to_string()));
        assert!(matches!(runtime_error(Error::RequestTimeoutError), RuntimeError::Unavailable(_)));
    }
}
