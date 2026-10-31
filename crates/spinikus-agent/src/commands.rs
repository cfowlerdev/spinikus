//! Commands from the control plane. Each runs in its own task, so a slow one (a stop
//! waiting out its grace period) never holds up the connection, and its result goes out
//! on whichever connection is open when it finishes. Delivery is at-least-once: a command
//! ID that is still running is ignored, and one that finished is answered with its stored
//! result. Progress (phases and log lines) is best effort: it is dropped while
//! disconnected, unlike results.
//!
//! Commands only live in memory so far: if the agent restarts, the control plane has to
//! send them again.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use spinikus_core::id::DeploymentId;
use spinikus_proto::agent::v1::{self as pb, FailureReason, command::Kind, command_result::Outcome};
use tokio::{
    sync::mpsc,
    task::{self, JoinSet},
};

use crate::{
    build::BuildOptions,
    deploy::{prepare_deployment, start_deployment},
    runtime::{ContainerRuntime, RuntimeError},
};

/// How long a container gets to stop when the command does not say.
const DEFAULT_GRACE: Duration = Duration::from_secs(10);
const MAX_GRACE: Duration = Duration::from_secs(10 * 60);

/// How many finished results are kept to answer repeated commands.
const KEPT_RESULTS: usize = 1024;

/// How many progress messages wait for the connection before new ones are dropped.
const PROGRESS_BUFFER: usize = 256;

pub(crate) struct Commands {
    runtime: Arc<dyn ContainerRuntime>,
    build: Arc<BuildOptions>,
    tasks: JoinSet<pb::CommandResult>,
    running: HashMap<task::Id, String>,
    finished: HashMap<String, pb::CommandResult>,
    finished_order: VecDeque<String>,
    progress_sender: mpsc::Sender<pb::CommandProgress>,
    progress: mpsc::Receiver<pb::CommandProgress>,
    /// Results waiting for their commands' progress to go out first.
    ready: VecDeque<pb::CommandResult>,
}

/// Something to send the control plane about a command.
#[derive(Debug, PartialEq)]
pub(crate) enum CommandEvent {
    Progress(pb::CommandProgress),
    Finished(pb::CommandResult),
}

/// A running command's way to report its progress.
pub(crate) struct Progress {
    command_id: String,
    phase: Mutex<String>,
    /// Values replaced in log lines, such as secret build variables.
    secrets: Mutex<Vec<String>>,
    sender: mpsc::Sender<pb::CommandProgress>,
}

/// Shorter values are not redacted, since they would mangle unrelated text.
const MIN_REDACTED_LEN: usize = 4;

impl Progress {
    pub(crate) fn new(command_id: String, sender: mpsc::Sender<pb::CommandProgress>) -> Self {
        Self { command_id, phase: Mutex::default(), secrets: Mutex::default(), sender }
    }

    /// Replaces these values with `[redacted]` in log lines from now on.
    pub(crate) fn redact(&self, secrets: impl IntoIterator<Item = String>) {
        let mut redacted = self.secrets.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        redacted.extend(secrets.into_iter().filter(|secret| secret.len() >= MIN_REDACTED_LEN));
        // Longest first, so a secret containing another is replaced whole.
        redacted.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    }

    /// Moves on to a new step, such as "pulling image".
    pub(crate) fn phase(&self, phase: &str) {
        *self.phase.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = phase.to_string();
        self.send(Vec::new());
    }

    /// A log line for the current step, with redacted values replaced.
    pub(crate) fn line(&self, mut text: String) {
        for secret in self.secrets.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).iter() {
            if text.contains(secret.as_str()) {
                text = text.replace(secret.as_str(), "[redacted]");
            }
        }
        let line = pb::LogLine { at: Some(std::time::SystemTime::now().into()), stream: pb::LogStream::Stdout.into(), text };
        self.send(vec![line]);
    }

    fn send(&self, log_lines: Vec<pb::LogLine>) {
        let phase = self.phase.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        let progress = pb::CommandProgress { command_id: self.command_id.clone(), phase, log_lines };
        let _ = self.sender.try_send(progress);
    }
}

impl Commands {
    /// The runtime commands run against, for sessions.
    pub(crate) fn runtime(&self) -> Arc<dyn ContainerRuntime> {
        self.runtime.clone()
    }

