//! Services' Git webhooks, which deploy each push to the service's branch, and keep the
//! previews of pull requests to it (`previews.rs`). Developers turn
//! one on (and get its secret, shown once) and off; viewers see it and what its last delivery
//! did. Deliveries come in without a principal: the signature is the only proof, and a
//! deployment they start has the webhook as its actor.
//!
//! A delivery deploys nothing, and says why, when it is not a push, is for another branch
//! or a tag, deletes the branch, changes nothing under the service's root directory, or is
//! for a commit already being deployed or live (providers deliver again on timeouts).

use hmac::{Hmac, KeyInit, Mac};
use serde_json::json;
use sha2::{Digest, Sha256};
use spinikus_core::{
    actor::Actor,
    deployment::{Deployment, DeploymentStatus, DeploymentTrigger},
    environment::Environment,
    id::{ServiceId, WebhookId},
    membership::Role,
    secret::Secret,
    source::SourceSpec,
    webhook::{GitProvider, Push, Webhook},
};
use spinikus_store::{
    DeploymentRepository, EnvironmentRepository, ServiceRepository, SourceRepository, StoreError, WebhookRepository,
};

use crate::{Control, ControlError, Principal, access::not_found};

/// A webhook just made, with its secret: the only time the secret is shown.
#[derive(Debug)]
pub struct NewWebhook {
    pub webhook: Webhook,
    pub secret: Secret,
}

/// A delivery as it arrived: the provider's event and signature headers, and the raw body
/// the signature covers.
#[derive(Debug, Clone, Copy)]
pub struct PushDelivery<'a> {
    pub event: Option<&'a str>,
    pub signature: Option<&'a str>,
    pub body: &'a [u8],
}

/// What a delivery did.
#[derive(Debug)]
pub enum PushOutcome {
    /// GitHub checking that the webhook works.
    Pong,
    Deployed(Box<Deployment>),
    /// A pull request's preview, made or updated, and what it deploys.
    Previewed {
        environment: Box<Environment>,
        deployments: Vec<Deployment>,
    },
    /// A closed pull request's preview, deleted.
    PreviewRemoved(Box<Environment>),
    /// Nothing to deploy, and why.
    Ignored(String),
}

/// How many recent deployments are checked for the pushed commit.
const RECENT_DEPLOYMENTS: u32 = 20;

impl Control {
    /// Turns on auto-deploy for a service with a Git source, or replaces its secret. The URL
    /// stays the same.
    pub async fn create_webhook(&self, principal: &Principal, service_id: ServiceId) -> Result<NewWebhook, ControlError> {
        let (_, environment) = self.service_with_access(principal, service_id, Role::Developer).await?;
        match self.store().get_source(service_id).await {
            Ok(source) if matches!(source.spec, SourceSpec::Git(_)) => {}
            Ok(_) | Err(StoreError::NotFound { .. }) => {
                return Err(ControlError::Invalid {
                    field: "source",
                    problem: "deploying on push needs a Git source".to_string(),
                });
            }
            Err(error) => return Err(error.into()),
        }
        let secret = Secret::new(rand::random::<[u8; 32]>().iter().map(|byte| format!("{byte:02x}")).collect::<String>());
        let webhook = self.store().set_webhook(service_id, &secret).await?;
        self.audit_by(principal, "webhook.create", webhook.id, Some(environment.project_id), json!({})).await?;
        Ok(NewWebhook { webhook, secret })
    }

    pub async fn get_webhook(&self, principal: &Principal, service_id: ServiceId) -> Result<Webhook, ControlError> {
        self.service_with_access(principal, service_id, Role::Viewer).await?;
        self.store().find_service_webhook(service_id).await?.ok_or_else(|| not_found("webhook", service_id))
    }

    /// Turns auto-deploy off. Deliveries to the old URL are refused from now on.
    pub async fn delete_webhook(&self, principal: &Principal, service_id: ServiceId) -> Result<(), ControlError> {
        let (_, environment) = self.service_with_access(principal, service_id, Role::Developer).await?;
        let webhook = self.get_webhook(principal, service_id).await?;
        self.store().delete_webhook(service_id).await?;
        self.audit_by(principal, "webhook.delete", webhook.id, Some(environment.project_id), json!({})).await?;
        Ok(())
    }

