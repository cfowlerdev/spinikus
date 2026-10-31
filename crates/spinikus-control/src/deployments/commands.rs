//! The commands the runner sends for a deployment. Command IDs are the deployment ID and
//! the step (`dep_…/start`), so sending the same step again is a repeat the agent can tell
//! apart, and a result names the deployment it is for.

use spinikus_core::{
    deployment::{Deployment, DeploymentStrategy},
    id::{DeploymentId, EnvironmentId},
    runtime::RuntimeSettings,
    source::SourceSpec,
    variable::{ResolvedVariable, VariableAvailability},
};
use spinikus_proto::agent::v1::{self as pb, command::Kind, prepare_deployment::Source};
use spinikus_store::{DeployKeyRepository, DeploymentRepository, ServiceRepository, StoreError, VolumeRepository};

use crate::{Control, ControlError};

/// The runtime network the environment's services share: they reach each other there by
/// their slugs, and nothing outside the environment can.
pub fn network_name(environment: EnvironmentId) -> String {
    format!("spinikus-{environment}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Prepare,
    Start,
    Stop,
    /// A second stop, for what a start finished after the deployment was canceled: the
    /// agent answers a repeated `stop` with its stored result instead of stopping again.
    Cleanup,
    /// Removes a finished deployment's containers and images (`cleanup.rs`).
    Remove,
}

impl Step {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Cleanup => "cleanup",
            Self::Remove => "remove",
        }
    }
}

pub(crate) fn command_id(deployment: DeploymentId, step: Step) -> String {
    format!("{deployment}/{}", step.as_str())
}

/// The deployment and step a command ID names, or `None` for IDs the runner did not make.
pub(crate) fn parse_command_id(id: &str) -> Option<(DeploymentId, Step)> {
    let (deployment, step) = id.split_once('/')?;
    let step = match step {
        "prepare" => Step::Prepare,
        "start" => Step::Start,
        "stop" => Step::Stop,
        "cleanup" => Step::Cleanup,
        "remove" => Step::Remove,
        _ => return None,
    };
    Some((deployment.parse().ok()?, step))
}

fn command(deployment: DeploymentId, step: Step, kind: Kind) -> pb::Command {
    pb::Command { command_id: command_id(deployment, step), kind: Some(kind) }
}

/// The deployment's settings, which were checked when it was created.
pub(crate) fn settings(deployment: &Deployment) -> Result<RuntimeSettings, ControlError> {
    RuntimeSettings::from_config(&deployment.snapshot.config)
        .map_err(|error| ControlError::internal("reading a deployment's runtime settings", error))
}

fn duration(duration: std::time::Duration) -> Option<prost_types::Duration> {
    prost_types::Duration::try_from(duration).ok()
}

/// The variables available at `stage`, values included: only ever sent to the agent.
fn variables(variables: &[ResolvedVariable], build: bool) -> Vec<pb::Variable> {
    variables
        .iter()
        .filter(|variable| match variable.availability {
            VariableAvailability::Both => true,
            VariableAvailability::Build => build,
            VariableAvailability::Runtime => !build,
        })
        .map(|variable| pb::Variable {
            name: variable.name.to_string(),
            value: variable.value.expose().to_string(),
            secret: variable.secret,
        })
        .collect()
}

impl Control {
    pub(crate) async fn prepare_command(&self, deployment: &Deployment) -> Result<pb::Command, ControlError> {
        let source = match &deployment.snapshot.source {
            SourceSpec::Image(image) => Source::Image(pb::ImageSource { image: image.image.clone() }),
            SourceSpec::Git(git) => {
                // The service's current key, so a key replaced since works for this build too.
                let key = match git.uses_ssh() {
                    true => self.store().deploy_key_private(deployment.service_id).await?,
                    false => None,
                };
                Source::Git(pb::GitSource {
                    repo_url: git.repo_url.clone(),
                    branch: git.branch.clone(),
                    commit_sha: deployment.commit_sha.clone().unwrap_or_default(),
                    root_directory: git.root_directory.clone().unwrap_or_default(),
                    access_token: String::new(),
                    ssh_private_key: key.map(|key| key.expose().to_string()).unwrap_or_default(),
                })
            }
        };
        let resolved = self.store().deployment_variables(deployment.id).await?;
        let prepare = pb::PrepareDeployment {
            deployment_id: deployment.id.to_string(),
            service_id: deployment.service_id.to_string(),
            source: Some(source),
            config_json: serde_json::Value::Object(deployment.snapshot.config.clone()).to_string(),
            build_variables: variables(&resolved, true),
        };
        Ok(command(deployment.id, Step::Prepare, Kind::PrepareDeployment(prepare)))
    }