    pub(crate) fn new(runtime: Arc<dyn ContainerRuntime>, build: BuildOptions) -> Self {
        let (progress_sender, progress) = mpsc::channel(PROGRESS_BUFFER);
        Self {
            runtime,
            build: Arc::new(build),
            tasks: JoinSet::new(),
            running: HashMap::new(),
            finished: HashMap::new(),
            finished_order: VecDeque::new(),
            progress_sender,
            progress,
            ready: VecDeque::new(),
        }
    }

    /// The commands still running, for `Hello` after a reconnect.
    pub(crate) fn in_flight(&self) -> Vec<String> {
        self.running.values().cloned().collect()
    }

    /// Whether there is anything to wait for with [`Commands::next`].
    pub(crate) fn busy(&self) -> bool {
        !self.tasks.is_empty() || !self.progress.is_empty() || !self.ready.is_empty()
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
        let progress = Progress::new(id.clone(), self.progress_sender.clone());
        let work = match command.kind {
            Some(Kind::PrepareDeployment(prepare)) => {
                tracing::info!(command = id, deployment = prepare.deployment_id, "preparing a deployment");
                let build = self.build.clone();
                Box::pin(async move { prepare_deployment(&*runtime, &build, prepare, &progress).await }) as Work
            }
            Some(Kind::StartDeployment(start)) => {
                tracing::info!(command = id, deployment = start.deployment_id, "starting a deployment");
                Box::pin(async move { start_deployment(&*runtime, start, &progress).await })
            }
            Some(Kind::StopDeployment(stop)) => {
                tracing::info!(command = id, deployment = stop.deployment_id, "stopping a deployment");
                Box::pin(async move { stop_deployment(&*runtime, stop).await.map(|()| pb::CommandSucceeded::default()) })
            }
            Some(Kind::RemoveDeployment(remove)) => {
                tracing::info!(command = id, deployment = remove.deployment_id, "removing a deployment");
                Box::pin(async move { remove_deployment(&*runtime, remove).await.map(|()| pb::CommandSucceeded::default()) })
            }
            Some(Kind::RestartInstance(restart)) => {
                tracing::info!(command = id, container = restart.container_id, "restarting a container");
                Box::pin(async move { restart_instance(&*runtime, restart).await.map(|()| pb::CommandSucceeded::default()) })
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

    /// The next progress report, or the next command to finish (recorded as finished by
    /// then). A command's progress always comes before its result. Pends while not busy.
    pub(crate) async fn next(&mut self) -> CommandEvent {
        loop {
            if let Ok(progress) = self.progress.try_recv() {
                return CommandEvent::Progress(progress);
            }
            if let Some(finished) = self.ready.pop_front() {
                return CommandEvent::Finished(finished);
            }
            tokio::select! {
                biased;
                Some(progress) = self.progress.recv() => return CommandEvent::Progress(progress),
                // Progress the task sent before it ended is now queued, and goes first.
                joined = self.tasks.join_next_with_id(), if !self.tasks.is_empty() => {
                    let finished = self.record(joined);
                    self.ready.push_back(finished);
                }
            }
        }
    }

    fn record(&mut self, joined: Option<Result<(task::Id, pb::CommandResult), task::JoinError>>) -> pb::CommandResult {
        let (task, finished) = match joined.expect("only joined while tasks run") {
            Ok((task, finished)) => (task, Ok(finished)),
            Err(error) => (error.id(), Err(error)),
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

type Work = std::pin::Pin<Box<dyn Future<Output = Result<pb::CommandSucceeded, pb::CommandFailed>> + Send>>;

fn name(kind: &Kind) -> &'static str {
    match kind {
        Kind::PrepareDeployment(_) => "PrepareDeployment",
        Kind::RunRelease(_) => "RunRelease",
        Kind::StartDeployment(_) => "StartDeployment",
        Kind::StopDeployment(_) => "StopDeployment",
        Kind::RestartInstance(_) => "RestartInstance",
        Kind::OpenSession(_) => "OpenSession",
        Kind::CancelCommand(_) => "CancelCommand",
        Kind::RemoveDeployment(_) => "RemoveDeployment",
    }
}

fn result(command_id: String, outcome: Result<pb::CommandSucceeded, pb::CommandFailed>) -> pb::CommandResult {
    let outcome = match outcome {
        Ok(succeeded) => Outcome::Succeeded(succeeded),
        Err(failed) => Outcome::Failed(failed),
    };
    pb::CommandResult { command_id, outcome: Some(outcome) }
}

pub(crate) fn failure(reason: FailureReason, message: impl Into<String>) -> pb::CommandFailed {
    pb::CommandFailed { reason: reason.into(), message: message.into() }
}

impl From<RuntimeError> for pb::CommandFailed {
    fn from(error: RuntimeError) -> Self {
        failure(FailureReason::Internal, error.to_string())
    }
}

pub(crate) fn grace(period: Option<prost_types::Duration>) -> Duration {
    period.and_then(|period| Duration::try_from(period).ok()).unwrap_or(DEFAULT_GRACE).min(MAX_GRACE)
}

/// Treats a container that is already gone as done.
fn gone_is_done(outcome: Result<(), RuntimeError>) -> Result<(), RuntimeError> {
    match outcome {
        Err(RuntimeError::NotFound) => Ok(()),
        outcome => outcome,
    }
}

/// Parses an ID field of a command.
pub(crate) fn parse_id<T: std::str::FromStr>(field: &str, value: &str) -> Result<T, pb::CommandFailed> {
    value.parse().map_err(|_| failure(FailureReason::Internal, format!("invalid {field} `{value}`")))
}

async fn stop_deployment(runtime: &dyn ContainerRuntime, stop: pb::StopDeployment) -> Result<(), pb::CommandFailed> {
    let deployment = parse_id("deployment ID", &stop.deployment_id)?;
    Ok(stop_containers(runtime, deployment, grace(stop.grace_period)).await?)
}

/// Stops and removes every container of a deployment. Containers already gone count as
/// stopped, so repeating it succeeds.
pub(crate) async fn stop_containers(
    runtime: &dyn ContainerRuntime,
    deployment: DeploymentId,
    grace: Duration,
) -> Result<(), RuntimeError> {
    let containers = runtime.list().await?.into_iter().filter(|instance| instance.deployment_id == deployment);
    for container in containers {
        gone_is_done(runtime.stop(&container.container_id, grace).await)?;
        gone_is_done(runtime.remove(&container.container_id).await)?;
    }
    Ok(())
}

/// Removes a finished deployment's containers, then its images. An image still in use stays,
/// such as one a container outside Spinikus runs.
async fn remove_deployment(runtime: &dyn ContainerRuntime, remove: pb::RemoveDeployment) -> Result<(), pb::CommandFailed> {
    let deployment = parse_id("deployment ID", &remove.deployment_id)?;
    // Its containers stopped long ago, or never ran.
    stop_containers(runtime, deployment, Duration::from_secs(1)).await?;
    for image in &remove.images {
        match runtime.remove_image(image).await {
            Ok(()) => tracing::info!(deployment = %deployment, image, "removed an image"),
            Err(RuntimeError::NotFound) => {}
            Err(RuntimeError::InUse) => tracing::info!(deployment = %deployment, image, "kept an image that is in use"),
            Err(error) => return Err(error.into()),
        }
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
        let finished = async {
            loop {
                if let CommandEvent::Finished(result) = commands.next().await {
                    return result;
                }
            }
        };
        timeout(Duration::from_secs(5), finished).await.expect("a command finished")
    }

    fn builds() -> BuildOptions {
        BuildOptions::in_dir(&std::env::temp_dir().join("spinikus-agent-tests"))
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

    #[test]
    fn redacts_secrets_from_log_lines() {
        let (sender, mut receiver) = mpsc::channel(8);
        let progress = Progress::new("cmd".to_string(), sender);
        progress.redact(["s3cret".to_string(), "s3cret-longer".to_string(), "abc".to_string()]);

        progress.line("token s3cret-longer and s3cret, abc".to_string());

        let line = receiver.try_recv().unwrap().log_lines.remove(0).text;
        assert_eq!(line, "token [redacted] and [redacted], abc");
    }

    #[tokio::test]
    async fn stops_and_removes_a_deployments_containers_only() {
        let runtime = Arc::new(MemoryRuntime::new());
        let (draining, live) = (DeploymentId::new(), DeploymentId::new());
        runtime.put(container("old-1", draining));
        runtime.put(container("old-2", draining));
        runtime.put(container("new", live));
        let mut commands = Commands::new(runtime.clone(), builds());

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
    async fn removes_a_deployments_containers_and_images_but_not_images_in_use() {
        let runtime = Arc::new(MemoryRuntime::new());
        let (old, live) = (DeploymentId::new(), DeploymentId::new());
        runtime.add_image("web:1", "web@sha256:01");
        runtime.add_image("web:2", "web@sha256:02");
        runtime.put(container("old", old));
        let spec = crate::runtime::ContainerSpec {
            name: "spinikus-live".to_string(),
            image: "web@sha256:02".to_string(),
            service_id: ServiceId::new(),
            deployment_id: live,
            env: Vec::new(),
            port: None,
            volumes: Vec::new(),
        };
        runtime.create(&spec).await.unwrap();
        let mut commands = Commands::new(runtime.clone(), builds());
        let images = ["web@sha256:01", "web@sha256:02", "gone@sha256:03"].map(str::to_string).to_vec();
        let remove = pb::RemoveDeployment { deployment_id: old.to_string(), images };

        commands.start(command("cmd-1", Kind::RemoveDeployment(remove)));
        let result = finish(&mut commands).await;

        assert!(succeeded(&result), "{result:?}");
        assert!(runtime.get("old").is_none());
        assert_eq!(runtime.removed_images(), ["web@sha256:01"]);
        assert!(!runtime.has_image("web@sha256:01") && runtime.has_image("web@sha256:02"));
    }

    #[tokio::test]
    async fn a_repeated_command_gets_the_stored_result() {
        let runtime = Arc::new(MemoryRuntime::new());
        let deployment = DeploymentId::new();
        runtime.put(container("c", deployment));
        let mut commands = Commands::new(runtime.clone(), builds());

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
        let mut commands = Commands::new(Arc::new(MemoryRuntime::new()), builds());

        commands.start(command("cmd-1", stop(DeploymentId::new())));

        assert!(succeeded(&finish(&mut commands).await));
    }

    #[tokio::test]
    async fn restarts_a_managed_container() {
        let runtime = Arc::new(MemoryRuntime::new());
        let mut exited = container("c", DeploymentId::new());
        exited.status = InstanceStatus::Exited;
        runtime.put(exited);
        let mut commands = Commands::new(runtime.clone(), builds());

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
        let mut commands = Commands::new(runtime, builds());

        commands.start(command("cmd-1", stop(DeploymentId::new())));
        let result = finish(&mut commands).await;

        assert_eq!(failed(&result).reason, FailureReason::Internal as i32);
        assert!(failed(&result).message.contains("unreachable"), "{result:?}");
    }

    #[tokio::test]
    async fn refuses_commands_it_cannot_run_yet() {
        let mut commands = Commands::new(Arc::new(MemoryRuntime::new()), builds());
        let start = Kind::RunRelease(pb::RunRelease::default());
        let invalid = Kind::StopDeployment(pb::StopDeployment { deployment_id: "web".to_string(), grace_period: None });

        let refused = commands.start(command("cmd-1", start)).unwrap();
        let unknown = commands.start(pb::Command { command_id: "cmd-2".to_string(), kind: None }).unwrap();
        commands.start(command("cmd-3", invalid));
        let invalid = finish(&mut commands).await;

        assert_eq!(failed(&refused).message, "this agent cannot run RunRelease yet");
        assert_eq!(failed(&unknown).message, "this agent does not know this kind of command");
        assert_eq!(failed(&invalid).message, "invalid deployment ID `web`");
    }

    #[tokio::test]
    async fn progress_comes_before_the_result() {
        let runtime = Arc::new(MemoryRuntime::new());
        runtime.add_image("web:1", "web@sha256:01");
        let mut commands = Commands::new(runtime, builds());
        let prepare = pb::PrepareDeployment {
            deployment_id: DeploymentId::new().to_string(),
            source: Some(pb::prepare_deployment::Source::Image(pb::ImageSource { image: "web:1".to_string() })),
            ..Default::default()
        };

        commands.start(command("cmd-1", Kind::PrepareDeployment(prepare)));
        let mut events = Vec::new();
        while commands.busy() {
            events.push(timeout(Duration::from_secs(5), commands.next()).await.unwrap());
        }

        let Some((CommandEvent::Finished(result), progress)) = events.split_last() else { panic!("{events:?}") };
        assert!(succeeded(result));
        let Some(Outcome::Succeeded(succeeded)) = &result.outcome else { unreachable!() };
        assert_eq!(succeeded.prepared.as_ref().unwrap().image, "web@sha256:01");
        let lines: Vec<_> = progress
            .iter()
            .flat_map(|event| match event {
                CommandEvent::Progress(progress) => progress.log_lines.iter().map(|line| line.text.as_str()),
                CommandEvent::Finished(_) => panic!("a second result"),
            })
            .collect();
        assert_eq!(lines, ["Pulling web:1", "Pinned web:1 as web@sha256:01"]);
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
