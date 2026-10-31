//! The deployment runner: moves deployments through the state machine.
//!
//! The database is the state: a deployment's status says what should happen next, so a
//! restarted control plane carries on where it stopped. Each pass over the active
//! deployments starts queued ones (one per service at a time), fails those past their
//! phase's timeout or whose agent is gone, and sends the command for the current phase:
//! prepare, start (which also verifies), or stop for a draining deployment. Commands are
//! sent again when unanswered for a while and when the agent reconnects; the agent
//! recognizes repeats by command ID.
//!
//! Results and progress come back from the agent and move the deployment on: a prepared
//! image leads to starting, a verified start to `live` (which drains the previous live
//! deployment), and a stopped draining deployment to `superseded`. A result only counts
//! from the agent of the server the deployment runs on.
//!
//! Each pass also brings the servers' route tables up to date ([`super::routes`]), and a
//! draining deployment is stopped only once no applied route points at it.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    time::Duration,
};

use jiff::Timestamp;
use spinikus_core::{
    actor::Actor,
    app_file::AppFile,
    deployment::{Deployment, DeploymentSnapshot, DeploymentStatus, DeploymentTrigger, EndReason},
    id::{ServerId, ServiceId},
    server::Server,
};
use spinikus_proto::agent::v1::{self as pb, FailureReason, command_result::Outcome, control_message};
use spinikus_store::{DeploymentRepository, RouteRepository, ServerRepository, StoreError};
use tokio::{
    sync::{Mutex, Notify},
    time::Instant,
};