    pub(crate) async fn start_command(&self, deployment: &Deployment) -> Result<pb::Command, ControlError> {
        let settings = settings(deployment)?;
        let image = deployment.image.clone().ok_or_else(|| {
            let problem = format!("deployment {} has no image", deployment.id);
            ControlError::internal("starting a deployment", std::io::Error::other(problem))
        })?;
        let resolved = self.store().deployment_variables(deployment.id).await?;
        let volumes = self.store().list_volumes(deployment.service_id).await?;
        // The environment's network, where the service's slug is its name. A deleted service
        // keeps the default network.
        let network = match self.store().get_service(deployment.service_id).await {
            Ok(service) => {
                Some(pb::ServiceNetwork { name: network_name(service.environment_id), aliases: vec![service.slug.to_string()] })
            }
            Err(StoreError::NotFound { .. }) => None,
            Err(error) => return Err(error.into()),
        };
        let check = settings.health_check();
        let strategy = match deployment.strategy {
            DeploymentStrategy::BlueGreen => pb::DeploymentStrategy::BlueGreen,
            DeploymentStrategy::Recreate => pb::DeploymentStrategy::Recreate,
        };
        // Under `recreate` the agent stops the live deployment first, if it runs there too.
        let replaces = match deployment.strategy {
            DeploymentStrategy::Recreate => self
                .store()
                .live_deployment(deployment.service_id)
                .await?
                .filter(|live| live.server_id == deployment.server_id && live.id != deployment.id),
            DeploymentStrategy::BlueGreen => None,
        };
        let start = pb::StartDeployment {
            deployment_id: deployment.id.to_string(),
            service_id: deployment.service_id.to_string(),
            image,
            variables: variables(&resolved, false),
            strategy: strategy.into(),
            port: settings.port.map(u32::from).unwrap_or(0),
            volumes: volumes
                .into_iter()
                .map(|volume| pb::VolumeMount { volume_id: volume.id.to_string(), mount_path: volume.mount_path.to_string() })
                .collect(),
            health_check: Some(pb::HealthCheck {
                path: check.path.unwrap_or_default(),
                interval: duration(check.interval),
                timeout: duration(check.timeout),
                retries: check.retries,
                start_period: duration(check.start_period),
            }),
            replaces_deployment_id: replaces.map(|live| live.id.to_string()).unwrap_or_default(),
            network,
            command: settings.command(),
        };
        Ok(command(deployment.id, Step::Start, Kind::StartDeployment(start)))
    }

    /// A `StopDeployment` as `step`, [`Step::Stop`] or [`Step::Cleanup`].
    pub(crate) fn stop_command(&self, deployment: &Deployment, step: Step) -> Result<pb::Command, ControlError> {
        let grace = settings(deployment).map(|settings| settings.shutdown_timeout()).unwrap_or_default();
        let stop = pb::StopDeployment { deployment_id: deployment.id.to_string(), grace_period: duration(grace) };
        Ok(command(deployment.id, step, Kind::StopDeployment(stop)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_ids_name_the_deployment_and_step() {
        let deployment = DeploymentId::new();

        for step in [Step::Prepare, Step::Start, Step::Stop, Step::Cleanup, Step::Remove] {
            assert_eq!(parse_command_id(&command_id(deployment, step)), Some((deployment, step)));
        }
        assert_eq!(parse_command_id(&format!("{deployment}/build")), None);
        assert_eq!(parse_command_id("cmd-1"), None);
        assert_eq!(parse_command_id("svc_01/start"), None);
    }
}
