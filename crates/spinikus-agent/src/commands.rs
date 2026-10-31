//! Commands from the control plane. Each runs in its own task, so a slow one (a stop
//! waiting out its grace period) never holds up the connection, and its result goes out
//! on whichever connection is open when it finishes. Delivery is at-least-once: a command
//! ID that is still running is ignored, and one that finished is answered with its stored
//! result.
//!
//! Commands only live in memory so far: if the agent restarts, the control plane has to
//! send them again.

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use spinikus_core::id::DeploymentId;
use spinikus_proto::agent::v1::{self as pb, FailureReason, command::Kind, command_result::Outcome};
use tokio::task::{self, JoinSet};

use crate::runtime::{ContainerRuntime, RuntimeError};

/// How long a container gets to stop when the command does not say.
const DEFAULT_GRACE: Duration = Duration::from_secs(10);
const MAX_GRACE: Duration = Duration::from_secs(10 * 60);

/// How many finished results are kept to answer repeated commands.
const KEPT_RESULTS: usize = 1024;

pub(crate) struct Commands {
    runtime: Arc<dyn ContainerRuntime>,
    tasks: JoinSet<pb::CommandResult>,
    running: HashMap<task::Id, String>,
    finished: HashMap<String, pb::CommandResult>,
    finished_order: VecDeque<String>,
}

impl Commands {
    pub(crate) fn new(runtime: Arc<dyn ContainerRuntime>) -> Self {
        Self {
            runtime,
            tasks: JoinSet::new(),
            running: HashMap::new(),
            finished: HashMap::new(),
            finished_order: VecDeque::new(),
        }
    }

    /// The commands still running, for `Hello` after a reconnect.
    pub(crate) fn in_flight(&self) -> Vec<String> {
        self.running.values().cloned().collect()
    }

    pub(crate) fn busy(&self) -> bool {
        !self.tasks.is_empty()
    }

    /// Starts a command. Returns a result to send right away instead: the stored one for a
    /// command that already finished, or a failure for one this agent cannot run.
    pub(crate) fn start(&mut self, command: pb::Command) -> Option<pb::CommandResult> {
        let id = command.command_id;
        if let Some(result) = self.finished.get(&id) {
            return Some(result.clone());
        }
        if self.running.values().any(|running| *running == id) {
            return None;
        }
        let runtime = self.runtime.clone();
        let work = match command.kind {
            Some(Kind::StopDeployment(stop)) => {
                tracing::info!(command = id, deployment = stop.deployment_id, "stopping a deployment");
                Box::pin(async move { stop_deployment(&*runtime, stop).await }) as Work
            }
            Some(Kind::RestartInstance(restart)) => {
                tracing::info!(command = id, container = restart.container_id, "restarting a container");
                Box::pin(async move { restart_instance(&*runtime, restart).await })
            }
            kind => {
                // The rest come with the builder, proxy and sessions.
                let message = match &kind {
                    Some(kind) => format!("this agent cannot run {} yet", name(kind)),
                    None => "this agent does not know this kind of command".to_string(),
                };
                tracing::warn!(command = id, message, "refused a command");
                return Some(result(id, Err(failure(FailureReason::Internal, message))));
            }
        };
        let command_id = id.clone();
        let handle = self.tasks.spawn(async move { result(command_id, work.await) });
        self.running.insert(handle.id(), id);
        None
    }

    /// The next command to finish, once it is recorded as finished. Pends while none run.
    pub(crate) async fn next_finished(&mut self) -> pb::CommandResult {
        let (task, finished) = match self.tasks.join_next_with_id().await {
            Some(Ok((task, finished))) => (task, Ok(finished)),
            Some(Err(error)) => (error.id(), Err(error)),
            None => std::future::pending().await,
        };
        let id = self.running.remove(&task).expect("every task is recorded when spawned");
        let finished = finished.unwrap_or_else(|error| {
            tracing::error!(command = id, %error, "a command's task failed");
            result(id.clone(), Err(failure(FailureReason::Internal, "the agent failed while running this command")))
        });
        if self.finished_order.len() == KEPT_RESULTS
            && let Some(oldest) = self.finished_order.pop_front()
        {
            self.finished.remove(&oldest);
        }
        self.finished_order.push_back(id.clone());
        self.finished.insert(id, finished.clone());
        finished
    }
}

type Work = std::pin::Pin<Box<dyn Future<Output = Result<(), pb::CommandFailed>> + Send>>;