use super::commands::{Step, parse_command_id, settings};
use crate::{Control, ControlError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeploymentTimings {
    /// How often to look at the active deployments, besides when something happens.
    pub tick: Duration,
    /// How long an unanswered command waits before it is sent again.
    pub resend_after: Duration,
    /// How long a server's agent can be gone before its in-progress deployments fail.
    pub agent_lost_after: Duration,
    /// The longest a deployment may spend `preparing` (pulling or building).
    pub prepare_timeout: Duration,
    /// Time allowed for starting, on top of the longest the health check can take.
    pub start_margin: Duration,
}

impl Default for DeploymentTimings {
    fn default() -> Self {
        Self {
            tick: Duration::from_secs(2),
            resend_after: Duration::from_secs(30),
            agent_lost_after: Duration::from_secs(2 * 60),
            prepare_timeout: Duration::from_secs(30 * 60),
            start_margin: Duration::from_secs(2 * 60),
        }
    }
}

pub(crate) struct Runner {
    pub(crate) timings: DeploymentTimings,
    wake: Notify,
    /// When each unanswered command was last sent, and to which server. Held across a whole
    /// pass or result, so they never interleave.
    sent: Mutex<HashMap<String, (ServerId, Instant)>>,
    /// The route table revision last sent to each server's agent, and when. Locked inside
    /// `sent`, never on its own while waiting for `sent`.
    pub(super) routes_sent: Mutex<HashMap<ServerId, (u64, Instant)>>,
    /// Timeouts and lost agents count from here at the earliest, so a restarted control
    /// plane gives agents time to reconnect and report.
    started_at: Timestamp,
}

impl Runner {
    pub(crate) fn new(timings: DeploymentTimings) -> Self {
        Self { timings, wake: Notify::new(), sent: Mutex::default(), routes_sent: Mutex::default(), started_at: Timestamp::now() }
    }
}

pub(super) type Sent = HashMap<String, (ServerId, Instant)>;

/// The deployment's snapshot with the settings from its `spinikus.json` merged in.
fn with_app_file(deployment: &Deployment, text: &str) -> Result<DeploymentSnapshot, String> {
    let file = AppFile::parse(text).map_err(|error| error.to_string())?;
    let (config, from_file) = file.apply(&deployment.snapshot.config).map_err(|error| error.to_string())?;
    Ok(DeploymentSnapshot { source: deployment.snapshot.source.clone(), config, from_file })
}

/// How long ago `since` was, zero if in the future.
fn elapsed(since: Timestamp) -> Duration {
    Duration::try_from(Timestamp::now().duration_since(since)).unwrap_or_default()
}

impl Control {
    /// Runs deployments until `shutdown` resolves. One runner per control plane.
    pub async fn run_deployments(&self, shutdown: impl Future<Output = ()>) {
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            self.advance_deployments().await;
            tokio::select! {
                () = shutdown.as_mut() => return,
                () = self.runner().wake.notified() => {}
                () = tokio::time::sleep(self.runner().timings.tick) => {}
            }
        }
    }

    /// Makes the runner look at the deployments now.
    pub(crate) fn wake_deployments(&self) {
        self.runner().wake.notify_one();
    }

    /// The agent may have lost what it was sent: send it again now. The revisions are the
    /// route table and certificates it already has.
    pub(crate) async fn agent_connected(&self, server: ServerId, routes_revision: u64, certificates_revision: u64) {
        let mut sent = self.runner().sent.lock().await;
        sent.retain(|_, (to, _)| *to != server);
        self.runner().routes_sent.lock().await.remove(&server);
        drop(sent);
        self.certificates_connected(server, certificates_revision).await;
        self.routes_applied(server, routes_revision).await;
        self.backups_agent_connected(server).await;
    }

    pub(crate) async fn advance_deployments(&self) {
        let mut sent = self.runner().sent.lock().await;
        // Active deployments are never cleaned up, so these include them.
        let uncleaned = match self.store().list_uncleaned_deployments().await {
            Ok(uncleaned) => uncleaned,
            Err(error) => {
                tracing::error!(%error, "failed to list the deployments");
                return;
            }
        };
        let active: Vec<Deployment> = uncleaned
            .iter()
            .filter(|deployment| deployment.status.is_in_progress() || deployment.status == DeploymentStatus::Draining)
            .cloned()
            .collect();
        self.forget_log_servers(&active.iter().map(|deployment| deployment.id).collect::<Vec<_>>()).await;
        // Forget commands of deployments that are cleaned up.
        let ids: HashSet<String> = uncleaned.iter().map(|deployment| deployment.id.to_string()).collect();
        sent.retain(|command, _| command.split_once('/').is_some_and(|(deployment, _)| ids.contains(deployment)));
        // Before draining deployments are looked at, so they see the routes that replace them.
        if let Err(error) = self.sync_routes().await {
            tracing::error!(%error, "failed to update the route tables");
        }

        let mut busy: HashSet<ServiceId> = active
            .iter()
            .filter(|deployment| deployment.status.is_in_progress() && deployment.status != DeploymentStatus::Queued)
            .map(|deployment| deployment.service_id)
            .collect();
        for deployment in active {
            let id = deployment.id;
            if let Err(error) = self.advance(&mut sent, deployment, &mut busy).await {
                tracing::error!(deployment = %id, %error, "failed to advance a deployment");
            }
        }
        if let Err(error) = self.clean_up_deployments(&mut sent, &uncleaned).await {
            tracing::error!(%error, "failed to clean up deployments");
        }
    }

    async fn advance(&self, sent: &mut Sent, deployment: Deployment, busy: &mut HashSet<ServiceId>) -> Result<(), ControlError> {
        let deployment = match deployment.status {
            DeploymentStatus::Queued => {
                // One deployment of a service runs at a time; the newest queued one waits.
                if !busy.insert(deployment.service_id) {
                    return Ok(());
                }
                // A rollback reuses its image, so it skips preparing.
                let next = match (deployment.trigger, &deployment.image) {
                    (DeploymentTrigger::Rollback, Some(_)) => DeploymentStatus::Starting,
                    _ => DeploymentStatus::Preparing,
                };
                tracing::info!(deployment = %deployment.id, service = %deployment.service_id, "starting a deployment");
                self.store().transition_deployment(deployment.id, next, None, None, Actor::System).await?
            }
            // Release commands are not supported yet, so nothing stops here.
            DeploymentStatus::Releasing => {
                self.store().transition_deployment(deployment.id, DeploymentStatus::Starting, None, None, Actor::System).await?
            }
            _ => deployment,
        };

        let server = self.store().get_server(deployment.server_id).await?;
        if deployment.status.is_in_progress() {
            if let Some(problem) = self.agent_lost(&server, &deployment) {
                return self.fail(&deployment, EndReason::AgentLost, &problem, Actor::System).await;
            }
            let limit = self.phase_timeout(&deployment)?;
            if elapsed(deployment.updated_at.max(self.runner().started_at)) > limit {
                let problem = format!("it was {} for longer than {}s", deployment.status, limit.as_secs());
                return self.fail(&deployment, EndReason::Timeout, &problem, Actor::System).await;
            }
        }
        let command = match deployment.status {
            DeploymentStatus::Preparing => self.prepare_command(&deployment).await?,
            DeploymentStatus::Starting | DeploymentStatus::Verifying => self.start_command(&deployment).await?,
            DeploymentStatus::Draining if server.revoked_at.is_some() => {
                // Its containers cannot be stopped any more; it no longer serves anything.
                let superseded = DeploymentStatus::Superseded;
                self.store().transition_deployment(deployment.id, superseded, None, None, Actor::System).await?;
                return Ok(());
            }
            DeploymentStatus::Draining => {
                // Wait until the proxy no longer sends it requests.
                if let Some(routes) = self.store().get_route_table(deployment.server_id).await?
                    && (!routes.applied() || routes.table.targets(deployment.id))
                {
                    return Ok(());
                }
                self.stop_command(&deployment, Step::Stop)?
            }
            _ => return Ok(()),
        };
        self.send_command(sent, deployment.server_id, command).await;
        Ok(())
    }

    /// Sends a command unless it was sent recently. A disconnected agent gets it later.
    pub(super) async fn send_command(&self, sent: &mut Sent, server: ServerId, command: pb::Command) {
        let id = command.command_id.clone();
        if let Some((to, at)) = sent.get(&id)
            && *to == server
            && at.elapsed() < self.runner().timings.resend_after
        {
            return;
        }
        let message = pb::ControlMessage { message: Some(control_message::Message::Command(command)) };
        if self.agents().send(server, message).await.is_ok() {
            tracing::debug!(server = %server, command = id, "sent a command");
            sent.insert(id, (server, Instant::now()));
        }
    }

    /// Why the deployment's agent counts as lost, if it does: its server was revoked, or it
    /// has been disconnected for too long.
    fn agent_lost(&self, server: &Server, deployment: &Deployment) -> Option<String> {
        if server.revoked_at.is_some() {
            return Some(format!("server `{}` was revoked", server.name));
        }
        if self.agents().is_connected(server.id) {
            return None;
        }
        let since = server.last_seen_at.map_or(deployment.updated_at, |seen| seen.max(deployment.updated_at));
        let since = since.max(self.runner().started_at);
        let limit = self.runner().timings.agent_lost_after;
        (elapsed(since) > limit)
            .then(|| format!("the agent of server `{}` was disconnected for more than {}s", server.name, limit.as_secs()))
    }

    fn phase_timeout(&self, deployment: &Deployment) -> Result<Duration, ControlError> {
        let timings = self.runner().timings;
        Ok(match deployment.status {
            DeploymentStatus::Preparing => timings.prepare_timeout,
            _ => settings(deployment)?.health_check().longest_run() + timings.start_margin,
        })
    }

    async fn fail(&self, deployment: &Deployment, reason: EndReason, problem: &str, actor: Actor) -> Result<(), ControlError> {
        tracing::warn!(deployment = %deployment.id, %reason, problem, "deployment failed");
        let failed = DeploymentStatus::Failed;
        self.store().transition_deployment(deployment.id, failed, Some(reason), Some(problem), actor).await?;
        self.log_line(deployment.id, &format!("failed ({reason}): {problem}")).await;
        Ok(())
    }

    /// Stops a deployment's containers, such as once it is canceled. Best effort: a
    /// disconnected agent keeps them.
    pub(crate) async fn stop_containers(&self, deployment: &Deployment) -> Result<(), ControlError> {
        let command = self.stop_command(deployment, Step::Stop)?;
        let mut sent = self.runner().sent.lock().await;
        self.send_command(&mut sent, deployment.server_id, command).await;
        Ok(())
    }

    /// Progress from an agent: the phase can move a starting deployment to `verifying`.
    pub(crate) async fn deployment_progress(&self, server: ServerId, progress: pb::CommandProgress) {
        let Some((id, step)) = parse_command_id(&progress.command_id) else {
            return;
        };
        if matches!(step, Step::Prepare | Step::Start) {
            self.log_progress(server, id, &progress).await;
        }
        if step != Step::Start || progress.phase != "verifying" {
            return;
        }
        let _sent = self.runner().sent.lock().await;
        match self.store().get_deployment(id).await {
            Ok(deployment) if deployment.server_id == server && deployment.status == DeploymentStatus::Starting => {
                let verifying = DeploymentStatus::Verifying;
                if let Err(error) = self.store().transition_deployment(id, verifying, None, None, Actor::Server(server)).await {
                    tracing::error!(deployment = %id, %error, "failed to record a deployment's progress");
                }
            }
            Ok(_) | Err(StoreError::NotFound { .. }) => {}
            Err(error) => tracing::error!(deployment = %id, %error, "failed to record a deployment's progress"),
        }
    }

    /// A command's result from an agent.
    pub(crate) async fn deployment_result(&self, server: ServerId, result: pb::CommandResult) {
        let command = result.command_id.clone();
        if let Err(error) = self.apply_result(server, result).await {
            tracing::error!(server = %server, command, %error, "failed to apply a command's result");
        }
        self.wake_deployments();
    }

    async fn apply_result(&self, server: ServerId, result: pb::CommandResult) -> Result<(), ControlError> {
        let Some((id, step)) = parse_command_id(&result.command_id) else {
            tracing::debug!(server = %server, command = result.command_id, "ignored the result of an unknown command");
            return Ok(());
        };
        let mut sent = self.runner().sent.lock().await;
        let deployment = match self.store().get_deployment(id).await {
            Ok(deployment) => deployment,
            Err(StoreError::NotFound { .. }) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        // Only the agent running the deployment speaks for it.
        if deployment.server_id != server {
            tracing::warn!(server = %server, deployment = %id, "ignored a result for a deployment on another server");
            return Ok(());
        }
        sent.remove(&result.command_id);
        let actor = Actor::Server(server);
        let status = deployment.status;
        match (step, result.outcome, status) {
            (Step::Prepare, Some(Outcome::Succeeded(succeeded)), DeploymentStatus::Preparing) => {
                let prepared = succeeded.prepared.unwrap_or_default();
                if prepared.image.is_empty() {
                    return self.fail(&deployment, EndReason::BuildFailed, "the agent reported no image", actor).await;
                }
                let commit = Some(prepared.commit_sha).filter(|commit| !commit.is_empty());
                let snapshot = match prepared.app_file.as_str() {
                    "" => None,
                    text => match with_app_file(&deployment, text) {
                        Ok(snapshot) => Some(snapshot),
                        Err(problem) => return self.fail(&deployment, EndReason::BuildFailed, &problem, actor).await,
                    },
                };
                self.store().record_build(id, commit, Some(prepared.image), snapshot).await?;
                let starting = self.store().transition_deployment(id, DeploymentStatus::Starting, None, None, actor).await?;
                // Start right away rather than on the next pass.
                self.advance(&mut sent, starting, &mut HashSet::new()).await?;
            }
            (Step::Prepare, Some(Outcome::Failed(failed)), DeploymentStatus::Preparing) => {
                let reason = match failed.reason() {
                    FailureReason::Timeout => EndReason::Timeout,
                    _ => EndReason::BuildFailed,
                };
                self.fail(&deployment, reason, &failed.message, actor).await?;
            }
            (Step::Start, Some(Outcome::Succeeded(_)), DeploymentStatus::Starting | DeploymentStatus::Verifying) => {
                if status == DeploymentStatus::Starting {
                    self.store().transition_deployment(id, DeploymentStatus::Verifying, None, None, actor).await?;
                }
                self.store().transition_deployment(id, DeploymentStatus::Live, None, None, actor).await?;
                tracing::info!(deployment = %id, service = %deployment.service_id, "deployment is live");
            }
            (Step::Start, Some(Outcome::Failed(failed)), DeploymentStatus::Starting | DeploymentStatus::Verifying) => {
                let reason = match (failed.reason(), status) {
                    (FailureReason::Timeout, _) => EndReason::Timeout,
                    (FailureReason::HealthCheckFailed, _) | (_, DeploymentStatus::Verifying) => EndReason::HealthCheckFailed,
                    _ => EndReason::StartFailed,
                };
                // A failed health check ends a deployment that is verifying.
                if reason == EndReason::HealthCheckFailed && status == DeploymentStatus::Starting {
                    self.store().transition_deployment(id, DeploymentStatus::Verifying, None, None, actor).await?;
                }
                self.fail(&deployment, reason, &failed.message, actor).await?;
            }
            (Step::Stop, Some(Outcome::Succeeded(_)), DeploymentStatus::Draining) => {
                self.store().transition_deployment(id, DeploymentStatus::Superseded, None, None, actor).await?;
            }
            (Step::Stop, Some(Outcome::Failed(failed)), DeploymentStatus::Draining) => {
                // Sent again after a while.
                tracing::warn!(deployment = %id, problem = failed.message, "failed to stop a draining deployment");
            }
            (Step::Remove, Some(Outcome::Succeeded(_)), _) => {
                self.store().mark_deployment_cleaned(id).await?;
                tracing::info!(deployment = %id, "cleaned up a deployment");
            }
            (Step::Remove, Some(Outcome::Failed(failed)), _) => {
                // Sent again after a while.
                tracing::warn!(deployment = %id, problem = failed.message, "failed to clean up a deployment");
            }
            // Started after it was canceled: stop what it started.
            (Step::Start, Some(Outcome::Succeeded(_)), DeploymentStatus::Canceled) => {
                let command = self.stop_command(&deployment, Step::Cleanup)?;
                self.send_command(&mut sent, server, command).await;
            }
            _ => tracing::debug!(deployment = %id, ?step, %status, "ignored a result for a deployment that has moved on"),
        }
        Ok(())
    }
}