    /// Handles a delivery to `/hooks/git/<provider>/<id>`. `Unauthenticated` if the
    /// signature does not match; what it did otherwise is kept as the webhook's last delivery.
    pub async fn receive_push(
        &self,
        provider: GitProvider,
        id: WebhookId,
        delivery: PushDelivery<'_>,
    ) -> Result<PushOutcome, ControlError> {
        let webhook = self.store().get_webhook(id).await?;
        let secret = self.store().webhook_secret(id).await?;
        if !signed(provider, &secret, delivery) {
            let problem = format!("refused: the {provider} signature does not match the secret");
            self.store().record_webhook_delivery(id, &problem).await?;
            return Err(ControlError::Unauthenticated);
        }
        let outcome = self.handle_push(provider, &webhook, delivery).await;
        let result = match &outcome {
            Ok(PushOutcome::Pong) => "ping".to_string(),
            Ok(PushOutcome::Deployed(deployment)) => format!("deployed {}", deployment.id),
            Ok(PushOutcome::Previewed { environment, deployments }) => {
                let ids: Vec<String> = deployments.iter().map(|deployment| deployment.id.to_string()).collect();
                format!("preview {}: deployed {}", environment.slug, ids.join(", "))
            }
            Ok(PushOutcome::PreviewRemoved(environment)) => format!("removed preview {}", environment.slug),
            Ok(PushOutcome::Ignored(why)) => format!("ignored: {why}"),
            Err(ControlError::Internal(_)) => "failed: internal error".to_string(),
            Err(error) => format!("failed: {error}"),
        };
        self.store().record_webhook_delivery(id, &result).await?;
        if let Ok(PushOutcome::Ignored(why)) = &outcome {
            tracing::info!(webhook = %id, why, "ignored a push");
        }
        outcome
    }

    async fn handle_push(
        &self,
        provider: GitProvider,
        webhook: &Webhook,
        delivery: PushDelivery<'_>,
    ) -> Result<PushOutcome, ControlError> {
        let event = delivery.event.unwrap_or_default();
        if provider.is_ping(event) {
            return Ok(PushOutcome::Pong);
        }
        if provider.is_pull_request(event) {
            return self.handle_pull_request(provider, webhook, delivery.body).await;
        }
        if !provider.is_push(event) {
            return Ok(PushOutcome::Ignored(format!("`{event}` events do not deploy; send push and pull request events")));
        }
        let push =
            Push::parse(delivery.body).map_err(|error| ControlError::Invalid { field: "body", problem: error.to_string() })?;
        let service = self.store().get_service(webhook.service_id).await?;
        let git = match self.store().get_source(service.id).await {
            Ok(source) => match source.spec {
                SourceSpec::Git(git) => git,
                SourceSpec::Image(_) => return Ok(PushOutcome::Ignored("the service deploys an image now".to_string())),
            },
            Err(StoreError::NotFound { .. }) => return Ok(PushOutcome::Ignored("the service has no source".to_string())),
            Err(error) => return Err(error.into()),
        };
        let Some(branch) = &push.branch else {
            return Ok(PushOutcome::Ignored("tags do not deploy".to_string()));
        };
        if *branch != git.branch {
            return Ok(PushOutcome::Ignored(format!("a push to `{branch}`; the service deploys `{}`", git.branch)));
        }
        let Some(commit) = push.commit.clone() else {
            return Ok(PushOutcome::Ignored(format!("the push deleted `{branch}`")));
        };
        if !push.touches(git.root_directory.as_deref()) {
            let root = git.root_directory.unwrap_or_default();
            return Ok(PushOutcome::Ignored(format!("nothing under `{root}` changed")));
        }
        if let Some(same) = self.deploying_commit(service.id, &commit).await? {
            return Ok(PushOutcome::Ignored(format!("{} is in deployment {} already", short(&commit), same.id)));
        }

        let actor = Actor::Webhook(webhook.id);
        let deployment = self.queue_deployment(&service, DeploymentTrigger::GitPush, Some(commit.clone()), actor).await?;
        let environment = self.store().get_environment(service.environment_id).await?;
        self.audit_deployment(actor, &deployment, environment.project_id).await?;
        tracing::info!(webhook = %webhook.id, deployment = %deployment.id, commit, "deploying a push");
        Ok(PushOutcome::Deployed(Box::new(deployment)))
    }
}

impl Control {
    /// A recent deployment of `commit` to the service that is in progress or live, if there
    /// is one: providers deliver again on timeouts, and several webhooks see a pull request.
    pub(crate) async fn deploying_commit(&self, service: ServiceId, commit: &str) -> Result<Option<Deployment>, ControlError> {
        let recent = self.store().list_deployments(service, None, RECENT_DEPLOYMENTS).await?;
        Ok(recent.into_iter().find(|deployment| {
            deployment.commit_sha.as_deref() == Some(commit)
                && (deployment.status.is_in_progress() || deployment.status == DeploymentStatus::Live)
        }))
    }
}

/// Whether the delivery carries the provider's proof of the secret: an HMAC-SHA256 of the
/// body (GitHub, Gitea), or the secret itself (GitLab), compared by digest so the time taken
/// says nothing about it.
fn signed(provider: GitProvider, secret: &Secret, delivery: PushDelivery<'_>) -> bool {
    let Some(signature) = delivery.signature.map(str::trim) else {
        return false;
    };
    let hex = match provider {
        GitProvider::GitLab => return Sha256::digest(signature.as_bytes()) == Sha256::digest(secret.expose().as_bytes()),
        GitProvider::GitHub => signature.strip_prefix("sha256="),
        GitProvider::Gitea => Some(signature),
    };
    let Some(tag) = hex.and_then(decode_hex) else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.expose().as_bytes()).expect("HMAC takes keys of any length");
    mac.update(delivery.body);
    mac.verify_slice(&tag).is_ok()
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len()).step_by(2).map(|at| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok()).collect()
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

#[cfg(test)]
mod tests;