fn name(kind: &Kind) -> &'static str {
    match kind {
        Kind::PrepareDeployment(_) => "PrepareDeployment",
        Kind::RunRelease(_) => "RunRelease",
        Kind::StartDeployment(_) => "StartDeployment",
        Kind::StopDeployment(_) => "StopDeployment",
        Kind::RestartInstance(_) => "RestartInstance",
        Kind::OpenSession(_) => "OpenSession",
        Kind::CancelCommand(_) => "CancelCommand",
    }
}

fn result(command_id: String, outcome: Result<(), pb::CommandFailed>) -> pb::CommandResult {
    let outcome = match outcome {
        Ok(()) => Outcome::Succeeded(pb::CommandSucceeded::default()),
        Err(failed) => Outcome::Failed(failed),
    };
    pb::CommandResult { command_id, outcome: Some(outcome) }
}

fn failure(reason: FailureReason, message: impl Into<String>) -> pb::CommandFailed {
    pb::CommandFailed { reason: reason.into(), message: message.into() }
}

impl From<RuntimeError> for pb::CommandFailed {
    fn from(error: RuntimeError) -> Self {
        failure(FailureReason::Internal, error.to_string())
    }
}

fn grace(period: Option<prost_types::Duration>) -> Duration {
    period.and_then(|period| Duration::try_from(period).ok()).unwrap_or(DEFAULT_GRACE).min(MAX_GRACE)
}

/// Treats a container that is already gone as done.
fn gone_is_done(outcome: Result<(), RuntimeError>) -> Result<(), RuntimeError> {
    match outcome {
        Err(RuntimeError::NotFound) => Ok(()),
        outcome => outcome,
    }
}

/// Stops and removes every container of a deployment. Containers already gone count as
/// stopped, so repeating the command succeeds.
async fn stop_deployment(runtime: &dyn ContainerRuntime, stop: pb::StopDeployment) -> Result<(), pb::CommandFailed> {
    let deployment: DeploymentId = stop
        .deployment_id
        .parse()
        .map_err(|_| failure(FailureReason::Internal, format!("invalid deployment ID `{}`", stop.deployment_id)))?;
    let grace = grace(stop.grace_period);
    let containers = runtime.list().await?.into_iter().filter(|instance| instance.deployment_id == deployment);
    for container in containers {
        gone_is_done(runtime.stop(&container.container_id, grace).await)?;
        gone_is_done(runtime.remove(&container.container_id).await)?;
    }
    Ok(())
}

/// Restarts one container Spinikus manages.
async fn restart_instance(runtime: &dyn ContainerRuntime, restart: pb::RestartInstance) -> Result<(), pb::CommandFailed> {
    let unknown = || failure(FailureReason::Internal, format!("no container `{}` managed by Spinikus", restart.container_id));
    if runtime.inspect(&restart.container_id).await?.is_none() {
        return Err(unknown());
    }
    match runtime.restart(&restart.container_id, DEFAULT_GRACE).await {
        Err(RuntimeError::NotFound) => Err(unknown()),
        outcome => Ok(outcome?),
    }
}

#[cfg(test)]
mod tests {
    use spinikus_core::{
        id::ServiceId,
        server::{InstanceHealth, InstanceStatus, ObservedInstance},
    };
    use tokio::time::timeout;

    use super::*;
    use crate::runtime::MemoryRuntime;

    fn container(id: &str, deployment: DeploymentId) -> ObservedInstance {
        ObservedInstance {
            service_id: ServiceId::new(),
            deployment_id: deployment,
            container_id: id.to_string(),
            status: InstanceStatus::Running,
            health: InstanceHealth::None,
            restart_count: 0,
            started_at: None,
        }
    }

    fn command(id: &str, kind: Kind) -> pb::Command {
        pb::Command { command_id: id.to_string(), kind: Some(kind) }
    }

    fn stop(deployment: DeploymentId) -> Kind {
        Kind::StopDeployment(pb::StopDeployment { deployment_id: deployment.to_string(), grace_period: None })
    }

    async fn finish(commands: &mut Commands) -> pb::CommandResult {
        timeout(Duration::from_secs(5), commands.next_finished()).await.expect("a command finished")
    }

