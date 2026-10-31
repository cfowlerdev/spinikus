//! The container runtime, behind a trait so Docker is one driver among later ones
//! (Podman, containerd). The agent only sees containers Spinikus manages: those with the
//! [`SERVICE_LABEL`] and [`DEPLOYMENT_LABEL`] labels, which say what they belong to.

use std::{pin::Pin, time::Duration};

use async_trait::async_trait;
use spinikus_core::server::ObservedInstance;
use tokio_stream::Stream;

mod docker;
#[cfg(any(test, feature = "test-support"))]
mod memory;

pub use docker::DockerRuntime;
#[cfg(any(test, feature = "test-support"))]
pub use memory::MemoryRuntime;

/// The label holding the service ID (`svc_…`) on every container Spinikus starts.
pub const SERVICE_LABEL: &str = "spinikus.service";
/// The label holding the deployment ID (`dep_…`) on every container Spinikus starts.
pub const DEPLOYMENT_LABEL: &str = "spinikus.deployment";

/// The IDs of managed containers as they change (created, started, stopped, removed, or
/// their health changed). An item can repeat a change already seen. The stream fails or
/// ends when the runtime goes away; changes after that are only found by listing again.
pub type ContainerEvents = Pin<Box<dyn Stream<Item = Result<String, RuntimeError>> + Send>>;

#[async_trait]
pub trait ContainerRuntime: Send + Sync + 'static {
    /// Every container Spinikus manages on this server, running or not.
    async fn list(&self) -> Result<Vec<ObservedInstance>, RuntimeError>;

    /// One container, or `None` if it does not exist or Spinikus does not manage it.
    async fn inspect(&self, container_id: &str) -> Result<Option<ObservedInstance>, RuntimeError>;

    /// Stops a container, killing it if it has not stopped after `grace`. Stopping a
    /// stopped container succeeds.
    async fn stop(&self, container_id: &str, grace: Duration) -> Result<(), RuntimeError>;

    /// Removes a stopped container.
    async fn remove(&self, container_id: &str) -> Result<(), RuntimeError>;

    /// Restarts a container, killing it if it has not stopped after `grace`.
    async fn restart(&self, container_id: &str, grace: Duration) -> Result<(), RuntimeError>;

    /// Changes to managed containers from now on, including any that happen while the
    /// stream is being set up.
    fn events(&self) -> ContainerEvents;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeError {
    #[error("no such container")]
    NotFound,
    #[error("the container runtime is unreachable: {0}")]
    Unavailable(String),
    #[error("the container runtime failed: {0}")]
    Failed(String),
}

/// The service and deployment a container belongs to, from its labels. `None` for
/// containers Spinikus does not manage, or whose labels are not valid IDs.
pub(crate) fn owner(
    labels: Option<&std::collections::HashMap<String, String>>,
) -> Option<(spinikus_core::id::ServiceId, spinikus_core::id::DeploymentId)> {
    let labels = labels?;
    let service = labels.get(SERVICE_LABEL)?.parse().ok()?;
    let deployment = labels.get(DEPLOYMENT_LABEL)?.parse().ok()?;
    Some((service, deployment))
}
