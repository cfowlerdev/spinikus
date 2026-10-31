//! Deployments: immutable records of one release of a service, moving through the
//! state machine in [`state`].

mod state;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
pub use state::*;

use crate::{
    actor::Actor,
    id::{DeploymentEventId, DeploymentId, ServerId, ServiceId},
    service::{ServiceConfig, ServiceKind},
    source::SourceSpec,
    text::impl_text,
    variable::ResolvedVariable,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Deployment {
    pub id: DeploymentId,
    pub service_id: ServiceId,
    /// The server it runs on, fixed when it is created.
    pub server_id: ServerId,
    pub trigger: DeploymentTrigger,
    pub strategy: DeploymentStrategy,
    pub status: DeploymentStatus,
    /// Set when the deployment is `failed` or `canceled`.
    pub reason: Option<EndReason>,
    /// The deployment this one rolls back to, for rollbacks.
    pub rollback_of: Option<DeploymentId>,
    pub snapshot: DeploymentSnapshot,
    /// The commit being deployed, once known: from the webhook, or after the agent clones.
    pub commit_sha: Option<String>,
    /// The image that runs, once known: the source image, or the result of the build.
    pub image: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// What a new deployment starts from. The ID, status and timestamps are set by the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDeployment {
    pub service_id: ServiceId,
    pub server_id: ServerId,
    pub trigger: DeploymentTrigger,
    pub strategy: DeploymentStrategy,
    pub rollback_of: Option<DeploymentId>,
    pub snapshot: DeploymentSnapshot,
    pub commit_sha: Option<String>,
    pub image: Option<String>,
    /// The service's variables, resolved when the deployment is created (or copied from the
    /// target of a rollback). Stored encrypted, apart from the deployment, and read back with
    /// `DeploymentRepository::deployment_variables`.
    pub variables: Vec<ResolvedVariable>,
}

/// The service's settings as they were when the deployment was created, so later
/// edits never change what an existing deployment runs. A rollback reuses its target's snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentSnapshot {
    pub source: SourceSpec,
    pub config: ServiceConfig,
    /// The config fields `spinikus.json` set, such as `runtime.port`, once the build has
    /// read it; the file's values are already in `config`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub from_file: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentTrigger {
    /// Started by a user from the UI, API or CLI.
    Manual,
    /// Started by a Git webhook.
    GitPush,
    /// Redeploys an earlier deployment's image and snapshot.
    Rollback,
}

impl DeploymentTrigger {
    pub const ALL: [DeploymentTrigger; 3] = [Self::Manual, Self::GitPush, Self::Rollback];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::GitPush => "git_push",
            Self::Rollback => "rollback",
        }
    }
}

/// How the new deployment replaces the live one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentStrategy {
    /// Start the new containers, verify them, switch routes, then drain the old ones.
    BlueGreen,
    /// Stop the old containers before starting the new ones, accepting brief downtime.
    /// Used when two containers must not share a volume.
    Recreate,
}

impl DeploymentStrategy {
    pub const ALL: [DeploymentStrategy; 2] = [Self::BlueGreen, Self::Recreate];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::BlueGreen => "blue_green",
            Self::Recreate => "recreate",
        }
    }

    /// Databases and services with persistent volumes are recreated, since two containers
    /// must not share a volume; everything else is blue-green.
    pub fn default_for(kind: ServiceKind, has_volumes: bool) -> Self {
        match kind {
            ServiceKind::Database => Self::Recreate,
            ServiceKind::App | ServiceKind::Template if has_volumes => Self::Recreate,
            ServiceKind::App | ServiceKind::Template => Self::BlueGreen,
        }
    }
}

impl_text!(DeploymentTrigger, "deployment trigger");
impl_text!(DeploymentStrategy, "deployment strategy");

/// One recorded change of a deployment's status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeploymentEvent {
    pub id: DeploymentEventId,
    pub deployment_id: DeploymentId,
    /// `None` for the event that creates the deployment.
    pub from: Option<DeploymentStatus>,
    pub to: DeploymentStatus,
    pub reason: Option<EndReason>,
    /// What went wrong, in words, for a deployment that failed.
    pub message: Option<String>,
    pub actor: Actor,
    pub created_at: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trips() {
        for trigger in DeploymentTrigger::ALL {
            assert_eq!(trigger.to_string().parse(), Ok(trigger));
            assert_eq!(serde_json::to_string(&trigger).unwrap(), format!("\"{trigger}\""));
        }
        for strategy in DeploymentStrategy::ALL {
            assert_eq!(strategy.to_string().parse(), Ok(strategy));
            assert_eq!(serde_json::to_string(&strategy).unwrap(), format!("\"{strategy}\""));
        }
    }

    #[test]
    fn databases_and_services_with_volumes_default_to_recreate() {
        assert_eq!(DeploymentStrategy::default_for(ServiceKind::Database, false), DeploymentStrategy::Recreate);
        assert_eq!(DeploymentStrategy::default_for(ServiceKind::App, true), DeploymentStrategy::Recreate);
        assert_eq!(DeploymentStrategy::default_for(ServiceKind::App, false), DeploymentStrategy::BlueGreen);
        assert_eq!(DeploymentStrategy::default_for(ServiceKind::Template, false), DeploymentStrategy::BlueGreen);
    }
}
