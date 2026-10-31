//! The Docker driver against a real Docker daemon. Ignored by default, since it needs one:
//! run with `cargo test -p spinikus-agent --test docker -- --ignored`. It pulls `busybox`
//! and `traefik/whoami`, and builds from `busybox`.

use std::{collections::HashMap, time::Duration};

use bollard::{
    models::ContainerCreateBody,
    query_parameters::{CreateImageOptionsBuilder, RemoveContainerOptionsBuilder},
};
use spinikus_agent::runtime::{
    BuildSpec, ContainerRuntime, ContainerSpec, DEPLOYMENT_LABEL, DockerRuntime, Mount, RuntimeError, SERVICE_LABEL,
};
use spinikus_core::{
    id::{DeploymentId, ServiceId, VolumeId},
    server::InstanceStatus,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::timeout,
};
use tokio_stream::StreamExt;

const IMAGE: &str = "busybox:1.37";

/// Removes the container even when an assertion fails.
struct Cleanup(DockerRuntime, String);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let (runtime, id) = (self.0.clone(), self.1.clone());
        let options = RemoveContainerOptionsBuilder::new().force(true).build();
        let _ = std::thread::spawn(move || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(async move { runtime.client().unwrap().remove_container(&id, Some(options)).await })
        })
        .join();
    }
}

/// Removes a named volume even when an assertion fails. Declare it before the container's
/// [`Cleanup`], so the container goes first.
struct VolumeCleanup(DockerRuntime, String);

impl Drop for VolumeCleanup {
    fn drop(&mut self) {
        let (runtime, name) = (self.0.clone(), self.1.clone());
        let _ = std::thread::spawn(move || {
            tokio::runtime::Runtime::new().unwrap().block_on(async move {
                runtime.client().unwrap().remove_volume(&name, None::<bollard::query_parameters::RemoveVolumeOptions>).await
            })
        })
        .join();
    }
}

