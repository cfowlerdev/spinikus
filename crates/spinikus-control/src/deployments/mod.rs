//! Deployments. Developers deploy a service and cancel its deployments; viewers see them
//! and their events. The runner ([`runner`]) then moves each deployment through the state
//! machine, by sending commands to the agent of the server it runs on.
//!
//! A deployment records what it deploys when it is created: the service's source and config
//! (the snapshot), its resolved variables, and the server it runs on (the service's
//! placement, or the embedded server). Later edits to the service never change it.

mod cleanup;
mod commands;
pub(crate) mod logs;
mod routes;
mod runner;
pub mod stream;

use serde_json::json;
use spinikus_core::{
    actor::Actor,
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
        let deployment = self.queue_deployment(&service, DeploymentTrigger::Manual, None, principal.actor()).await?;
        let details = json!({ "service_id": service_id.to_string(), "server_id": deployment.server_id.to_string() });
        self.audit_by(principal, "deployment.create", deployment.id, Some(environment.project_id), details).await?;
        Ok(deployment)
    }

    /// Queues a deployment of the service as it is now, of `commit` if given (else the
    /// branch's latest), and wakes the runner. Access is the caller's to check.
    pub(crate) async fn queue_deployment(
        &self,
        service: &Service,
        trigger: DeploymentTrigger,
        commit_sha: Option<String>,
        actor: Actor,
    ) -> Result<Deployment, ControlError> {
        let service_id = service.id;
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
        let server_id = self.placement(service).await?;
        let has_volumes = !self.store().list_volumes(service_id).await?.is_empty();
        let new = NewDeployment {
            service_id,
            server_id,
            trigger,
            strategy: DeploymentStrategy::default_for(service.kind, has_volumes),
            rollback_of: None,
            snapshot: DeploymentSnapshot { source: source.spec, config: service.config.clone(), from_file: Vec::new() },
            commit_sha,
            image: None,
            variables: self.store().resolve_service_variables(service_id).await?,
        };
        let deployment = self.store().create_deployment(new, actor).await?;
        self.wake_deployments();
        Ok(deployment)
    }

    /// Queues a rollback: a new deployment that runs an earlier one's image, snapshot and
    /// variables again on its server, without building, starting at `starting`. The target
    /// must have gone live, been replaced, and still have its image (see `cleanup.rs`);
    /// without one, it is the newest such deployment.
    pub async fn rollback_service(
        &self,
        principal: &Principal,
        service_id: ServiceId,
        target: Option<DeploymentId>,
    ) -> Result<Deployment, ControlError> {
        let (service, environment) = self.service_with_access(principal, service_id, Role::Developer).await?;
        let invalid = |problem: String| ControlError::Invalid { field: "deployment_id", problem };
        let target = match target {
            Some(id) => match self.store().get_deployment(id).await {
                Ok(target) if target.service_id == service_id => target,
                Ok(_) | Err(StoreError::NotFound { .. }) => return Err(not_found("deployment", id)),
                Err(error) => return Err(error.into()),
            },
            None => self
                .rollback_target(service_id)
                .await?
                .ok_or_else(|| invalid("the service has no earlier deployment to roll back to".to_string()))?,
        };
        match (target.status, &target.image, target.cleaned_at) {
            (DeploymentStatus::Live, _, _) => return Err(invalid(format!("deployment {} is live already", target.id))),
            (DeploymentStatus::Superseded, Some(_), None) => {}
            (DeploymentStatus::Superseded, _, _) => {
                return Err(invalid(format!(
                    "deployment {} is no longer kept, so its image is gone; the releases_kept setting says how many are",
                    target.id
                )));
            }
            (status, _, _) => {
                return Err(invalid(format!(
                    "deployment {} is {status}; only a deployment that was live and was replaced can be rolled back to",
                    target.id
                )));
            }
        }
        let server = self.store().get_server(target.server_id).await?;
        if server.revoked_at.is_some() {
            return Err(invalid(format!("deployment {} ran on server `{}`, which is revoked", target.id, server.name)));
        }
        let has_volumes = !self.store().list_volumes(service_id).await?.is_empty();
        let new = NewDeployment {
            service_id,
            server_id: target.server_id,
            trigger: DeploymentTrigger::Rollback,
            strategy: DeploymentStrategy::default_for(service.kind, has_volumes),
            rollback_of: Some(target.id),
            snapshot: target.snapshot.clone(),
            commit_sha: target.commit_sha.clone(),
            image: target.image.clone(),
            variables: self.store().deployment_variables(target.id).await?,
        };
        let deployment = self.store().create_deployment(new, principal.actor()).await?;
        let details = json!({ "service_id": service_id.to_string(), "rollback_of": target.id.to_string() });
        self.audit_by(principal, "deployment.rollback", deployment.id, Some(environment.project_id), details).await?;
        self.wake_deployments();
        Ok(deployment)
    }

    /// The newest deployment of the service that was replaced and still has its image.
    async fn rollback_target(&self, service_id: ServiceId) -> Result<Option<Deployment>, ControlError> {
        let mut after = None;
        loop {
            let deployments = self.store().list_deployments(service_id, after, page::MAX_PAGE_SIZE).await?;
            let found = deployments.iter().find(|deployment| {
                deployment.status == DeploymentStatus::Superseded && deployment.image.is_some() && deployment.cleaned_at.is_none()
            });
            if found.is_some() || deployments.len() < page::MAX_PAGE_SIZE as usize {
                return Ok(found.cloned());
            }
            after = deployments.last().map(|deployment| deployment.id);
        }
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
        self.log_line(id, "canceled").await;
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
pub(crate) mod tests;
