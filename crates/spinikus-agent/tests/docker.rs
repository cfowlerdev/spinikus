//! The Docker driver against a real Docker daemon. Ignored by default, since it needs one:
//! run with `cargo test -p spinikus-agent --test docker -- --ignored`. It pulls `busybox`
//! and `traefik/whoami`, and builds from `busybox`.

use std::{collections::HashMap, time::Duration};

use bollard::{
    models::ContainerCreateBody,
    query_parameters::{CreateImageOptionsBuilder, RemoveContainerOptionsBuilder},
};
use spinikus_agent::runtime::{
    BuildSpec, ContainerRuntime, ContainerSpec, DEPLOYMENT_LABEL, DockerRuntime, ExecOutput, LogOptions, Mount, Network,
    OneOffExit, OneOffSpec, RuntimeError, SERVICE_LABEL,
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
        network: None,
        command: Vec::new(),
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

    // A mounted volume stays; once its container is gone, it can go.
    assert_eq!(runtime.remove_volume(&volume).await, Err(RuntimeError::InUse));
    runtime.stop(&id, Duration::from_secs(1)).await.unwrap();
    runtime.remove(&id).await.unwrap();
    runtime.remove_volume(&volume).await.unwrap();
    assert_eq!(runtime.remove_volume(&volume).await, Err(RuntimeError::NotFound));
}

/// Held by the tests that use networks: one of them removes every unused Spinikus network,
/// and each test has its own runtime, whose lock guards only its own containers.
static NETWORKS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
        secrets: Vec::new(),
        tag: tag.clone(),
        local_sources: false,
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

