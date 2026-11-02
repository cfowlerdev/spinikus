//! Previews of pull requests. An environment with `previews` on gets, for each pull request
//! to the branch its Git service deploys, a preview environment (`pr-<number>`) copied from
//! it: every service with its source, settings, placement, volumes (new and empty), deploy key
//! and variables, and the environment's variables, but no domains, webhooks or backup
//! schedules, so the preview is reached at its generated hostnames. Copies of Git services of
//! the same repository build the pull request's branch; everything else deploys what it does
//! in the base environment. Deliveries arrive through the services' webhooks, like pushes,
//! with the webhook as the actor.
//!
//! New commits redeploy the preview's copies of that repository's services at the newest
//! commit, and closing (or merging) the pull request deletes the preview: its services, so
//! the runner stops and removes their containers, and their volumes with their data, then the
//! environment. Pull requests from forks get no preview, since their code would run with the
//! environment's secrets. A project admin can also delete a preview by hand; the next commit
//! to the pull request makes it again.

use serde_json::json;
use spinikus_core::{
    actor::Actor,
    audit::NewAuditEntry,
    deployment::{Deployment, DeploymentTrigger},
    environment::{Environment, EnvironmentFields, EnvironmentKind},
    id::{EnvironmentId, ProjectId, ServiceId},
    service::{Service, ServiceFields},
    source::{GitSource, SourceSpec},
    variable::{VariableFields, VariableScope},
    volume::VolumeFields,
    webhook::{GitProvider, PullRequest, PullRequestAction, Webhook},
};
use spinikus_store::{
    DeployKeyRepository, DomainRepository, EnvironmentRepository, ServiceRepository, SourceRepository, StoreError,
    VariableRepository, VolumeRepository,
};

use crate::{Control, ControlError, page::MAX_PAGE_SIZE, webhooks::PushOutcome};

impl Control {
    /// Handles a pull request event delivered to `webhook`.
    pub(crate) async fn handle_pull_request(
        &self,
        provider: GitProvider,
        webhook: &Webhook,
        body: &[u8],
    ) -> Result<PushOutcome, ControlError> {
        let request = PullRequest::parse(provider, body)
            .map_err(|error| ControlError::Invalid { field: "body", problem: error.to_string() })?;
        let service = self.store().get_service(webhook.service_id).await?;
        let base = self.store().get_environment(service.environment_id).await?;
        let git = match self.store().get_source(service.id).await {
            Ok(source) => match source.spec {
                SourceSpec::Git(git) => git,
                SourceSpec::Image(_) => return Ok(PushOutcome::Ignored("the service deploys an image now".to_string())),
            },
            Err(StoreError::NotFound { .. }) => return Ok(PushOutcome::Ignored("the service has no source".to_string())),
            Err(error) => return Err(error.into()),
        };
        let number = request.number;
        if request.base_branch != git.branch {
            let to = &request.base_branch;
            return Ok(PushOutcome::Ignored(format!(
                "pull request #{number} is to `{to}`; the service deploys `{}`",
                git.branch
            )));
        }
        let actor = Actor::Webhook(webhook.id);
        match request.action {
            PullRequestAction::Other => Ok(PushOutcome::Ignored(
                "only opening, updating, reopening and closing a pull request change its preview".to_string(),
            )),
            PullRequestAction::Closed => {
                let _previews = self.previews.lock().await;
                match self.store().find_preview_environment(base.project_id, number).await? {
                    Some(preview) if preview.base_environment_id == Some(base.id) => {
                        self.remove_preview(&preview, actor).await?;
                        Ok(PushOutcome::PreviewRemoved(Box::new(preview)))
                    }
                    Some(preview) => Ok(PushOutcome::Ignored(format!("{} is not a preview of `{}`", preview.slug, base.slug))),
                    None => Ok(PushOutcome::Ignored(format!("pull request #{number} has no preview"))),
                }
            }
            PullRequestAction::Updated if !base.previews => {
                Ok(PushOutcome::Ignored(format!("previews of `{}` are off", base.slug)))
            }
            PullRequestAction::Updated if request.from_fork => Ok(PushOutcome::Ignored(
                "pull requests from forks get no preview, since their code would run with the environment's secrets".to_string(),
            )),
            PullRequestAction::Updated => self.deploy_preview(&base, &git, &request, actor).await,
        }
    }