    fn failed(result: &pb::CommandResult) -> &pb::CommandFailed {
        match &result.outcome {
            Some(Outcome::Failed(failed)) => failed,
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    fn succeeded(result: &pb::CommandResult) -> bool {
        matches!(result.outcome, Some(Outcome::Succeeded(_)))
    }

    #[tokio::test]
    async fn stops_and_removes_a_deployments_containers_only() {
        let runtime = Arc::new(MemoryRuntime::new());
        let (draining, live) = (DeploymentId::new(), DeploymentId::new());
        runtime.put(container("old-1", draining));
        runtime.put(container("old-2", draining));
        runtime.put(container("new", live));
        let mut commands = Commands::new(runtime.clone());

        assert_eq!(commands.start(command("cmd-1", stop(draining))), None);
        assert_eq!(commands.in_flight(), ["cmd-1"]);
        let result = finish(&mut commands).await;

        assert_eq!(result.command_id, "cmd-1");
        assert!(succeeded(&result), "{result:?}");
        let left: Vec<_> = runtime.containers().into_iter().map(|instance| instance.container_id).collect();
        assert_eq!(left, ["new"]);
        assert!(commands.in_flight().is_empty());
        assert!(!commands.busy());
    }

    #[tokio::test]
    async fn a_repeated_command_gets_the_stored_result() {
        let runtime = Arc::new(MemoryRuntime::new());
        let deployment = DeploymentId::new();
        runtime.put(container("c", deployment));
        let mut commands = Commands::new(runtime.clone());

        commands.start(command("cmd-1", stop(deployment)));
        // Still running: the repeat is dropped, and the one result comes later.
        assert_eq!(commands.start(command("cmd-1", stop(deployment))), None);
        let first = finish(&mut commands).await;
        runtime.put(container("c", deployment));
        let repeated = commands.start(command("cmd-1", stop(deployment)));

        assert_eq!(repeated, Some(first));
        assert!(!commands.busy());
        // The stored result was sent; the command did not run again.
        assert!(runtime.get("c").is_some());
    }

    #[tokio::test]
    async fn stopping_a_deployment_without_containers_succeeds() {
        let mut commands = Commands::new(Arc::new(MemoryRuntime::new()));

        commands.start(command("cmd-1", stop(DeploymentId::new())));

        assert!(succeeded(&finish(&mut commands).await));
    }

    #[tokio::test]
    async fn restarts_a_managed_container() {
        let runtime = Arc::new(MemoryRuntime::new());
        let mut exited = container("c", DeploymentId::new());
        exited.status = InstanceStatus::Exited;
        runtime.put(exited);
        let mut commands = Commands::new(runtime.clone());

        let restart = |id: &str| Kind::RestartInstance(pb::RestartInstance { container_id: id.to_string() });
        commands.start(command("cmd-1", restart("c")));
        commands.start(command("cmd-2", restart("missing")));
        let mut results = [finish(&mut commands).await, finish(&mut commands).await];
        results.sort_by(|a, b| a.command_id.cmp(&b.command_id));

        assert!(succeeded(&results[0]), "{:?}", results[0]);
        assert_eq!(runtime.get("c").unwrap().status, InstanceStatus::Running);
        assert_eq!(failed(&results[1]).message, "no container `missing` managed by Spinikus");
    }

    #[tokio::test]
    async fn reports_runtime_failures() {
        let runtime = Arc::new(MemoryRuntime::new());
        runtime.set_available(false);
        let mut commands = Commands::new(runtime);

        commands.start(command("cmd-1", stop(DeploymentId::new())));
        let result = finish(&mut commands).await;

        assert_eq!(failed(&result).reason, FailureReason::Internal as i32);
        assert!(failed(&result).message.contains("unreachable"), "{result:?}");
    }

    #[tokio::test]
    async fn refuses_commands_it_cannot_run_yet() {
        let mut commands = Commands::new(Arc::new(MemoryRuntime::new()));
        let start = Kind::StartDeployment(pb::StartDeployment::default());
        let invalid = Kind::StopDeployment(pb::StopDeployment { deployment_id: "web".to_string(), grace_period: None });

        let refused = commands.start(command("cmd-1", start)).unwrap();
        let unknown = commands.start(pb::Command { command_id: "cmd-2".to_string(), kind: None }).unwrap();
        commands.start(command("cmd-3", invalid));
        let invalid = finish(&mut commands).await;

        assert_eq!(failed(&refused).message, "this agent cannot run StartDeployment yet");
        assert_eq!(failed(&unknown).message, "this agent does not know this kind of command");
        assert_eq!(failed(&invalid).message, "invalid deployment ID `web`");
    }

    #[test]
    fn clamps_grace_periods() {
        let seconds = |seconds| Some(prost_types::Duration { seconds, nanos: 0 });

        assert_eq!(grace(None), DEFAULT_GRACE);
        assert_eq!(grace(seconds(30)), Duration::from_secs(30));
        assert_eq!(grace(seconds(-5)), DEFAULT_GRACE);
        assert_eq!(grace(seconds(86_400)), MAX_GRACE);
    }
}
