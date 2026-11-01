//! `RunRelease`: the release command (such as migrations), run once per deployment between
//! its build and its start, in a one-off container from the deployment's image
//! ([`ContainerRuntime::run_once`]). Its output goes to the deployment's log, with secret
//! variables redacted, and the container is removed once it has stopped. Canceling the
//! deployment kills it: `StopDeployment` removes the container by its name
//! ([`container_name`]), so the command ends with the exit code of a killed process.

use std::time::Duration;

use spinikus_core::id::{DeploymentId, ServiceId};
use spinikus_proto::agent::v1::{self as pb, FailureReason};

use crate::{
    commands::{Progress, failure, parse_id},
    deploy::network,
    runtime::{ContainerRuntime, OneOffExit, OneOffSpec},
};

/// How long a release command may run when the command does not say, and at most.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MAX_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// The release container's name: one per deployment, so a repeat replaces it.
pub(crate) fn container_name(deployment: DeploymentId) -> String {
    format!("spinikus-{deployment}-release")
}

pub(crate) async fn run_release(
    runtime: &dyn ContainerRuntime,
    release: pb::RunRelease,
    progress: &Progress,
) -> Result<pb::CommandSucceeded, pb::CommandFailed> {
    let deployment_id: DeploymentId = parse_id("deployment ID", &release.deployment_id)?;
    let service_id: ServiceId = parse_id("service ID", &release.service_id)?;
    if release.image.is_empty() || release.command.trim().is_empty() {
        return Err(failure(FailureReason::Internal, "the release needs an image and a command"));
    }
    let timeout = release
        .timeout
        .and_then(|timeout| Duration::try_from(timeout).ok())
        .filter(|timeout| !timeout.is_zero())
        .unwrap_or(DEFAULT_TIMEOUT)
        .min(MAX_TIMEOUT);
    let spec = OneOffSpec {
        name: container_name(deployment_id),
        image: release.image.clone(),
        service_id,
        deployment_id,
        env: release.variables.iter().map(|variable| (variable.name.clone(), variable.value.clone())).collect(),
        network: network(release.network.as_ref())?,
        command: vec!["/bin/sh".to_string(), "-c".to_string(), release.command.clone()],
        timeout,
    };
    progress.redact(release.variables.iter().filter(|variable| variable.secret).map(|variable| variable.value.clone()));
    progress.phase("releasing");
    progress.line(format!("$ {}", release.command));
    let exit = runtime
        .run_once(&spec, &|line| progress.line(line.text))
        .await
        .map_err(|error| failure(FailureReason::ReleaseFailed, format!("the release command could not run: {error}")))?;
    match exit {
        OneOffExit::Exited(0) => Ok(pb::CommandSucceeded::default()),
        OneOffExit::Exited(code) => {
            Err(failure(FailureReason::ReleaseFailed, format!("the release command exited with code {code}")))
        }
        OneOffExit::TimedOut => Err(failure(
            FailureReason::Timeout,
            format!("the release command was still running after {}s, so it was stopped", timeout.as_secs()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;
    use crate::runtime::MemoryRuntime;

    fn release(deployment: DeploymentId) -> pb::RunRelease {
        pb::RunRelease {
            deployment_id: deployment.to_string(),
            service_id: ServiceId::new().to_string(),
            image: "app@sha256:01".to_string(),
            command: "npm run migrate".to_string(),
            variables: vec![
                pb::Variable {
                    name: "DATABASE_URL".to_string(),
                    value: "postgres://u:hunter22@db/app".to_string(),
                    secret: true,
                },
                pb::Variable { name: "PORT".to_string(), value: "3000".to_string(), secret: false },
            ],
            timeout: Some(prost_types::Duration { seconds: 120, nanos: 0 }),
            network: Some(pb::ServiceNetwork { name: "spinikus-env_01".to_string(), aliases: Vec::new() }),
        }
    }

    async fn run(
        runtime: &MemoryRuntime,
        command: pb::RunRelease,
    ) -> (Result<pb::CommandSucceeded, pb::CommandFailed>, Vec<String>) {
        let (sender, mut receiver) = mpsc::channel(64);
        let progress = Progress::new("cmd".to_string(), sender);
        let result = run_release(runtime, command, &progress).await;
        let mut lines = Vec::new();
        while let Ok(progress) = receiver.try_recv() {
            lines.extend(progress.log_lines.into_iter().map(|line| line.text));
        }
        (result, lines)
    }

    #[tokio::test]
    async fn runs_the_command_once_in_a_one_off_container() {
        let runtime = MemoryRuntime::new();
        let deployment = DeploymentId::new();
        runtime.queue_one_off(&["migrating postgres://u:hunter22@db/app", "done"], OneOffExit::Exited(0));

        let (result, lines) = run(&runtime, release(deployment)).await;

        result.unwrap();
        let spec = &runtime.one_offs()[0];
        assert_eq!(spec.name, format!("spinikus-{deployment}-release"));
        assert_eq!(spec.command, ["/bin/sh", "-c", "npm run migrate"]);
        assert_eq!((spec.image.as_str(), spec.timeout), ("app@sha256:01", Duration::from_secs(120)));
        assert_eq!(spec.network.as_ref().unwrap().name, "spinikus-env_01");
        assert!(spec.env.contains(&("PORT".to_string(), "3000".to_string())));
        assert_eq!(lines, ["$ npm run migrate", "migrating [redacted]", "done"]);
        assert!(runtime.containers().is_empty(), "never one of the deployment's containers");
    }

    #[tokio::test]
    async fn a_failure_or_a_timeout_fails_the_release() {
        let runtime = MemoryRuntime::new();
        runtime.queue_one_off(&["relation \"users\" already exists"], OneOffExit::Exited(1));
        runtime.queue_one_off(&[], OneOffExit::TimedOut);

        let failed = run(&runtime, release(DeploymentId::new())).await.0.unwrap_err();
        let timed_out = run(&runtime, release(DeploymentId::new())).await.0.unwrap_err();
        let mut network = release(DeploymentId::new());
        network.network = Some(pb::ServiceNetwork { name: "host".to_string(), aliases: Vec::new() });
        let refused = run(&runtime, network).await.0.unwrap_err();

        assert_eq!(
            (failed.reason(), failed.message.as_str()),
            (FailureReason::ReleaseFailed, "the release command exited with code 1")
        );
        assert_eq!(timed_out.reason(), FailureReason::Timeout);
        assert!(timed_out.message.contains("after 120s"), "{}", timed_out.message);
        assert_eq!(refused.message, "invalid network `host`");
        assert_eq!(runtime.one_offs().len(), 2);
    }
}