/// Waits for an event about `id`, skipping others.
async fn event_for(events: &mut spinikus_agent::runtime::ContainerEvents, id: &str) {
    timeout(Duration::from_secs(20), async {
        while let Some(event) = events.next().await {
            if event.unwrap() == id {
                return;
            }
        }
        panic!("the event stream ended");
    })
    .await
    .expect("an event for the container");
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn follows_a_managed_container_through_its_life() {
    let runtime = DockerRuntime::new();
    let docker = runtime.client().unwrap();
    let pull = CreateImageOptionsBuilder::new().from_image(IMAGE).build();
    let mut pulling = docker.create_image(Some(pull), None, None);
    while let Some(progress) = pulling.next().await {
        progress.unwrap();
    }
    let (service, deployment) = (ServiceId::new(), DeploymentId::new());
    let labels =
        HashMap::from([(SERVICE_LABEL.to_string(), service.to_string()), (DEPLOYMENT_LABEL.to_string(), deployment.to_string())]);
    let body = ContainerCreateBody {
        image: Some(IMAGE.to_string()),
        cmd: Some(vec!["sleep".to_string(), "300".to_string()]),
        labels: Some(labels),
        ..Default::default()
    };
    let mut events = runtime.events();

    let id = docker.create_container(None, body).await.unwrap().id;
    let _cleanup = Cleanup(runtime.clone(), id.clone());
    docker.start_container(&id, None).await.unwrap();
    event_for(&mut events, &id).await;
    let listed = runtime.list().await.unwrap();
    let started = listed.iter().find(|instance| instance.container_id == id).expect("listed");

    assert_eq!((started.service_id, started.deployment_id), (service, deployment));
    assert_eq!(started.status, InstanceStatus::Running);
    assert!(started.started_at.is_some());

    runtime.stop(&id, Duration::from_secs(1)).await.unwrap();
    runtime.stop(&id, Duration::from_secs(1)).await.unwrap();
    event_for(&mut events, &id).await;
    assert_eq!(runtime.inspect(&id).await.unwrap().unwrap().status, InstanceStatus::Exited);

    runtime.restart(&id, Duration::from_secs(1)).await.unwrap();
    assert_eq!(runtime.inspect(&id).await.unwrap().unwrap().status, InstanceStatus::Running);

    runtime.stop(&id, Duration::from_secs(1)).await.unwrap();
    runtime.remove(&id).await.unwrap();
    assert_eq!(runtime.inspect(&id).await.unwrap(), None);
    assert_eq!(runtime.remove(&id).await, Err(spinikus_agent::runtime::RuntimeError::NotFound));
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn pulls_creates_and_publishes_an_app_on_loopback() {
    let runtime = DockerRuntime::new();
    let lines = std::sync::Mutex::new(Vec::new());
    let pinned = runtime.pull("traefik/whoami:v1.10", &|line| lines.lock().unwrap().push(line)).await.unwrap();
    let missing = runtime.pull("spinikus/does-not-exist-anywhere", &|_| {}).await;

    assert!(pinned.starts_with("traefik/whoami@sha256:"), "{pinned}");
    assert!(!lines.lock().unwrap().is_empty());
    assert!(matches!(missing, Err(spinikus_agent::runtime::RuntimeError::Failed(_))), "{missing:?}");

    let volume = VolumeId::new().to_string();
    let _volume = VolumeCleanup(runtime.clone(), volume.clone());
    let deployment = DeploymentId::new();
    let spec = ContainerSpec {
        name: format!("spinikus-{deployment}"),
        image: pinned,
        service_id: ServiceId::new(),
        deployment_id: deployment,
        env: vec![("WHOAMI_NAME".to_string(), "spinikus".to_string())],
        port: Some(80),
        volumes: vec![Mount { volume: volume.clone(), path: "/data".to_string() }],
    };
    let id = runtime.create(&spec).await.unwrap();
    let _cleanup = Cleanup(runtime.clone(), id.clone());

    assert_eq!(runtime.create(&spec).await, Err(spinikus_agent::runtime::RuntimeError::Conflict));
    assert_eq!(runtime.endpoint(&id).await.unwrap(), None);

    runtime.start(&id).await.unwrap();
    runtime.start(&id).await.unwrap();
    let endpoint = runtime.endpoint(&id).await.unwrap().expect("a published port");
    assert!(endpoint.ip().is_loopback());

    let mut stream = tokio::net::TcpStream::connect(endpoint).await.unwrap();
    stream.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("Name: spinikus"), "{response}");

    let inspected = runtime.client().unwrap().inspect_container(&id, None).await.unwrap();
    let mounts = inspected.mounts.unwrap_or_default();
    assert!(mounts.iter().any(|mount| mount.name.as_deref() == Some(&volume) && mount.destination.as_deref() == Some("/data")));
}

/// Removes an image even when an assertion fails.
struct ImageCleanup(DockerRuntime, String);

impl Drop for ImageCleanup {
    fn drop(&mut self) {
        let (runtime, image) = (self.0.clone(), self.1.clone());
        let options = bollard::query_parameters::RemoveImageOptionsBuilder::new().force(true).build();
        let _ = std::thread::spawn(move || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(async move { runtime.client().unwrap().remove_image(&image, Some(options), None).await })
        })
        .join();
    }
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn builds_with_buildkit() {
    let runtime = DockerRuntime::new();
    let dir = tempfile::tempdir().unwrap();
    let context = dir.path().join("app");
    std::fs::create_dir_all(context.join("docker")).unwrap();
    std::fs::create_dir_all(context.join(".git")).unwrap();
    std::fs::write(context.join(".git/HEAD"), "not for the image").unwrap();
    std::fs::write(context.join("hello.txt"), "hello").unwrap();
    // A cache mount needs BuildKit; the last stage checks what the context held.
    std::fs::write(
        context.join("docker/Dockerfile"),
        "FROM busybox:1.37 AS base\n\
         ARG GREETING\n\
         COPY . /src\n\
         RUN --mount=type=cache,target=/cache test ! -e /src/.git && echo \"$GREETING\" > /greeting\n\
         FROM base AS broken\n\
         RUN echo going wrong && exit 3\n",
    )
    .unwrap();
    let tag = format!("spinikus/test:{}", DeploymentId::new().to_string().to_lowercase());
    let _cleanup = ImageCleanup(runtime.clone(), tag.clone());
    let spec = BuildSpec {
        context: context.clone(),
        dockerfile: "docker/Dockerfile".to_string(),
        target: Some("base".to_string()),
        args: vec![("GREETING".to_string(), "hi there".to_string())],
        tag: tag.clone(),
    };
    let lines = std::sync::Mutex::new(Vec::new());

    let image = runtime.build(&spec, &|line| lines.lock().unwrap().push(line)).await.unwrap();
    let broken = runtime.build(&BuildSpec { target: Some("broken".to_string()), ..spec.clone() }, &|_| {}).await;

    assert!(image.starts_with("sha256:"), "{image}");
    let inspected = runtime.client().unwrap().inspect_image(&tag).await.unwrap();
    assert_eq!(inspected.id.as_deref(), Some(image.as_str()));
    let lines = lines.into_inner().unwrap();
    assert!(lines.iter().any(|line| line.contains("COPY . /src")), "{lines:#?}");
    match broken {
        Err(RuntimeError::Failed(message)) => {
            assert!(message.contains("exit 3") || message.contains("exit code: 3"), "{message}")
        }
        other => panic!("expected a failed build, got {other:?}"),
    }
}
