//! The container runtime, behind a trait so Docker is one driver among later ones
//! (Podman, containerd). The agent only sees containers Spinikus manages: those with the
//! [`SERVICE_LABEL`] and [`DEPLOYMENT_LABEL`] labels, which say what they belong to.
//!
//! An app container's port is published on `127.0.0.1` with a host port the runtime picks
//! ([`ContainerRuntime::endpoint`]); the health check and the proxy connect there.

use std::{net::SocketAddr, path::PathBuf, pin::Pin, time::Duration};

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
/// The label holding the port the app listens on inside the container, if it has one.
pub const PORT_LABEL: &str = "spinikus.port";

/// A container to create for a deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerSpec {
    /// Unique on the server; creating a second container with the same name fails.
    pub name: String,
    /// The image, pinned by digest.
    pub image: String,
    pub service_id: spinikus_core::id::ServiceId,
    pub deployment_id: spinikus_core::id::DeploymentId,
    /// Environment variables. Values may be secrets: never log them.
    pub env: Vec<(String, String)>,
    /// The port the app listens on, published on `127.0.0.1`.
    pub port: Option<u16>,
    /// Named runtime volumes and where to mount them.
    pub volumes: Vec<Mount>,
    /// The network to join, created if it does not exist; the runtime's default if `None`.
    pub network: Option<Network>,
    /// The command to run in place of the image's; the image's if empty.
    pub command: Vec<String>,
}

/// A network shared with other containers, and the names they reach this one by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    pub name: String,
    pub aliases: Vec<String>,
}

/// An image to build from a directory with a Dockerfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildSpec {
    /// The build context, sent to the builder whole except for `.git`.
    pub context: PathBuf,
    /// The Dockerfile, relative to the context.
    pub dockerfile: String,
    /// The stage to build, the last one if `None`.
    pub target: Option<String>,
    /// Build arguments. Values may be secrets: never log them.
    pub args: Vec<(String, String)>,
    /// The tag to give the image.
    pub tag: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub volume: String,
    pub path: String,
}

/// The IDs of managed containers as they change (created, started, stopped, removed, or
/// their health changed). An item can repeat a change already seen. The stream fails or
/// ends when the runtime goes away; changes after that are only found by listing again.
pub type ContainerEvents = Pin<Box<dyn Stream<Item = Result<String, RuntimeError>> + Send>>;

/// Which part of a container's log to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LogOptions {
    /// Only lines written at or after this time.
    pub since: Option<jiff::Timestamp>,
    /// At most this many of the lines already written.
    pub tail: Option<u32>,
    /// Keep going with new lines as they are written.
    pub follow: bool,
}

/// One line a container wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub at: Option<jiff::Timestamp>,
    pub stderr: bool,
    pub text: String,
}

/// A container's log lines; it ends when there are no more, or with an error.
pub type LogLines = Pin<Box<dyn Stream<Item = Result<LogLine, RuntimeError>> + Send>>;

/// What a command run in a container writes, as it writes it, and then how it exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecOutput {
    Stdout(bytes::Bytes),
    Stderr(bytes::Bytes),
    /// The last item: the command's exit code.
    Exited(i64),
}

/// A command's output; it ends after [`ExecOutput::Exited`], or with an error.
pub type ExecStream = Pin<Box<dyn Stream<Item = Result<ExecOutput, RuntimeError>> + Send>>;

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

    /// The container's log, stdout and stderr together, as `options` says.
    fn logs(&self, container_id: &str, options: LogOptions) -> LogLines;

    /// Runs a command in a running container, with the container's environment, without a
    /// terminal or stdin. Dropping the stream stops reading its output, not the command.
    fn exec(&self, container_id: &str, command: Vec<String>) -> ExecStream;

    /// Pulls an image and returns the reference to run, pinned by digest. `progress` gets
    /// a line for each step of the pull.
    async fn pull(&self, image: &str, progress: &(dyn Fn(String) + Send + Sync)) -> Result<String, RuntimeError>;

    /// Builds an image and returns its ID (`sha256:…`), which runs it exactly. `progress`
    /// gets the build's output, a line at a time. A failed build is `Failed`, with the
    /// builder's error.
    async fn build(&self, spec: &BuildSpec, progress: &(dyn Fn(String) + Send + Sync)) -> Result<String, RuntimeError>;

    /// Removes an image reference (a tag, a reference pinned by digest, or an ID); the image
    /// goes once nothing refers to it. `NotFound` if there is no such reference, `InUse` if
    /// a container uses the image or an ID still has other references.
    async fn remove_image(&self, image: &str) -> Result<(), RuntimeError>;

    /// Creates a container, without starting it, and returns its ID. Its network is created
    /// first if it does not exist.
    async fn create(&self, spec: &ContainerSpec) -> Result<String, RuntimeError>;

    /// Starts a created or stopped container. Starting a running one succeeds.
    async fn start(&self, container_id: &str) -> Result<(), RuntimeError>;

    /// Where the container's app port is published on this host, or `None` if it has no
    /// port or is not running.
    async fn endpoint(&self, container_id: &str) -> Result<Option<SocketAddr>, RuntimeError>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeError {
    #[error("no such container")]
    NotFound,
    #[error("the container runtime is unreachable: {0}")]
    Unavailable(String),
    #[error("the container runtime failed: {0}")]
    Failed(String),
    /// A container with the same name exists.
    #[error("a container with this name exists")]
    Conflict,
    /// The image is used by a container, or has other references.
    #[error("the image is in use")]
    InUse,
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
