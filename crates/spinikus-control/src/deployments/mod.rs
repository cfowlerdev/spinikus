//! Deployments. Developers deploy a service and cancel its deployments; viewers see them
//! and their events. The runner ([`runner`]) then moves each deployment through the state
//! machine, by sending commands to the agent of the server it runs on.
//!
//! A deployment records what it deploys when it is created: the service's source and config
//! (the snapshot), its resolved variables, and the server it runs on (the service's
//! placement, or the embedded server). Later edits to the service never change it.

mod commands;
mod runner;

use serde_json::json;
use spinikus_core::{
    deployment::{
        Deployment, DeploymentEvent, DeploymentSnapshot, DeploymentStatus, DeploymentStrategy, DeploymentTrigger, EndReason,
        NewDeployment,
    },
    environment::Environment,
    id::{DeploymentId, ServerId, ServiceId},
    membership::Role,
    runtime::RuntimeSettings,
    service::Service,
};
use spinikus_store::{
    DeploymentRepository, ServerRepository, SourceRepository, StoreError, VariableRepository, VolumeRepository,
};

pub use runner::DeploymentTimings;
pub(crate) use runner::Runner;

use crate::{
    Control, ControlError, Principal,
    access::not_found,
    page::{self, Page},
};

impl Control {
    /// Queues a deployment of the service as it is now. A deployment already queued for the
    /// service is canceled (`superseded_in_queue`); one in progress finishes first.
    pub async fn create_deployment(&self, principal: &Principal, service_id: ServiceId) -> Result<Deployment, ControlError> {
        let (service, environment) = self.service_with_access(principal, service_id, Role::Developer).await?;
        let source = match self.store().get_source(service_id).await {
            Ok(source) => source,
            Err(StoreError::NotFound { .. }) => {
                return Err(ControlError::Invalid {
                    field: "source",
                    problem: "the service has no source to deploy".to_string(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        RuntimeSettings::from_config(&service.config)
            .map_err(|error| ControlError::Invalid { field: "config.runtime", problem: error.to_string() })?;
        let server_id = self.placement(&service).await?;
        let has_volumes = !self.store().list_volumes(service_id).await?.is_empty();
        let new = NewDeployment {
            service_id,
            server_id,
            trigger: DeploymentTrigger::Manual,
            strategy: DeploymentStrategy::default_for(service.kind, has_volumes),
            rollback_of: None,
            snapshot: DeploymentSnapshot { source: source.spec, config: service.config },
            commit_sha: None,
            image: None,
            variables: self.store().resolve_service_variables(service_id).await?,
        };
        let deployment = self.store().create_deployment(new, principal.actor()).await?;
        let details = json!({ "service_id": service_id.to_string(), "server_id": server_id.to_string() });
        self.audit_by(principal, "deployment.create", deployment.id, Some(environment.project_id), details).await?;
        self.wake_deployments();
        Ok(deployment)
    }

    /// The service's deployments, newest first. The next page starts after the last one.
    pub async fn list_deployments(
        &self,
        principal: &Principal,
        service_id: ServiceId,
        after: Option<DeploymentId>,
        limit: u32,
    ) -> Result<Page<Deployment>, ControlError> {
        let limit = page::limit(limit);
        self.service_with_access(principal, service_id, Role::Viewer).await?;
        let items = self.store().list_deployments(service_id, after, limit + 1).await?;
        Ok(Page::from_overfetch(items, limit))
    }

    pub async fn get_deployment(&self, principal: &Principal, id: DeploymentId) -> Result<Deployment, ControlError> {
        let (deployment, _) = self.deployment_with_access(principal, id, Role::Viewer).await?;
        Ok(deployment)
    }

    /// Every status change of the deployment, oldest first, with failure messages.
    pub async fn list_deployment_events(
        &self,
        principal: &Principal,
        id: DeploymentId,
    ) -> Result<Vec<DeploymentEvent>, ControlError> {
        self.deployment_with_access(principal, id, Role::Viewer).await?;
        Ok(self.store().list_deployment_events(id).await?)
    }

    /// Cancels a deployment that has not gone live, and stops any containers it started.
    pub async fn cancel_deployment(&self, principal: &Principal, id: DeploymentId) -> Result<Deployment, ControlError> {
        let (deployment, environment) = self.deployment_with_access(principal, id, Role::Developer).await?;
        let canceled = DeploymentStatus::Canceled;
        let reason = Some(EndReason::CanceledByUser);
        let canceled = self.store().transition_deployment(id, canceled, reason, None, principal.actor()).await?;
        self.audit_by(principal, "deployment.cancel", id, Some(environment.project_id), json!({})).await?;
        if deployment.status != DeploymentStatus::Queued {
            self.stop_containers(&canceled).await?;
        }
        Ok(canceled)
    }

    /// Where the service's next deployment runs: its placement, or the embedded server.
    async fn placement(&self, service: &Service) -> Result<ServerId, ControlError> {
        let server = match service.server_id {
            Some(server_id) => Some(self.store().get_server(server_id).await?),
            None => self.store().find_embedded_server().await?,
        };
        match server {
            Some(server) if server.revoked_at.is_none() => Ok(server.id),
            Some(server) => Err(ControlError::Invalid {
                field: "server_id",
                problem: format!("server `{}` is revoked: place the service on another server", server.name),
            }),
            None => Err(ControlError::Invalid {
                field: "server_id",
                problem: "the service is not placed on a server, and there is no embedded server".to_string(),
            }),
        }
    }

    /// The deployment and its service's environment, if the principal has `needed` there.
    async fn deployment_with_access(
        &self,
        principal: &Principal,
        id: DeploymentId,
        needed: Role,
    ) -> Result<(Deployment, Environment), ControlError> {
        let deployment = self.store().get_deployment(id).await?;
        match self.service_with_access(principal, deployment.service_id, needed).await {
            Err(ControlError::NotFound { .. }) => Err(not_found("deployment", id)),
            Err(error) => Err(error),
            Ok((_, environment)) => Ok((deployment, environment)),
        }
    }
}

#[cfg(test)]
mod tests;
