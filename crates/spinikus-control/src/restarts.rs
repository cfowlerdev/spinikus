//! Restarting a service: its live container is restarted in place by the agent of its
//! server, stopped with the service's shutdown timeout first. Nothing about the deployment
//! changes, so variables and settings changed since it was deployed need a deployment
//! instead. The caller waits for the agent's answer: restarts are short, and are not
//! resent, since a restart that finished while its answer was lost needs no second one.

use std::{collections::HashMap, sync::Mutex, time::Duration};

use serde_json::json;
use spinikus_core::{
    deployment::Deployment,
    id::{ServerId, ServiceId},
    membership::Role,
};
use spinikus_proto::agent::v1::{self as pb, command::Kind, command_result::Outcome, control_message};
use spinikus_store::{DeploymentRepository, InstanceRepository};
use tokio::sync::oneshot;

use crate::{Control, ControlError, Principal, deployments::commands::settings};

/// Command IDs of restarts start with this.
const PREFIX: &str = "restart/";

/// How long a restart may take on top of the shutdown timeout.
pub const RESTART_MARGIN: Duration = Duration::from_secs(30);

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

/// Whether a command's result is a restart's.
pub(crate) fn is_restart_command(command_id: &str) -> bool {
    command_id.starts_with(PREFIX)
}

impl Control {
    /// Restarts the service's live container and returns its live deployment once the agent
    /// has. Developers may restart.
    pub async fn restart_service(&self, principal: &Principal, service_id: ServiceId) -> Result<Deployment, ControlError> {
        let (_, environment) = self.service_with_access(principal, service_id, Role::Developer).await?;
        let deployment = self.store().live_deployment(service_id).await?.ok_or_else(|| ControlError::Rejected {
            entity: "service",
            id: service_id.to_string(),
            problem: "it has nothing live to restart; deploy it instead".to_string(),
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
        let command_id = format!("{PREFIX}{}/{}", deployment.id, rand::random::<u64>());
        let (done, answered) = oneshot::channel();
        self.restarts.lock().insert(command_id.clone(), Pending { server: deployment.server_id, done });
        let restart = pb::RestartInstance { container_id: container, grace_period: prost_types::Duration::try_from(grace).ok() };
        let command = pb::Command { command_id: command_id.clone(), kind: Some(Kind::RestartInstance(restart)) };
        let message = pb::ControlMessage { message: Some(control_message::Message::Command(command)) };
        let outcome = match self.agents().send(deployment.server_id, message).await {
            Ok(()) => tokio::time::timeout(grace + RESTART_MARGIN, answered).await.ok().and_then(Result::ok),
            Err(_) => None,
        };
        self.restarts.lock().remove(&command_id);
        match outcome {
            Some(Ok(())) => {}
            Some(Err(problem)) => return Err(rejected(format!("the agent could not restart it: {problem}"))),
            None => return Err(rejected("the agent of its server did not answer in time".to_string())),
        }
        let details = json!({ "deployment_id": deployment.id.to_string() });
        self.audit_by(principal, "service.restart", service_id, Some(environment.project_id), details).await?;
        Ok(deployment)
    }

    /// An agent's answer to a restart. Only the server it was sent to may answer.
    pub(crate) fn restart_result(&self, server: ServerId, result: pb::CommandResult) {
        let mut pending = self.restarts.lock();
        if pending.get(&result.command_id).is_none_or(|waiting| waiting.server != server) {
            tracing::debug!(server = %server, command = result.command_id, "ignored the result of an unknown restart");
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
