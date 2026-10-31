//! Deployment commands: `PrepareDeployment` pulls the image and pins it by digest, or
//! builds it from Git ([`crate::build`]), and
//! `StartDeployment` starts the deployment's container and runs the health check, whose
//! outcome is the command's result.
//!
//! A deployment has one container, named after the deployment, so a repeated
//! `StartDeployment` finds the container it created before instead of creating another.
//! A container that fails verification is stopped but kept, so its logs can still be read;
//! the control plane removes it with `StopDeployment`.

use std::time::Duration;

use spinikus_core::id::{DeploymentId, ServiceId, VolumeId};
use spinikus_proto::agent::v1::{self as pb, DeploymentStrategy, FailureReason, prepare_deployment::Source};

use crate::{
    build::{BuildOptions, build_from_git},
    commands::{Progress, failure, parse_id, stop_containers},
    health::{HealthCheck, verify},
    runtime::{ContainerRuntime, ContainerSpec, Mount, RuntimeError},
};

/// How long the previous deployment's containers get to stop under `recreate`, and a
/// failed container gets before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(10);

pub(crate) async fn prepare_deployment(
    runtime: &dyn ContainerRuntime,
    build: &BuildOptions,
    prepare: pb::PrepareDeployment,
    progress: &Progress,
) -> Result<pb::CommandSucceeded, pb::CommandFailed> {
    let _: DeploymentId = parse_id("deployment ID", &prepare.deployment_id)?;
    let image = match &prepare.source {
        Some(Source::Image(source)) if !source.image.is_empty() => source.image.clone(),
        Some(Source::Image(_)) => return Err(failure(FailureReason::Internal, "the image source names no image")),
        Some(Source::Git(git)) => {
            let prepared = build_from_git(runtime, build, &prepare, git, progress).await?;
            return Ok(pb::CommandSucceeded { prepared: Some(prepared) });
        }
        None => return Err(failure(FailureReason::Internal, "the deployment has no source")),
    };
    progress.phase("pulling image");
    let pinned = runtime
        .pull(&image, &|line| progress.line(line))
        .await
        .map_err(|error| failure(FailureReason::BuildFailed, error.to_string()))?;
    progress.line(format!("Pinned {image} as {pinned}"));
    Ok(pb::CommandSucceeded { prepared: Some(pb::PreparedImage { image: pinned, ..Default::default() }) })
}

pub(crate) async fn start_deployment(
    runtime: &dyn ContainerRuntime,
    start: pb::StartDeployment,
    progress: &Progress,
) -> Result<pb::CommandSucceeded, pb::CommandFailed> {
    let spec = spec(&start)?;
    let check = HealthCheck::new(start.health_check).map_err(|problem| failure(FailureReason::Internal, problem))?;
    if check.path.is_some() && spec.port.is_none() {
        return Err(failure(FailureReason::Internal, "a health check path needs the app's port"));
    }
    let strategy = DeploymentStrategy::try_from(start.strategy).unwrap_or(DeploymentStrategy::BlueGreen);
    if strategy == DeploymentStrategy::Recreate && !start.replaces_deployment_id.is_empty() {
        let replaced = parse_id("replaced deployment ID", &start.replaces_deployment_id)?;
        progress.phase("stopping the previous deployment");
        stop_containers(runtime, replaced, STOP_GRACE).await?;
    }

    progress.phase("starting");
    let started = |error: RuntimeError| failure(FailureReason::StartFailed, error.to_string());
    let existing = runtime.list().await?.into_iter().find(|instance| instance.deployment_id == spec.deployment_id);
    let container = match existing {
        Some(existing) => existing.container_id,
        None => match runtime.create(&spec).await {
            Ok(container) => container,
            Err(RuntimeError::Conflict) => {
                let message = format!("a container named `{}` exists but Spinikus does not manage it", spec.name);
                return Err(failure(FailureReason::StartFailed, message));
            }
            Err(error) => return Err(started(error)),
        },
    };
    runtime.start(&container).await.map_err(started)?;

    progress.phase("verifying");
    if let Err(failed) = verify(runtime, &container, &check, progress).await {
        // Keep it, for its logs, but end any crash loop under the restart policy.
        if let Err(error) = runtime.stop(&container, STOP_GRACE).await {
            tracing::warn!(container, %error, "failed to stop a container that failed verification");
        }
        return Err(failed);
    }
    Ok(pb::CommandSucceeded::default())
}

