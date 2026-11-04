//! Restarting, stopping and starting a service: its live container is restarted, stopped or
//! started in place by the agent of its server, stopped with the service's shutdown timeout.
//! Nothing about the deployment changes, so variables and settings changed since it was
//! deployed need a deployment instead. A stopped service is marked (`Service::stopped_at`)
//! until it is started or another deployment goes live, which starts it again. The caller
//! waits for the agent's answer: these are short, and are not resent, since one that finished
//! while its answer was lost needs no second one.

use std::{collections::HashMap, sync::Mutex, time::Duration};

use serde_json::json;
use spinikus_core::{
    deployment::Deployment,
    id::{ServerId, ServiceId},
    membership::Role,
    service::Service,
};
use spinikus_proto::agent::v1::{self as pb, command::Kind, command_result::Outcome, control_message};
use spinikus_store::{DeploymentRepository, InstanceRepository, ServiceRepository};
use tokio::sync::oneshot;

use crate::{Control, ControlError, Principal, deployments::commands::settings};

/// Command IDs of restarts, stops and starts start with one of these.
const PREFIXES: [&str; 3] = ["restart/", "stop/", "start/"];

/// How long a restart, stop or start may take on top of the shutdown timeout.
pub const RESTART_MARGIN: Duration = Duration::from_secs(30);

/// What to do to the live container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Restart,
    Stop,
    Start,
}

impl Action {
    fn verb(self) -> &'static str {
        match self {
            Action::Restart => "restart",
            Action::Stop => "stop",
            Action::Start => "start",
        }
    }
}

/// Restarts waiting for their agent's answer, by command ID.
#[derive(Default)]
pub(crate) struct Restarts {
    pending: Mutex<HashMap<String, Pending>>,
}

struct Pending {
    server: ServerId,
    done: oneshot::Sender<Result<(), String>>,
}

impl Restarts {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Whether a command's result is a restart's, a stop's or a start's.
pub(crate) fn is_restart_command(command_id: &str) -> bool {
    PREFIXES.iter().any(|prefix| command_id.starts_with(prefix))
}

impl Control {
    /// Restarts the service's live container and returns its live deployment once the agent
    /// has. Developers may restart; a stopped service is started instead.
    pub async fn restart_service(&self, principal: &Principal, service_id: ServiceId) -> Result<Deployment, ControlError> {
        let (service, environment) = self.service_with_access(principal, service_id, Role::Developer).await?;
        if service.stopped_at.is_some() {
            return Err(ControlError::Rejected {
                entity: "service",
                id: service_id.to_string(),
                problem: "it is stopped; start it instead".to_string(),
            });
        }
        let deployment = self.act_on_instance(service_id, Action::Restart).await?;
        let details = json!({ "deployment_id": deployment.id.to_string() });
        self.audit_by(principal, "service.restart", service_id, Some(environment.project_id), details).await?;
        Ok(deployment)
    }

    /// Stops the service's live container, leaving it stopped until the service is started
    /// or another deployment goes live. Developers may stop; stopping a stopped service stops
    /// its container again, in case it was started some other way.
    pub async fn stop_service(&self, principal: &Principal, service_id: ServiceId) -> Result<Service, ControlError> {
        let (_, environment) = self.service_with_access(principal, service_id, Role::Developer).await?;
        let deployment = self.act_on_instance(service_id, Action::Stop).await?;
        let service = self.store().set_service_stopped(service_id, true).await?;
        let details = json!({ "deployment_id": deployment.id.to_string() });
        self.audit_by(principal, "service.stop", service_id, Some(environment.project_id), details).await?;
        Ok(service)
    }

    /// Starts the service's live container again. Developers may start.
    pub async fn start_service(&self, principal: &Principal, service_id: ServiceId) -> Result<Service, ControlError> {
        let (_, environment) = self.service_with_access(principal, service_id, Role::Developer).await?;
        let deployment = self.act_on_instance(service_id, Action::Start).await?;
        let service = self.store().set_service_stopped(service_id, false).await?;
        let details = json!({ "deployment_id": deployment.id.to_string() });
        self.audit_by(principal, "service.start", service_id, Some(environment.project_id), details).await?;
        Ok(service)
    }

    /// Has the agent do `action` to the live deployment's container, and waits for it.
    async fn act_on_instance(&self, service_id: ServiceId, action: Action) -> Result<Deployment, ControlError> {
        let verb = action.verb();
        let deployment = self.store().live_deployment(service_id).await?.ok_or_else(|| ControlError::Rejected {
            entity: "service",
            id: service_id.to_string(),
            problem: match action {
                Action::Stop => "it has nothing live to stop".to_string(),
                _ => format!("it has nothing live to {verb}; deploy it instead"),
            },
        })?;
        let rejected = |problem: String| ControlError::Rejected { entity: "deployment", id: deployment.id.to_string(), problem };
        let container = self
            .store()
            .list_server_instances(deployment.server_id)
            .await?
            .into_iter()
            .find(|instance| instance.observed.deployment_id == deployment.id)
            .map(|instance| instance.observed.container_id)
            .ok_or_else(|| rejected("it has no container on its server".to_string()))?;
        if !self.agents().is_connected(deployment.server_id) {
            return Err(rejected("the agent of its server is not connected".to_string()));
        }

        let grace = settings(&deployment)?.shutdown_timeout();
        let grace_period = prost_types::Duration::try_from(grace).ok();
        let kind = match action {
            Action::Restart => Kind::RestartInstance(pb::RestartInstance { container_id: container, grace_period }),
            Action::Stop => Kind::StopInstance(pb::StopInstance { container_id: container, grace_period }),
            Action::Start => Kind::StartInstance(pb::StartInstance { container_id: container }),
        };
        let command_id = format!("{verb}/{}/{}", deployment.id, rand::random::<u64>());
        let (done, answered) = oneshot::channel();
        self.restarts.lock().insert(command_id.clone(), Pending { server: deployment.server_id, done });
        let command = pb::Command { command_id: command_id.clone(), kind: Some(kind) };
        let message = pb::ControlMessage { message: Some(control_message::Message::Command(command)) };
        let outcome = match self.agents().send(deployment.server_id, message).await {
            Ok(()) => tokio::time::timeout(grace + RESTART_MARGIN, answered).await.ok().and_then(Result::ok),
            Err(_) => None,
        };
        self.restarts.lock().remove(&command_id);
        match outcome {
            Some(Ok(())) => Ok(deployment),
            Some(Err(problem)) => Err(rejected(format!("the agent could not {verb} it: {problem}"))),
            None => Err(rejected("the agent of its server did not answer in time".to_string())),
        }
    }

    /// An agent's answer to a restart, stop or start. Only the server it was sent to may answer.
    pub(crate) fn restart_result(&self, server: ServerId, result: pb::CommandResult) {
        let mut pending = self.restarts.lock();
        if pending.get(&result.command_id).is_none_or(|waiting| waiting.server != server) {
            tracing::debug!(server = %server, command = result.command_id, "ignored the result of an unknown restart, stop or start");
            return;
        }
        let waiting = pending.remove(&result.command_id).expect("checked above");
        let outcome = match result.outcome {
            Some(Outcome::Succeeded(_)) => Ok(()),
            Some(Outcome::Failed(failed)) => Err(failed.message),
            None => Err("no outcome".to_string()),
        };
        let _ = waiting.done.send(outcome);
    }
}

#[cfg(test)]
mod tests;