    /// Makes the preview if there is none, then deploys the head commit to its copies of the
    /// repository's services (and, in a new preview, everything else once).
    async fn deploy_preview(
        &self,
        base: &Environment,
        git: &GitSource,
        request: &PullRequest,
        actor: Actor,
    ) -> Result<PushOutcome, ControlError> {
        let _previews = self.previews.lock().await;
        let number = request.number;
        let (preview, created) = match self.store().find_preview_environment(base.project_id, number).await? {
            Some(preview) if preview.base_environment_id == Some(base.id) => (preview, false),
            Some(preview) => {
                return Ok(PushOutcome::Ignored(format!("{} is not a preview of `{}`", preview.slug, base.slug)));
            }
            None => (self.create_preview(base, git, request, actor).await?, true),
        };
        let mut deployments = Vec::new();
        for service in self.all_services(preview.id).await? {
            let same_repository = match self.store().get_source(service.id).await {
                Ok(source) => matches!(&source.spec, SourceSpec::Git(copy) if copy.repo_url == git.repo_url),
                Err(StoreError::NotFound { .. }) => continue,
                Err(error) => return Err(error.into()),
            };
            let commit = if same_repository {
                if self.deploying_commit(service.id, &request.head_commit).await?.is_some() {
                    continue;
                }
                Some(request.head_commit.clone())
            } else if created {
                None
            } else {
                continue;
            };
            let deployment = self.queue_deployment(&service, DeploymentTrigger::GitPush, commit, actor).await?;
            self.audit_deployment(actor, &deployment, base.project_id).await?;
            deployments.push(deployment);
        }
        if deployments.is_empty() {
            let commit = &request.head_commit[..12];
            return Ok(PushOutcome::Ignored(format!("{commit} is in the deployments of {} already", preview.slug)));
        }
        tracing::info!(environment = %preview.id, pr = number, deployments = deployments.len(), "deploying a preview");
        Ok(PushOutcome::Previewed { environment: Box::new(preview), deployments })
    }

    /// Copies `base` into a new preview of the pull request. If a part fails, what was made
    /// is deleted again.
    async fn create_preview(
        &self,
        base: &Environment,
        git: &GitSource,
        request: &PullRequest,
        actor: Actor,
    ) -> Result<Environment, ControlError> {
        let number = request.number;
        let fields = EnvironmentFields {
            name: format!("PR #{number}"),
            slug: format!("pr-{number}").parse().expect("a valid slug"),
            previews: false,
        };
        let preview = self.store().create_preview_environment(base.id, number, fields).await?;
        let details = json!({
            "slug": preview.slug,
            "kind": preview.kind.as_str(),
            "pr_number": number,
            "base_environment_id": base.id.to_string(),
            "branch": request.head_branch,
        });
        self.audit_as(actor, "environment.create", preview.id, base.project_id, details).await?;
        if let Err(error) = self.copy_environment(base, &preview, git, &request.head_branch).await {
            if let Err(cleanup) = self.remove_preview(&preview, actor).await {
                tracing::error!(environment = %preview.id, error = %cleanup, "failed to remove a preview that was not made whole");
            }
            return Err(error);
        }
        Ok(preview)
    }