/// The container to create, from the command, with its fields checked.
fn spec(start: &pb::StartDeployment) -> Result<ContainerSpec, pb::CommandFailed> {
    let invalid = |problem: String| failure(FailureReason::Internal, problem);
    let deployment_id: DeploymentId = parse_id("deployment ID", &start.deployment_id)?;
    let service_id: ServiceId = parse_id("service ID", &start.service_id)?;
    if start.image.is_empty() {
        return Err(invalid("no image to start".to_string()));
    }
    let port = match start.port {
        0 => None,
        port => Some(u16::try_from(port).map_err(|_| invalid(format!("invalid port {port}")))?),
    };
    let volumes = start
        .volumes
        .iter()
        .map(|volume| {
            let _: VolumeId = parse_id("volume ID", &volume.volume_id)?;
            // A colon would be read as mount options.
            if !volume.mount_path.starts_with('/') || volume.mount_path.contains(':') {
                return Err(invalid(format!("invalid mount path `{}`", volume.mount_path)));
            }
            Ok(Mount { volume: volume.volume_id.clone(), path: volume.mount_path.clone() })
        })
        .collect::<Result<_, _>>()?;
    Ok(ContainerSpec {
        name: format!("spinikus-{deployment_id}"),
        image: start.image.clone(),
        service_id,
        deployment_id,
        env: start.variables.iter().map(|variable| (variable.name.clone(), variable.value.clone())).collect(),
        port,
        volumes,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use spinikus_core::server::InstanceStatus;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::mpsc,
    };

    use super::*;
    use crate::runtime::MemoryRuntime;

    fn builds() -> BuildOptions {
        BuildOptions::in_dir(&std::env::temp_dir().join("spinikus-agent-tests"))
    }

    fn progress() -> (Progress, mpsc::Receiver<pb::CommandProgress>) {
        let (sender, receiver) = mpsc::channel(64);
        (Progress::new("cmd".to_string(), sender), receiver)
    }

    fn command(deployment: DeploymentId, service: ServiceId) -> pb::StartDeployment {
        pb::StartDeployment {
            deployment_id: deployment.to_string(),
            service_id: service.to_string(),
            image: "app@sha256:0123".to_string(),
            variables: vec![pb::Variable { name: "SECRET".to_string(), value: "hunter2".to_string(), secret: true }],
            port: 3000,
            volumes: vec![pb::VolumeMount { volume_id: VolumeId::new().to_string(), mount_path: "/data".to_string() }],
            // No path: the container only has to keep running, briefly.
            health_check: Some(pb::HealthCheck {
                start_period: Some(prost_types::Duration { seconds: 0, nanos: 20_000_000 }),
                interval: Some(prost_types::Duration { seconds: 0, nanos: 5_000_000 }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// An app that always answers `status`.
    async fn app(status: &'static str) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let _ = stream.read(&mut [0; 1024]).await;
                let _ = stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").as_bytes()).await;
            }
        });
        address
    }

    #[tokio::test]
    async fn pulls_and_pins_an_image() {
        let runtime = MemoryRuntime::new();
        runtime.add_image("ghcr.io/acme/web:1.2", "ghcr.io/acme/web@sha256:abcd");
        let (progress, mut reported) = progress();
        let prepare = |image: &str| pb::PrepareDeployment {
            deployment_id: DeploymentId::new().to_string(),
            service_id: ServiceId::new().to_string(),
            source: Some(Source::Image(pb::ImageSource { image: image.to_string() })),
            ..Default::default()
        };

        let prepared = prepare_deployment(&runtime, &builds(), prepare("ghcr.io/acme/web:1.2"), &progress).await.unwrap();
        let missing = prepare_deployment(&runtime, &builds(), prepare("ghcr.io/acme/nope"), &progress).await.unwrap_err();

        assert_eq!(prepared.prepared.unwrap().image, "ghcr.io/acme/web@sha256:abcd");
        assert_eq!(missing.reason, FailureReason::BuildFailed as i32);
        assert_eq!(missing.message, "the container runtime failed: cannot pull ghcr.io/acme/nope: not found");
        let first = reported.recv().await.unwrap();
        assert_eq!((first.command_id.as_str(), first.phase.as_str()), ("cmd", "pulling image"));
    }

    #[tokio::test]
    async fn starts_a_container_for_the_deployment() {
        let runtime = MemoryRuntime::new();
        let (deployment, service) = (DeploymentId::new(), ServiceId::new());
        let start = command(deployment, service);

        start_deployment(&runtime, start.clone(), &progress().0).await.unwrap();
        // A repeat reuses the container.
        start_deployment(&runtime, start.clone(), &progress().0).await.unwrap();

        let containers = runtime.containers();
        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].status, InstanceStatus::Running);
        let spec = runtime.spec(&containers[0].container_id).unwrap();
        assert_eq!(spec.name, format!("spinikus-{deployment}"));
        assert_eq!((spec.service_id, spec.deployment_id), (service, deployment));
        assert_eq!(spec.image, "app@sha256:0123");
        assert_eq!(spec.env, [("SECRET".to_string(), "hunter2".to_string())]);
        assert_eq!(spec.port, Some(3000));
        assert_eq!(spec.volumes, [Mount { volume: start.volumes[0].volume_id.clone(), path: "/data".to_string() }]);
    }

    #[tokio::test]
    async fn verifies_with_the_health_check_path() {
        let runtime = MemoryRuntime::new();
        runtime.set_endpoint(app("200 OK").await);
        let mut start = command(DeploymentId::new(), ServiceId::new());
        start.health_check.as_mut().unwrap().path = "/healthz".to_string();

        start_deployment(&runtime, start, &progress().0).await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_health_check_stops_the_container_but_keeps_it() {
        let runtime = MemoryRuntime::new();
        runtime.set_endpoint(app("500 Internal Server Error").await);
        let mut start = command(DeploymentId::new(), ServiceId::new());
        let check = start.health_check.as_mut().unwrap();
        check.path = "/healthz".to_string();
        check.start_period = Some(prost_types::Duration::default());

        let failed = start_deployment(&runtime, start, &progress().0).await.unwrap_err();

        assert_eq!(failed.reason, FailureReason::HealthCheckFailed as i32);
        let containers = runtime.containers();
        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].status, InstanceStatus::Exited);
    }

    #[tokio::test]
    async fn a_crashing_app_fails_to_start() {
        let runtime = MemoryRuntime::new();
        runtime.exit_on_start(true);

        let failed = start_deployment(&runtime, command(DeploymentId::new(), ServiceId::new()), &progress().0).await.unwrap_err();

        assert_eq!(failed.reason, FailureReason::StartFailed as i32);
    }

    #[tokio::test]
    async fn recreate_stops_the_previous_deployment_first() {
        let runtime = Arc::new(MemoryRuntime::new());
        let service = ServiceId::new();
        let (old, new) = (DeploymentId::new(), DeploymentId::new());
        start_deployment(&*runtime, command(old, service), &progress().0).await.unwrap();
        let mut start = command(new, service);
        start.strategy = DeploymentStrategy::Recreate.into();
        start.replaces_deployment_id = old.to_string();

        start_deployment(&*runtime, start, &progress().0).await.unwrap();

        let deployments: Vec<_> = runtime.containers().into_iter().map(|instance| instance.deployment_id).collect();
        assert_eq!(deployments, [new]);
    }

    #[tokio::test]
    async fn blue_green_leaves_the_previous_deployment_running() {
        let runtime = MemoryRuntime::new();
        let service = ServiceId::new();
        let old = DeploymentId::new();
        start_deployment(&runtime, command(old, service), &progress().0).await.unwrap();
        let mut start = command(DeploymentId::new(), service);
        start.replaces_deployment_id = old.to_string();

        start_deployment(&runtime, start, &progress().0).await.unwrap();

        assert_eq!(runtime.containers().len(), 2);
    }

    #[tokio::test]
    async fn rejects_invalid_commands() {
        let runtime = MemoryRuntime::new();
        let valid = || command(DeploymentId::new(), ServiceId::new());
        let mut no_image = valid();
        no_image.image.clear();
        let mut bad_port = valid();
        bad_port.port = 70_000;
        let mut bad_mount = valid();
        bad_mount.volumes[0].mount_path = "/data:ro".to_string();
        let mut path_without_port = valid();
        path_without_port.port = 0;
        path_without_port.health_check.as_mut().unwrap().path = "/up".to_string();

        for (start, expected) in [
            (no_image, "no image to start"),
            (bad_port, "invalid port 70000"),
            (bad_mount, "invalid mount path `/data:ro`"),
            (path_without_port, "a health check path needs the app's port"),
        ] {
            let failed = start_deployment(&runtime, start, &progress().0).await.unwrap_err();
            assert_eq!(failed.message, expected);
        }
        assert!(runtime.containers().is_empty());
    }
}