/// The context as local sources over the agent's BuildKit session, as Railpack's frontend
/// reads it. Docker's Dockerfile frontend reads it the same way with `remote=client-session`,
/// including `.dockerignore` (by `followpaths`, then as `exclude-patterns`).
#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn builds_from_local_sources_over_the_session() {
    let runtime = DockerRuntime::new();
    let dir = tempfile::tempdir().unwrap();
    let context = dir.path();
    std::fs::create_dir_all(context.join(".git")).unwrap();
    std::fs::create_dir_all(context.join("src/deep")).unwrap();
    std::fs::write(context.join(".git/HEAD"), "not for the image").unwrap();
    std::fs::write(context.join("src/deep/big.bin"), vec![7u8; 300_000]).unwrap();
    std::fs::write(context.join("ignored.txt"), "left out").unwrap();
    std::fs::write(context.join(".dockerignore"), "ignored.txt\n").unwrap();
    std::fs::write(context.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
    std::fs::set_permissions(context.join("run.sh"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("run.sh", context.join("link")).unwrap();
    std::fs::write(
        context.join("Build.dockerfile"),
        "FROM busybox:1.37\n\
         COPY . /src\n\
         RUN test ! -e /src/.git && test ! -e /src/ignored.txt && test -x /src/run.sh && test -L /src/link \\\n\
             && test \"$(wc -c < /src/src/deep/big.bin)\" -eq 300000 && /src/link | grep -qx hi\n",
    )
    .unwrap();
    let tag = format!("spinikus/test:{}", DeploymentId::new().to_string().to_lowercase());
    let _cleanup = ImageCleanup(runtime.clone(), tag.clone());
    let spec = BuildSpec {
        context: context.to_path_buf(),
        dockerfile: "Build.dockerfile".to_string(),
        target: None,
        args: Vec::new(),
        secrets: Vec::new(),
        tag: tag.clone(),
        local_sources: true,
    };
    let lines = std::sync::Mutex::new(Vec::new());

    let built = runtime.build(&spec, &|line| lines.lock().unwrap().push(line)).await;

    let lines = lines.into_inner().unwrap();
    let image = built.unwrap_or_else(|error| panic!("{error}: {lines:#?}"));
    assert!(image.starts_with("sha256:"), "{image}");
    let missing = runtime.build(&BuildSpec { dockerfile: "Missing.dockerfile".to_string(), ..spec.clone() }, &|_| {}).await;
    assert!(matches!(missing, Err(RuntimeError::Failed(_))), "{missing:?}");
}

/// A plan built by Railpack's BuildKit frontend, which reads the plan and the context as
/// local sources: what a Railpack build does once `railpack prepare` has written the plan.
/// The plan is a minimal hand-written one, so this needs no `railpack` binary.
#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn builds_a_plan_with_the_railpack_frontend() {
    const PLAN_FILE: &str = ".spinikus-railpack-plan.json";
    let runtime = DockerRuntime::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join("hello.txt"), "hello from the context").unwrap();
    let plan = serde_json::json!({
        "deploy": {
            // Railpack's images run commands with /bin/bash -c.
            "base": { "image": "debian:trixie-slim" },
            "inputs": [{ "local": true, "include": ["."] }],
            "startCommand": "cat hello.txt",
        },
        "exclude": [PLAN_FILE],
    });
    std::fs::write(dir.path().join(PLAN_FILE), plan.to_string()).unwrap();
    let tag = format!("spinikus/test:{}", DeploymentId::new().to_string().to_lowercase());
    let _cleanup = ImageCleanup(runtime.clone(), tag.clone());
    let spec = BuildSpec {
        context: dir.path().to_path_buf(),
        dockerfile: PLAN_FILE.to_string(),
        target: None,
        args: vec![
            ("BUILDKIT_SYNTAX".to_string(), "ghcr.io/railwayapp/railpack-frontend:v0.43.0".to_string()),
            ("cache-key".to_string(), ServiceId::new().to_string()),
            ("secrets-hash".to_string(), "none".to_string()),
        ],
        secrets: Vec::new(),
        tag: tag.clone(),
        local_sources: true,
    };
    let lines = std::sync::Mutex::new(Vec::new());
    let built = runtime.build(&spec, &|line| lines.lock().unwrap().push(line)).await;
    let lines = lines.into_inner().unwrap();
    built.unwrap_or_else(|error| panic!("{error}: {lines:#?}"));

    let output = std::sync::Mutex::new(Vec::new());
    let check = OneOffSpec {
        name: format!("spinikus-test-{}", DeploymentId::new().to_string().to_lowercase()),
        image: tag.clone(),
        service_id: ServiceId::new(),
        deployment_id: DeploymentId::new(),
        env: Vec::new(),
        network: None,
        command: vec![format!("pwd; ls -a; cat hello.txt && test ! -e {PLAN_FILE} && test ! -e .git")],
        timeout: Duration::from_secs(30),
    };
    let exit = runtime.run_once(&check, &|line| output.lock().unwrap().push(line.text)).await.unwrap();
    let output = output.into_inner().unwrap();
    assert_eq!(exit, OneOffExit::Exited(0), "{output:#?}");
    assert!(output.iter().any(|line| line.contains("hello from the context")), "{output:#?}");
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn builds_with_secrets_that_stay_out_of_the_image() {
    let runtime = DockerRuntime::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Dockerfile"),
        "FROM busybox:1.37 AS base\n\
         ARG LEVEL\n\
         RUN --mount=type=secret,id=TOKEN grep -qx '[s]3cret-token' /run/secrets/TOKEN && echo \"$LEVEL\" > /level\n\
         FROM base AS broken\n\
         RUN --mount=type=secret,id=TOKEN,required=true --mount=type=secret,id=MISSING,required=true true\n",
    )
    .unwrap();
    let tag = format!("spinikus/test:{}", DeploymentId::new().to_string().to_lowercase());
    let _cleanup = ImageCleanup(runtime.clone(), tag.clone());
    let spec = BuildSpec {
        context: dir.path().to_path_buf(),
        dockerfile: "Dockerfile".to_string(),
        target: Some("base".to_string()),
        args: vec![("LEVEL".to_string(), "info".to_string())],
        secrets: vec![("TOKEN".to_string(), "s3cret-token".to_string())],
        tag: tag.clone(),
        local_sources: false,
    };
    let lines = std::sync::Mutex::new(Vec::new());

    let image = runtime.build(&spec, &|line| lines.lock().unwrap().push(line)).await.unwrap();
    let broken = runtime.build(&BuildSpec { target: Some("broken".to_string()), ..spec.clone() }, &|_| {}).await;

    assert!(image.starts_with("sha256:"), "{image}");
    let lines = lines.into_inner().unwrap();
    assert!(lines.iter().any(|line| line.contains("--mount=type=secret,id=TOKEN")), "{lines:#?}");
    let docker = runtime.client().unwrap();
    let history = serde_json::to_string(&docker.image_history(&tag).await.unwrap()).unwrap();
    let inspected = serde_json::to_string(&docker.inspect_image(&tag).await.unwrap()).unwrap();
    assert!(!history.contains("s3cret") && !inspected.contains("s3cret"), "the secret is not in the image");
    assert!(history.contains("LEVEL"), "{history}");
    match broken {
        Err(RuntimeError::Failed(message)) => assert!(message.contains("MISSING"), "{message}"),
        other => panic!("expected a failed build, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn reads_and_follows_a_containers_log() {
    let runtime = DockerRuntime::new();
    let docker = runtime.client().unwrap();
    let pull = CreateImageOptionsBuilder::new().from_image(IMAGE).build();
    let mut pulling = docker.create_image(Some(pull), None, None);
    while let Some(progress) = pulling.next().await {
        progress.unwrap();
    }
    let labels = HashMap::from([
        (SERVICE_LABEL.to_string(), ServiceId::new().to_string()),
        (DEPLOYMENT_LABEL.to_string(), DeploymentId::new().to_string()),
    ]);
    // Apart, since stdout and stderr are separate pipes and lines written together can swap.
    let script = "echo one; sleep 0.2; echo two >&2; sleep 0.2; echo three; sleep 1; echo four; sleep 300";
    let body = ContainerCreateBody {
        image: Some(IMAGE.to_string()),
        cmd: Some(vec!["sh".to_string(), "-c".to_string(), script.to_string()]),
        labels: Some(labels),
        ..Default::default()
    };
    let id = docker.create_container(None, body).await.unwrap().id;
    let _cleanup = Cleanup(runtime.clone(), id.clone());
    docker.start_container(&id, None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let within = Duration::from_secs(10);

    let tail = LogOptions { tail: Some(2), ..Default::default() };
    let tail: Vec<_> = timeout(within, runtime.logs(&id, tail).collect::<Vec<_>>()).await.unwrap();
    let mut followed = runtime.logs(&id, LogOptions { follow: true, ..Default::default() });
    let mut lines = Vec::new();
    while lines.len() < 4 {
        lines.push(timeout(within, followed.next()).await.unwrap().unwrap().unwrap());
    }

    let tail: Vec<_> = tail.into_iter().map(|line| line.unwrap()).map(|line| (line.stderr, line.text)).collect();
    assert_eq!(tail, [(true, "two".to_string()), (false, "three".to_string())]);
    let texts: Vec<_> = lines.iter().map(|line| line.text.as_str()).collect();
    assert_eq!(texts, ["one", "two", "three", "four"]);
    assert!(lines.iter().all(|line| line.at.is_some()));
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn runs_a_command_in_a_container_with_its_environment() {
    let runtime = DockerRuntime::new();
    let docker = runtime.client().unwrap();
    let pull = CreateImageOptionsBuilder::new().from_image(IMAGE).build();
    let mut pulling = docker.create_image(Some(pull), None, None);
    while let Some(progress) = pulling.next().await {
        progress.unwrap();
    }
    let body = ContainerCreateBody {
        image: Some(IMAGE.to_string()),
        cmd: Some(vec!["sleep".to_string(), "300".to_string()]),
        env: Some(vec!["GREETING=hello".to_string()]),
        ..Default::default()
    };
    let id = docker.create_container(None, body).await.unwrap().id;
    let _cleanup = Cleanup(runtime.clone(), id.clone());
    docker.start_container(&id, None).await.unwrap();
    let within = Duration::from_secs(10);

    // More than one frame of output, to check that nothing is lost between them.
    let script = r#"head -c 300000 /dev/zero; echo "$GREETING" >&2; exit 3"#;
    let command = vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()];
    let output: Vec<_> = timeout(within, runtime.exec(&id, command, None).collect::<Vec<_>>()).await.unwrap();
    let missing =
        timeout(within, runtime.exec("no-such-container", vec!["true".to_string()], None).collect::<Vec<_>>()).await.unwrap();

    let output: Vec<_> = output.into_iter().map(Result::unwrap).collect();
    let stdout: usize = output.iter().map(|item| if let ExecOutput::Stdout(bytes) = item { bytes.len() } else { 0 }).sum();
    let stderr: Vec<u8> = output
        .iter()
        .filter_map(|item| if let ExecOutput::Stderr(bytes) = item { Some(bytes.to_vec()) } else { None })
        .flatten()
        .collect();
    assert_eq!(stdout, 300_000);
    assert_eq!(stderr, b"hello\n");
    assert_eq!(output.last(), Some(&ExecOutput::Exited(3)));
    assert!(matches!(missing.as_slice(), [Err(RuntimeError::NotFound)]), "{missing:?}");

    // Stdin, in chunks, then closed: `wc` only answers once it reads the end.
    let chunks = (0..30).map(|_| bytes::Bytes::from(vec![b'x'; 10_000]));
    let input: spinikus_agent::runtime::ExecInput = Box::pin(tokio_stream::iter(chunks));
    let count = vec!["wc".to_string(), "-c".to_string()];
    let counted: Vec<_> = timeout(within, runtime.exec(&id, count, Some(input)).collect::<Vec<_>>()).await.unwrap();
    let counted: Vec<_> = counted.into_iter().map(Result::unwrap).collect();
    let text: Vec<u8> = counted
        .iter()
        .filter_map(|item| if let ExecOutput::Stdout(bytes) = item { Some(bytes.to_vec()) } else { None })
        .flatten()
        .collect();
    assert_eq!(String::from_utf8_lossy(&text).trim(), "300000");
    assert_eq!(counted.last(), Some(&ExecOutput::Exited(0)));
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn opens_a_terminal_in_a_container() {
    use spinikus_agent::runtime::{TerminalInput, TerminalInputs, TerminalSize};

    let runtime = DockerRuntime::new();
    let docker = runtime.client().unwrap();
    let pull = CreateImageOptionsBuilder::new().from_image(IMAGE).build();
    let mut pulling = docker.create_image(Some(pull), None, None);
    while let Some(progress) = pulling.next().await {
        progress.unwrap();
    }
    let body = ContainerCreateBody {
        image: Some(IMAGE.to_string()),
        cmd: Some(vec!["sleep".to_string(), "300".to_string()]),
        ..Default::default()
    };
    let id = docker.create_container(None, body).await.unwrap().id;
    let _cleanup = Cleanup(runtime.clone(), id.clone());
    docker.start_container(&id, None).await.unwrap();
    let within = Duration::from_secs(10);
    let shell = vec!["/bin/sh".to_string()];

    // Typed into a terminal of one size, resized, then asked about it.
    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    let input: TerminalInputs = Box::pin(tokio_stream::wrappers::ReceiverStream::new(receiver));
    let size = TerminalSize { rows: 24, columns: 80 };
    let mut output = runtime.terminal(&id, shell.clone(), size, input);
    let typed = |text: &str| TerminalInput::Data(bytes::Bytes::from(text.to_string()));
    sender.send(typed("echo \"term=$TERM\"; stty size\r")).await.unwrap();
    sender.send(TerminalInput::Resize(TerminalSize { rows: 33, columns: 101 })).await.unwrap();
    sender.send(typed("stty size; exit 4\r")).await.unwrap();
    let mut text = Vec::new();
    let exit = timeout(within, async {
        while let Some(item) = output.next().await {
            match item.unwrap() {
                ExecOutput::Stdout(bytes) | ExecOutput::Stderr(bytes) => text.extend_from_slice(&bytes),
                ExecOutput::Exited(code) => return code,
            }
        }
        panic!("no exit");
    })
    .await
    .unwrap();
    let text = String::from_utf8_lossy(&text);

    assert!(text.contains("term=xterm-256color"), "{text}");
    assert!(text.contains("24 80") && text.contains("33 101"), "{text}");
    assert_eq!(exit, 4);

    // Ctrl+D at the prompt is the end of input, as in any terminal.
    let input: TerminalInputs = Box::pin(tokio_stream::iter([typed("\u{4}")]).chain(tokio_stream::pending()));
    let ended: Vec<_> = timeout(within, runtime.terminal(&id, shell, size, input).collect::<Vec<_>>()).await.unwrap();
    assert_eq!(ended.last().map(|item| item.as_ref().unwrap()), Some(&ExecOutput::Exited(0)));
}

/// Removes a network even when an assertion fails. Declare it before the containers'
/// [`Cleanup`]s, so they go first.
struct NetworkCleanup(DockerRuntime, String);

impl Drop for NetworkCleanup {
    fn drop(&mut self) {
        let (runtime, name) = (self.0.clone(), self.1.clone());
        let _ = std::thread::spawn(move || {
            tokio::runtime::Runtime::new().unwrap().block_on(async move { runtime.client().unwrap().remove_network(&name).await })
        })
        .join();
    }
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn containers_on_an_environments_network_reach_each_other_by_alias() {
    let _networks = NETWORKS.lock().await;
    let runtime = DockerRuntime::new();
    let docker = runtime.client().unwrap();
    let pinned = runtime.pull("traefik/whoami:v1.10", &|_| {}).await.unwrap();
    runtime.pull(IMAGE, &|_| {}).await.unwrap();
    let name = format!("spinikus-test-{}", DeploymentId::new());
    let _network = NetworkCleanup(runtime.clone(), name.clone());
    let deployment = DeploymentId::new();
    let spec = ContainerSpec {
        name: format!("spinikus-{deployment}"),
        image: pinned,
        service_id: ServiceId::new(),
        deployment_id: deployment,
        env: vec![("WHOAMI_NAME".to_string(), "by-alias".to_string())],
        port: Some(80),
        volumes: Vec::new(),
        network: Some(Network { name: name.clone(), aliases: vec!["web".to_string()] }),
        command: Vec::new(),
    };

    let id = runtime.create(&spec).await.unwrap();
    let _web = Cleanup(runtime.clone(), id.clone());
    runtime.start(&id).await.unwrap();
    // A second container on the network, as another service would be.
    let client = ContainerCreateBody {
        image: Some(IMAGE.to_string()),
        cmd: Some(
            ["sh", "-c", "for i in 1 2 3 4 5 6 7 8 9 10; do wget -qO- http://web/ && exit 0; sleep 0.5; done; exit 1"]
                .map(str::to_string)
                .to_vec(),
        ),
        host_config: Some(bollard::models::HostConfig { network_mode: Some(name.clone()), ..Default::default() }),
        ..Default::default()
    };
    let client = docker.create_container(None, client).await.unwrap().id;
    let _client = Cleanup(runtime.clone(), client.clone());
    docker.start_container(&client, None).await.unwrap();
    let mut waiting = docker.wait_container(&client, None);
    let exit = timeout(Duration::from_secs(30), waiting.next()).await.unwrap();
    let output: Vec<_> = runtime.logs(&client, LogOptions::default()).collect().await;
    let output: Vec<_> = output.into_iter().map(|line| line.unwrap().text).collect();

    assert!(matches!(exit, Some(Ok(ref response)) if response.status_code == 0), "{exit:?} {output:?}");
    assert!(output.iter().any(|line| line == "Name: by-alias"), "{output:?}");
    let network = docker.inspect_network(&name, None).await.unwrap();
    assert_eq!(network.labels.unwrap_or_default().get("spinikus.network").map(String::as_str), Some("true"));
    // Joining an existing network works too.
    let again = ContainerSpec { name: format!("spinikus-{}", DeploymentId::new()), deployment_id: DeploymentId::new(), ..spec };
    let second = runtime.create(&again).await.unwrap();
    let _second = Cleanup(runtime.clone(), second);
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn runs_a_one_off_container_to_completion_and_removes_it() {
    let _networks = NETWORKS.lock().await;
    let runtime = DockerRuntime::new();
    runtime.pull(IMAGE, &|_| {}).await.unwrap();
    let network = format!("spinikus-test-{}", DeploymentId::new());
    let _network = NetworkCleanup(runtime.clone(), network.clone());
    let deployment = DeploymentId::new();
    let spec = OneOffSpec {
        name: format!("spinikus-{deployment}-release"),
        image: IMAGE.to_string(),
        service_id: ServiceId::new(),
        deployment_id: deployment,
        env: vec![("GREETING".to_string(), "migrating".to_string())],
        network: Some(Network { name: network.clone(), aliases: Vec::new() }),
        command: ["/bin/sh", "-c", "echo \"$GREETING\"; echo oops >&2; exit 3"].map(str::to_string).to_vec(),
        timeout: Duration::from_secs(60),
    };
    let _container = Cleanup(runtime.clone(), spec.name.clone());
    let lines = std::sync::Mutex::new(Vec::new());

    let exit = runtime.run_once(&spec, &|line| lines.lock().unwrap().push((line.stderr, line.text))).await.unwrap();

    assert_eq!(exit, OneOffExit::Exited(3));
    // Docker keeps each stream's order, not the order between them.
    let mut lines = lines.into_inner().unwrap();
    lines.sort();
    assert_eq!(lines, [(false, "migrating".to_string()), (true, "oops".to_string())]);
    let inspected = runtime.client().unwrap().inspect_container(&spec.name, None).await;
    assert!(inspected.is_err(), "the container is removed: {inspected:?}");
    assert!(runtime.list().await.unwrap().iter().all(|instance| instance.deployment_id != deployment));

    // Killed after its timeout, and removed.
    let slow = OneOffSpec {
        command: ["/bin/sh", "-c", "sleep 30"].map(str::to_string).to_vec(),
        timeout: Duration::from_secs(1),
        ..spec.clone()
    };
    assert_eq!(runtime.run_once(&slow, &|_| {}).await.unwrap(), OneOffExit::TimedOut);
    assert!(runtime.client().unwrap().inspect_container(&spec.name, None).await.is_err());
    // Removing one that is gone succeeds.
    runtime.remove_one_off(&spec.name).await.unwrap();
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn removes_networks_no_container_is_on() {
    let _networks = NETWORKS.lock().await;
    let runtime = DockerRuntime::new();
    runtime.pull(IMAGE, &|_| {}).await.unwrap();
    let (used, unused) = (format!("spinikus-test-{}", DeploymentId::new()), format!("spinikus-test-{}", DeploymentId::new()));
    let _used = NetworkCleanup(runtime.clone(), used.clone());
    let _unused = NetworkCleanup(runtime.clone(), unused.clone());
    let spec = |network: &str| {
        let deployment = DeploymentId::new();
        ContainerSpec {
            name: format!("spinikus-{deployment}"),
            image: IMAGE.to_string(),
            service_id: ServiceId::new(),
            deployment_id: deployment,
            env: Vec::new(),
            port: None,
            volumes: Vec::new(),
            network: Some(Network { name: network.to_string(), aliases: Vec::new() }),
            command: Vec::new(),
        }
    };
    // Never started: created containers count too.
    let kept = runtime.create(&spec(&used)).await.unwrap();
    let _kept = Cleanup(runtime.clone(), kept);
    let gone = runtime.create(&spec(&unused)).await.unwrap();
    runtime.remove(&gone).await.unwrap();

    let removed = runtime.remove_unused_networks().await.unwrap();

    assert!(removed.contains(&unused) && !removed.contains(&used), "{removed:?}");
    let docker = runtime.client().unwrap();
    assert!(docker.inspect_network(&unused, None).await.is_err());
    assert!(docker.inspect_network(&used, None).await.is_ok());
    // Only what nothing has used for a week goes, so a fresh cache stays.
    runtime.prune_build_cache(Duration::from_secs(7 * 24 * 60 * 60)).await.unwrap();
}