    async fn copy_environment(
        &self,
        base: &Environment,
        preview: &Environment,
        git: &GitSource,
        branch: &str,
    ) -> Result<(), ControlError> {
        self.copy_variables(VariableScope::Environment(base.id), VariableScope::Environment(preview.id)).await?;
        for service in self.all_services(base.id).await? {
            let fields = ServiceFields {
                name: service.name.clone(),
                slug: service.slug.clone(),
                config: service.config.clone(),
                server_id: service.server_id,
            };
            let copy = self.store().create_service(preview.id, service.kind, fields).await?;
            match self.store().get_source(service.id).await {
                Ok(source) => {
                    let spec = match source.spec {
                        SourceSpec::Git(source) if source.repo_url == git.repo_url => {
                            SourceSpec::Git(GitSource { branch: branch.to_string(), ..source })
                        }
                        spec => spec,
                    };
                    self.store().create_source(copy.id, spec).await?;
                }
                Err(StoreError::NotFound { .. }) => {}
                Err(error) => return Err(error.into()),
            }
            if let (Some(key), Some(private)) =
                (self.store().get_deploy_key(service.id).await?, self.store().deploy_key_private(service.id).await?)
            {
                self.store().set_deploy_key(copy.id, &key.public_key, &key.fingerprint, &private).await?;
            }
            for volume in self.store().list_volumes(service.id).await? {
                self.store().create_volume(copy.id, VolumeFields { name: volume.name, mount_path: volume.mount_path }).await?;
            }
            self.copy_variables(VariableScope::Service(service.id), VariableScope::Service(copy.id)).await?;
        }
        Ok(())
    }

    async fn copy_variables(&self, from: VariableScope, to: VariableScope) -> Result<(), ControlError> {
        for variable in self.store().list_variables(from).await? {
            let value = self.store().reveal_variable(variable.id).await?;
            let fields = VariableFields { name: variable.name, availability: variable.availability, secret: variable.secret };
            self.store().create_variable(to, fields, value).await?;
        }
        Ok(())
    }

    /// Deletes a preview: its services' domains and the services (whose deployments the
    /// runner retires, removing their volumes too), then the environment.
    pub(crate) async fn remove_preview(&self, preview: &Environment, actor: Actor) -> Result<(), ControlError> {
        debug_assert!(matches!(preview.kind, EnvironmentKind::Preview { .. }));
        let project = preview.project_id;
        for service in self.all_services(preview.id).await? {
            for domain in self.store().list_domains(service.id).await? {
                self.store().delete_domain(domain.id).await?;
                self.audit_as(actor, "domain.delete", domain.id, project, json!({ "hostname": domain.hostname })).await?;
            }
            self.store().delete_service(service.id).await?;
            let details = json!({ "slug": service.slug, "environment_id": preview.id.to_string() });
            self.audit_as(actor, "service.delete", service.id, project, details).await?;
        }
        self.store().delete_environment(preview.id).await?;
        self.audit_as(actor, "environment.delete", preview.id, project, json!({ "slug": preview.slug })).await?;
        self.wake_deployments();
        Ok(())
    }

    /// Every service of the environment, in creation order.
    async fn all_services(&self, environment: EnvironmentId) -> Result<Vec<Service>, ControlError> {
        let mut services: Vec<Service> = Vec::new();
        loop {
            let after: Option<ServiceId> = services.last().map(|service| service.id);
            let page = self.store().list_services(environment, after, MAX_PAGE_SIZE).await?;
            let done = page.len() < MAX_PAGE_SIZE as usize;
            services.extend(page);
            if done {
                return Ok(services);
            }
        }
    }

    pub(crate) async fn audit_deployment(
        &self,
        actor: Actor,
        deployment: &Deployment,
        project: ProjectId,
    ) -> Result<(), ControlError> {
        let details = json!({
            "service_id": deployment.service_id.to_string(),
            "server_id": deployment.server_id.to_string(),
            "commit_sha": deployment.commit_sha,
        });
        self.audit_as(actor, "deployment.create", deployment.id, project, details).await
    }

    async fn audit_as(
        &self,
        actor: Actor,
        action: &'static str,
        target: impl ToString,
        project: ProjectId,
        details: serde_json::Value,
    ) -> Result<(), ControlError> {
        self.audit(NewAuditEntry {
            actor,
            token_id: None,
            action: action.parse().expect("valid audit action"),
            target_id: Some(target.to_string()),
            project_id: Some(project),
            details: details.as_object().cloned().unwrap_or_default(),
        })
        .await
    }
}

#[cfg(test)]
mod tests;
