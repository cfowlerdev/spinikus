//! A real agent against a real control plane over mutual TLS: joining, staying connected,
//! renewing the certificate, being revoked, reporting its containers and running commands.

use std::{net::SocketAddr, sync::Arc, time::Duration};

mod common;

use common::ControlPlane;
use jiff::{SignedDuration, Timestamp};
use serde_json::json;
use spinikus_agent::{
    AgentError, AgentFiles, Backoff, ProxyListeners, RunOptions,
    runtime::{ContainerRuntime, DockerRuntime, MemoryRuntime},
};
use spinikus_control::{
    ControlError,
    ca::AgentCsr,
    webhooks::{PushDelivery, PushOutcome},
};
use spinikus_core::build::image_tag;
use spinikus_core::{
    deployment::DeploymentStatus,
    id::{DeploymentId, ServerId, ServiceId},
    secret::Secret,
    server::{InstanceHealth, InstanceStatus, ObservedInstance, ServerStatus},
    source::{GitSource, ImageSource, SourceSpec},
    webhook::GitProvider,
};
use spinikus_proto::agent::v1::{self as pb, command::Kind, control_message};
use spinikus_store::{DeploymentRepository, InstanceRepository, ServerRepository, SourceRepository};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};

/// Starts the agent in the background, with no containers. Dropping the sender stops it.
fn start_agent(files: &AgentFiles) -> (oneshot::Sender<()>, JoinHandle<Result<(), AgentError>>) {
    start_agent_with(files, Arc::new(MemoryRuntime::new()), None)
}

fn start_agent_with(
    files: &AgentFiles,
    runtime: Arc<dyn ContainerRuntime>,
    proxy: Option<TcpListener>,
) -> (oneshot::Sender<()>, JoinHandle<Result<(), AgentError>>) {
    let files = files.clone();
    let (stop, stopped) = oneshot::channel::<()>();
    let options = RunOptions {
        facts: spinikus_agent::server_facts(),
        backoff: Backoff::new(Duration::from_millis(50), Duration::from_millis(200)),
        runtime,
        proxy: proxy.map(|http| ProxyListeners { http, https: None }),
        local_repositories: true,
    };
    let handle = tokio::spawn(async move {
        spinikus_agent::run(&files, options, async move {
            let _ = stopped.await;
        })
        .await
    });
    (stop, handle)
}

async fn enroll(control_plane: &ControlPlane, files: &AgentFiles) -> ServerId {
    let (server, token) = control_plane.join_token().await;
    let credentials =
        spinikus_agent::join(&control_plane.url, &token, &control_plane.fingerprint, spinikus_agent::server_facts())
            .await
            .unwrap();
    files.save(&credentials).unwrap();
    assert_eq!(credentials.server_id, server);
    server
}

fn not_after(certificate_pem: &str) -> i64 {
    let (_, pem) = x509_parser::pem::parse_x509_pem(certificate_pem.as_bytes()).unwrap();
    let (_, certificate) = x509_parser::parse_x509_certificate(&pem.contents).unwrap();
    certificate.validity().not_after.timestamp()
}

#[tokio::test]
async fn an_agent_joins_connects_and_is_revoked() {
    let control_plane = ControlPlane::start().await;
    let dir = tempfile::tempdir().unwrap();
    let files = AgentFiles::new(dir.path().join("agent"));

    let server = enroll(&control_plane, &files).await;
    let (_stop, agent) = start_agent(&files);
    control_plane.wait_for_status(server, ServerStatus::Connected).await;
    let stored = control_plane.control.store().get_server(server).await.unwrap();

    assert_eq!(stored.agent.unwrap().agent_version, spinikus_agent::AGENT_VERSION);
    assert!(control_plane.control.agents().is_connected(server));

    control_plane.control.revoke_server(&control_plane.admin, server).await.unwrap();
    let stopped = tokio::time::timeout(Duration::from_secs(10), agent).await.unwrap().unwrap();

    assert!(matches!(stopped, Err(AgentError::Revoked)), "{stopped:?}");
}

#[tokio::test]
async fn the_agent_stops_cleanly_and_reconnects_after_a_restart() {
    let control_plane = ControlPlane::start().await;
    let dir = tempfile::tempdir().unwrap();
    let files = AgentFiles::new(dir.path().join("agent"));
    let server = enroll(&control_plane, &files).await;

    let (stop, agent) = start_agent(&files);
    control_plane.wait_for_status(server, ServerStatus::Connected).await;
    drop(stop);
    let stopped = tokio::time::timeout(Duration::from_secs(10), agent).await.unwrap().unwrap();
    control_plane.wait_for_status(server, ServerStatus::Disconnected).await;
    let (_stop, _agent) = start_agent(&files);

    assert!(stopped.is_ok(), "{stopped:?}");
    control_plane.wait_for_status(server, ServerStatus::Connected).await;
}

#[tokio::test]
async fn an_old_certificate_is_renewed_on_connect() {
    let control_plane = ControlPlane::start().await;
    let dir = tempfile::tempdir().unwrap();
    let files = AgentFiles::new(dir.path().join("agent"));
    let server = enroll(&control_plane, &files).await;
    // Swap in a certificate that is 25 days into its 30, past the renewal point.
    let credentials = files.load().unwrap();
    let key = rcgen::KeyPair::from_pem(credentials.key_pem.expose()).unwrap();
    let csr = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap().serialize_request(&key).unwrap();
    let authority = control_plane.control.certificate_authority().await.unwrap();
    let issued = Timestamp::now() - SignedDuration::from_hours(25 * 24);
    let old = authority
        .issue_agent_certificate(server, &AgentCsr::from_der(csr.der()).unwrap(), issued, SignedDuration::from_hours(30 * 24))
        .unwrap();
    files.save_identity(&old, &credentials.key_pem).unwrap();

    let (_stop, _agent) = start_agent(&files);
    let mut renewed = None;
    for _ in 0..250 {
        let current = files.load().unwrap();
        if current.certificate_pem != old {
            renewed = Some(current);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let renewed = renewed.expect("the certificate was never renewed");

    assert!(not_after(&renewed.certificate_pem) > Timestamp::now().as_second() + 29 * 86_400);
    assert_ne!(renewed.key_pem, credentials.key_pem, "renewal uses a fresh key");
    control_plane.wait_for_status(server, ServerStatus::Connected).await;
}

#[tokio::test]
async fn joining_checks_the_ca_fingerprint_and_the_token() {
    let control_plane = ControlPlane::start().await;
    let (_, token) = control_plane.join_token().await;
    let facts = spinikus_agent::server_facts;
    let wrong = format!("sha256:{}", "00".repeat(32));

    let impostor = spinikus_agent::join(&control_plane.url, &token, &wrong, facts()).await;
    let bad_token = spinikus_agent::join(&control_plane.url, &Secret::new("spj_nope"), &control_plane.fingerprint, facts()).await;
    let good = spinikus_agent::join(&control_plane.url, &token, &control_plane.fingerprint, facts()).await;
    let reused = spinikus_agent::join(&control_plane.url, &token, &control_plane.fingerprint, facts()).await;
    let not_https = spinikus_agent::join("http://127.0.0.1:1", &token, &control_plane.fingerprint, facts()).await;

    assert!(matches!(impostor, Err(AgentError::FingerprintMismatch { .. })), "{impostor:?}");
    assert!(matches!(bad_token, Err(AgentError::Refused(_))), "{bad_token:?}");
    assert!(good.is_ok(), "the token was not used up by the failed attempts: {good:?}");
    assert!(matches!(reused, Err(AgentError::Refused(_))), "{reused:?}");
    assert!(matches!(not_https, Err(AgentError::InvalidUrl(_))), "{not_https:?}");
}

fn container(id: &str, deployment: DeploymentId) -> ObservedInstance {
    ObservedInstance {
        service_id: ServiceId::new(),
        deployment_id: deployment,
        container_id: id.to_string(),
        status: InstanceStatus::Running,
        health: InstanceHealth::Healthy,
        restart_count: 0,
        started_at: Some(Timestamp::now()),
    }
}

/// Waits until the control plane knows exactly these containers on `server`.
async fn wait_for_containers(control_plane: &ControlPlane, server: ServerId, expected: &[&str]) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let instances = control_plane.control.store().list_server_instances(server).await.unwrap();
        let mut containers: Vec<_> = instances.iter().map(|instance| instance.observed.container_id.as_str()).collect();
        containers.sort_unstable();
        if containers == expected {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "expected containers {expected:?}, the control plane has {containers:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn the_agent_reports_its_containers_and_stops_a_deployment() {
    let control_plane = ControlPlane::start().await;
    let dir = tempfile::tempdir().unwrap();
    let files = AgentFiles::new(dir.path().join("agent"));
    let server = enroll(&control_plane, &files).await;
    let runtime = Arc::new(MemoryRuntime::new());
    let (draining, live) = (DeploymentId::new(), DeploymentId::new());
    runtime.put(container("old", draining));
    runtime.put(container("new", live));

    let (_stop, _agent) = start_agent_with(&files, runtime.clone(), None);
    wait_for_containers(&control_plane, server, &["new", "old"]).await;
    runtime.put(container("newer", live));
    wait_for_containers(&control_plane, server, &["new", "newer", "old"]).await;

    let stop = pb::StopDeployment { deployment_id: draining.to_string(), grace_period: None };
    let command = pb::Command { command_id: "cmd-1".to_string(), kind: Some(Kind::StopDeployment(stop)) };
    let message = pb::ControlMessage { message: Some(control_message::Message::Command(command)) };
    control_plane.control.agents().send(server, message).await.unwrap();
    wait_for_containers(&control_plane, server, &["new", "newer"]).await;

    assert_eq!(runtime.get("old"), None);
    let instances = control_plane.control.store().list_server_instances(server).await.unwrap();
    let new = instances.iter().find(|instance| instance.observed.container_id == "new").unwrap();
    assert_eq!(new.observed.deployment_id, live);
    assert_eq!(new.observed.health, InstanceHealth::Healthy);
}

fn send_command(control_plane: &ControlPlane, server: ServerId, id: &str, kind: Kind) -> impl Future<Output = ()> {
    let command = pb::Command { command_id: id.to_string(), kind: Some(kind) };
    let message = pb::ControlMessage { message: Some(control_message::Message::Command(command)) };
    let agents = control_plane.control.agents().clone();
    async move { agents.send(server, message).await.unwrap() }
}

/// A proxy listener on a free loopback port.
async fn proxy_listener() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    (listener, address)
}

/// A raw HTTP/1.1 GET through the proxy, returning the status and body.
async fn get(proxy: SocketAddr, host: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(proxy).await.unwrap();
    let request = format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    (head[9..12].parse().unwrap(), body.to_string())
}

/// Waits until the proxy answers for `host` with `status`, as routes arrive after go-live.
async fn wait_for_response(proxy: SocketAddr, host: &str, status: u16, within: Duration) -> String {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let (current, body) = get(proxy, host).await;
        if current == status {
            return body;
        }
        assert!(tokio::time::Instant::now() < deadline, "the proxy answers {current} for {host}, expected {status}: {body}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_deployment_goes_live_behind_the_proxy_and_the_next_one_supersedes_it() {
    let control_plane = ControlPlane::start().await;
    let dir = tempfile::tempdir().unwrap();
    let files = AgentFiles::new(dir.path().join("agent"));
    let server = enroll(&control_plane, &files).await;
    let runtime = Arc::new(MemoryRuntime::new());
    runtime.add_image("ghcr.io/acme/web:1", "ghcr.io/acme/web@sha256:01");
    // Every container's app port is this little app.
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    runtime.set_endpoint(app.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(app, axum::Router::new().route("/", axum::routing::get(|| async { "hello" }))).await });
    let (listener, proxy) = proxy_listener().await;
    let (_stop, _agent) = start_agent_with(&files, runtime.clone(), Some(listener));
    control_plane.wait_for_status(server, ServerStatus::Connected).await;
    let settings = json!({ "port": 3000, "healthcheck": { "start_period": "0s" } });
    let service = control_plane.service(server, "ghcr.io/acme/web:1", settings).await;
    let control = &control_plane.control;
    let host = "web-production-shop.localhost";

    let before = get(proxy, host).await.0;
    let first = control.create_deployment(&control_plane.admin, service).await.unwrap();
    control_plane.wait_for_deployment(first.id, DeploymentStatus::Live, Duration::from_secs(10)).await;
    wait_for_deployment(&control_plane, server, first.id, Some(InstanceStatus::Running)).await;
    let served = wait_for_response(proxy, host, 200, Duration::from_secs(10)).await;
    let second = control.create_deployment(&control_plane.admin, service).await.unwrap();
    control_plane.wait_for_deployment(second.id, DeploymentStatus::Live, Duration::from_secs(10)).await;
    control_plane.wait_for_deployment(first.id, DeploymentStatus::Superseded, Duration::from_secs(10)).await;
    wait_for_deployment(&control_plane, server, first.id, None).await;

    let after = get(proxy, host).await;

    assert_eq!(before, 404);
    assert_eq!(served, "hello");
    assert_eq!(after, (200, "hello".to_string()));
    let routes = files.load_routes().unwrap();
    assert!(routes.targets(second.id) && !routes.targets(first.id), "{routes:?}");
    let first = control.store().get_deployment(first.id).await.unwrap();
    assert_eq!(first.image.as_deref(), Some("ghcr.io/acme/web@sha256:01"));
    let path: Vec<_> = control.store().list_deployment_events(first.id).await.unwrap().iter().map(|event| event.to).collect();
    use DeploymentStatus::*;
    assert_eq!(path, [Queued, Preparing, Starting, Verifying, Live, Draining, Superseded]);
    let instances = control.store().list_server_instances(server).await.unwrap();
    assert_eq!(instances.iter().map(|instance| instance.observed.deployment_id).collect::<Vec<_>>(), [second.id]);
    assert_eq!(runtime.spec(&instances[0].observed.container_id).unwrap().service_id, service);
}

/// Waits for the instance of `deployment` to have `status`, or to be gone if `None`.
async fn wait_for_deployment(
    control_plane: &ControlPlane,
    server: ServerId,
    deployment: DeploymentId,
    status: Option<InstanceStatus>,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let instances = control_plane.control.store().list_server_instances(server).await.unwrap();
        let current = instances
            .iter()
            .find(|instance| instance.observed.deployment_id == deployment)
            .map(|instance| instance.observed.status);
        if current == status {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "the deployment is {current:?}, expected {status:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
#[ignore = "needs a Docker daemon; pulls traefik/whoami"]
async fn the_agent_deploys_an_image_with_docker() {
    let control_plane = ControlPlane::start().await;
    let dir = tempfile::tempdir().unwrap();
    let files = AgentFiles::new(dir.path().join("agent"));
    let server = enroll(&control_plane, &files).await;
    let (listener, proxy) = proxy_listener().await;
    let (_stop, _agent) = start_agent_with(&files, Arc::new(DockerRuntime::new()), Some(listener));
    control_plane.wait_for_status(server, ServerStatus::Connected).await;
    let runtime = json!({ "port": 80, "healthcheck": { "path": "/" }, "shutdown_timeout": "2s" });
    let service = control_plane.service(server, "traefik/whoami:v1.10", runtime).await;
    let control = &control_plane.control;
    let host = "web-production-shop.localhost";

    let first = control.create_deployment(&control_plane.admin, service).await.unwrap();
    control_plane.wait_for_deployment(first.id, DeploymentStatus::Live, Duration::from_secs(120)).await;
    let served = wait_for_response(proxy, host, 200, Duration::from_secs(10)).await;
    // Requests keep going through the cutover; none may fail.
    let (stop_requests, mut requests_stopped) = oneshot::channel::<()>();
    let requests = tokio::spawn(async move {
        let mut failures = Vec::new();
        let mut count = 0;
        while requests_stopped.try_recv().is_err() {
            let (status, body) = get(proxy, host).await;
            count += 1;
            if status != 200 {
                failures.push((status, body));
            }
        }
        (count, failures)
    });
    let second = control.create_deployment(&control_plane.admin, service).await.unwrap();
    control_plane.wait_for_deployment(second.id, DeploymentStatus::Live, Duration::from_secs(60)).await;
    control_plane.wait_for_deployment(first.id, DeploymentStatus::Superseded, Duration::from_secs(30)).await;
    wait_for_deployment(&control_plane, server, first.id, None).await;
    let _ = stop_requests.send(());
    let (count, failures) = requests.await.unwrap();
    let (status, body) = get(proxy, host).await;

    // whoami answers with the request it saw, including the forwarding headers.
    assert!(served.contains(&format!("Host: {host}")), "{served}");
    assert!(served.contains("X-Forwarded-For: 127.0.0.1"), "{served}");
    assert_eq!(status, 200, "{body}");
    assert!(count > 10 && failures.is_empty(), "{} of {count} requests failed: {failures:?}", failures.len());
    // Clean up: stop the live one too.
    let stop = pb::StopDeployment { deployment_id: second.id.to_string(), grace_period: None };
    send_command(&control_plane, server, "cleanup", Kind::StopDeployment(stop)).await;
    wait_for_deployment(&control_plane, server, second.id, None).await;
    assert!(remove_network(&control_plane, service).await, "the environment's network was not there");
}

/// Removes the network of the service's environment, which its containers joined, and
/// says whether it was there.
async fn remove_network(control_plane: &ControlPlane, service: ServiceId) -> bool {
    let environment = control_plane.control.get_service(&control_plane.admin, service).await.unwrap().environment_id;
    let network = format!("spinikus-{environment}");
    std::process::Command::new("docker").args(["network", "rm", &network]).output().unwrap().status.success()
}

/// Runs git in `dir` for setting up a test repository, and returns its output.
fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[tokio::test]
async fn a_deployment_is_built_from_git_with_its_app_file() {
    let logs = tempfile::tempdir().unwrap();
    let log_dir = logs.path().to_path_buf();
    let control_plane = ControlPlane::start_with(|control| control.with_log_dir(log_dir)).await;
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("web")).unwrap();
    std::fs::write(repo.join("web/Dockerfile"), "FROM scratch\n").unwrap();
    std::fs::write(repo.join("web/spinikus.json"), r#"{ "runtime": { "port": 8080 } } // the app's port"#).unwrap();
    git(&repo, &["init", "--quiet"]);
    git(&repo, &["add", "--all"]);
    git(&repo, &["commit", "--quiet", "--message", "first"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let files = AgentFiles::new(dir.path().join("agent"));
    let server = enroll(&control_plane, &files).await;
    let runtime = Arc::new(MemoryRuntime::new());
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    runtime.set_endpoint(app.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(app, axum::Router::new().route("/", axum::routing::get(|| async { "built" }))).await });
    let (listener, proxy) = proxy_listener().await;
    let (_stop, _agent) = start_agent_with(&files, runtime.clone(), Some(listener));
    control_plane.wait_for_status(server, ServerStatus::Connected).await;
    // No port here: the app file sets it.
    let service = control_plane.service(server, "unused:1", json!({ "healthcheck": { "start_period": "0s" } })).await;
    let control = &control_plane.control;
    // The service layer allows only remote URLs; the agent here allows local repositories.
    let source = SourceSpec::Git(GitSource {
        repo_url: repo.to_string_lossy().into_owned(),
        branch: "main".to_string(),
        root_directory: Some("web".to_string()),
    });
    control.store().update_source(service, 1, source).await.unwrap();

    let deployment = control.create_deployment(&control_plane.admin, service).await.unwrap();
    control_plane.wait_for_deployment(deployment.id, DeploymentStatus::Live, Duration::from_secs(20)).await;
    let served = wait_for_response(proxy, "web-production-shop.localhost", 200, Duration::from_secs(10)).await;

    assert_eq!(served, "built");
    let deployment = control.store().get_deployment(deployment.id).await.unwrap();
    assert_eq!(deployment.commit_sha.as_deref(), Some(commit.as_str()));
    assert!(deployment.image.as_deref().unwrap().starts_with("sha256:"), "{:?}", deployment.image);
    assert_eq!(deployment.snapshot.from_file, ["runtime.port"]);
    assert_eq!(deployment.snapshot.config["runtime"]["port"], 8080);
    let (spec, dockerfile) = runtime.builds().pop().unwrap();
    assert_eq!(
        (spec.tag, dockerfile.as_str()),
        (
            format!("spinikus/{}:{}", service.to_string().to_lowercase(), deployment.id.to_string().to_lowercase()),
            "FROM scratch\n"
        )
    );
    let log = control.deployment_logs(&control_plane.admin, deployment.id).await.unwrap();
    for expected in ["==> cloning", "==> building", &commit[..12]] {
        assert!(log.contains(expected), "{expected:?} is not in the log:\n{log}");
    }
}

#[tokio::test]
#[ignore = "needs a Docker daemon; pulls traefik/whoami"]
async fn the_agent_builds_from_git_and_deploys_with_docker() {
    let control_plane = ControlPlane::start().await;
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("Dockerfile"), "FROM traefik/whoami:v1.10\nARG GREETING\nLABEL greeting=$GREETING\n").unwrap();
    std::fs::write(repo.join("spinikus.json"), r#"{ "runtime": { "port": 80, "healthcheck": { "path": "/" } } }"#).unwrap();
    git(&repo, &["init", "--quiet"]);
    git(&repo, &["add", "--all"]);
    git(&repo, &["commit", "--quiet", "--message", "first"]);
    let files = AgentFiles::new(dir.path().join("agent"));
    let server = enroll(&control_plane, &files).await;
    let (listener, proxy) = proxy_listener().await;
    let (_stop, _agent) = start_agent_with(&files, Arc::new(DockerRuntime::new()), Some(listener));
    control_plane.wait_for_status(server, ServerStatus::Connected).await;
    let service = control_plane.service(server, "unused:1", json!({ "shutdown_timeout": "2s" })).await;
    let control = &control_plane.control;
    let source = SourceSpec::Git(GitSource {
        repo_url: repo.to_string_lossy().into_owned(),
        branch: "main".to_string(),
        root_directory: None,
    });
    control.store().update_source(service, 1, source).await.unwrap();

    let admin = &control_plane.admin;
    let within = Duration::from_secs(180);
    let kept = control.set_setting(admin, "releases_kept", 0, json!(1)).await.unwrap();

    let first = control.create_deployment(admin, service).await.unwrap();
    control_plane.wait_for_deployment(first.id, DeploymentStatus::Live, within).await;
    let served = wait_for_response(proxy, "web-production-shop.localhost", 200, Duration::from_secs(10)).await;
    std::fs::write(repo.join("Dockerfile"), "FROM traefik/whoami:v1.10\nLABEL version=2\n").unwrap();
    git(&repo, &["commit", "--quiet", "--all", "--message", "second"]);
    let second = control.create_deployment(admin, service).await.unwrap();
    control_plane.wait_for_deployment(second.id, DeploymentStatus::Live, within).await;
    control_plane.wait_for_deployment(first.id, DeploymentStatus::Superseded, within).await;
    let rollback = control.rollback_service(admin, service, None).await.unwrap();
    control_plane.wait_for_deployment(rollback.id, DeploymentStatus::Live, within).await;
    control_plane.wait_for_deployment(second.id, DeploymentStatus::Superseded, within).await;
    control.set_setting(admin, "releases_kept", kept.revision, json!(0)).await.unwrap();
    let deadline = tokio::time::Instant::now() + within;
    while control.store().get_deployment(second.id).await.unwrap().cleaned_at.is_none() {
        assert!(tokio::time::Instant::now() < deadline, "the second deployment was never cleaned up");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let image_exists = |reference: &str| {
        std::process::Command::new("docker").args(["image", "inspect", reference]).output().unwrap().status.success()
    };
    let (first, second) =
        (control.store().get_deployment(first.id).await.unwrap(), control.store().get_deployment(second.id).await.unwrap());
    let (first_tag, second_tag) = (image_tag(service, first.id), image_tag(service, second.id));
    let (first_image, second_image) = (first.image.clone().unwrap(), second.image.clone().unwrap());
    let kept_first = (image_exists(&first_tag), image_exists(&first_image));
    let removed_second = (image_exists(&second_tag), image_exists(&second_image));
    // Clean up: stop the rollback, then remove the first image.
    let stop = pb::StopDeployment { deployment_id: rollback.id.to_string(), grace_period: None };
    send_command(&control_plane, server, "cleanup", Kind::StopDeployment(stop)).await;
    wait_for_deployment(&control_plane, server, rollback.id, None).await;
    let removed = std::process::Command::new("docker").args(["image", "rm", "--force", &first_tag]).output().unwrap();
    remove_network(&control_plane, service).await;

    assert!(served.contains("Host: web-production-shop.localhost"), "{served}");
    assert!(first_image.starts_with("sha256:") && first_image != second_image, "{first_image} {second_image}");
    assert_eq!(first.snapshot.from_file, ["runtime.healthcheck", "runtime.port"]);
    assert_eq!(rollback.image.as_deref(), Some(first_image.as_str()));
    assert!(first.cleaned_at.is_some() && second.cleaned_at.is_some());
    // The rollback runs the first image, so it stays; the second is gone.
    assert_eq!(kept_first, (true, true));
    assert_eq!(removed_second, (false, false));
    assert!(removed.status.success(), "{}", String::from_utf8_lossy(&removed.stderr));
}

#[tokio::test]
async fn a_rollback_runs_the_earlier_image_and_cleanup_removes_what_no_one_needs() {
    let control_plane = ControlPlane::start().await;
    let dir = tempfile::tempdir().unwrap();
    let files = AgentFiles::new(dir.path().join("agent"));
    let server = enroll(&control_plane, &files).await;
    let runtime = Arc::new(MemoryRuntime::new());
    runtime.add_image("ghcr.io/acme/web:1", "ghcr.io/acme/web@sha256:01");
    runtime.add_image("ghcr.io/acme/web:2", "ghcr.io/acme/web@sha256:02");
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    runtime.set_endpoint(app.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(app, axum::Router::new().route("/", axum::routing::get(|| async { "hello" }))).await });
    let (listener, _proxy) = proxy_listener().await;
    let (_stop, _agent) = start_agent_with(&files, runtime.clone(), Some(listener));
    control_plane.wait_for_status(server, ServerStatus::Connected).await;
    let settings = json!({ "port": 3000, "healthcheck": { "start_period": "0s" } });
    let service = control_plane.service(server, "ghcr.io/acme/web:1", settings).await;
    let (control, admin) = (&control_plane.control, &control_plane.admin);
    let kept = control.set_setting(admin, "releases_kept", 0, json!(1)).await.unwrap();
    let within = Duration::from_secs(10);

    let first = control.create_deployment(admin, service).await.unwrap();
    control_plane.wait_for_deployment(first.id, DeploymentStatus::Live, within).await;
    let source = control.get_source(admin, service).await.unwrap();
    let image = SourceSpec::Image(ImageSource { image: "ghcr.io/acme/web:2".to_string() });
    control.replace_source(admin, service, source.revision, image).await.unwrap();
    let second = control.create_deployment(admin, service).await.unwrap();
    control_plane.wait_for_deployment(second.id, DeploymentStatus::Live, within).await;
    control_plane.wait_for_deployment(first.id, DeploymentStatus::Superseded, within).await;
    let rollback = control.rollback_service(admin, service, None).await.unwrap();
    control_plane.wait_for_deployment(rollback.id, DeploymentStatus::Live, within).await;
    control_plane.wait_for_deployment(second.id, DeploymentStatus::Superseded, within).await;
    // Keeping no releases cleans up both earlier deployments.
    control.set_setting(admin, "releases_kept", kept.revision, json!(0)).await.unwrap();
    let deadline = tokio::time::Instant::now() + within;
    while control.store().get_deployment(second.id).await.unwrap().cleaned_at.is_none() {
        assert!(tokio::time::Instant::now() < deadline, "the second deployment was never cleaned up");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(rollback.rollback_of, Some(first.id));
    let instances = control.store().list_server_instances(server).await.unwrap();
    assert_eq!(instances.iter().map(|instance| instance.observed.deployment_id).collect::<Vec<_>>(), [rollback.id]);
    assert_eq!(runtime.spec(&instances[0].observed.container_id).unwrap().image, "ghcr.io/acme/web@sha256:01");
    // The second's image is gone; the first's stays, since the rollback runs it.
    assert_eq!(runtime.removed_images(), ["ghcr.io/acme/web@sha256:02"]);
    assert!(runtime.has_image("ghcr.io/acme/web@sha256:01"));
    let first = control.store().get_deployment(first.id).await.unwrap();
    assert!(first.cleaned_at.is_some());
    let gone = control.rollback_service(admin, service, Some(first.id)).await;
    assert!(gone.is_err());
}

#[tokio::test]
async fn a_push_deploys_its_own_commit_even_if_the_branch_moved_on() {
    let control_plane = ControlPlane::start().await;
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("Dockerfile"), "FROM scratch\nLABEL version=1\n").unwrap();
    git(&repo, &["init", "--quiet"]);
    git(&repo, &["add", "--all"]);
    git(&repo, &["commit", "--quiet", "--message", "first"]);
    let pushed = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("Dockerfile"), "FROM scratch\nLABEL version=2\n").unwrap();
    git(&repo, &["commit", "--quiet", "--all", "--message", "second"]);
    let files = AgentFiles::new(dir.path().join("agent"));
    let server = enroll(&control_plane, &files).await;
    let runtime = Arc::new(MemoryRuntime::new());
    let (_stop, _agent) = start_agent_with(&files, runtime.clone(), None);
    control_plane.wait_for_status(server, ServerStatus::Connected).await;
    let service = control_plane.service(server, "unused:1", json!({ "healthcheck": { "start_period": "0s" } })).await;
    let (control, admin) = (&control_plane.control, &control_plane.admin);
    let https = SourceSpec::Git(GitSource {
        repo_url: "https://example.com/acme/web.git".to_string(),
        branch: "main".to_string(),
        root_directory: None,
    });
    let source = control.replace_source(admin, service, 1, https).await.unwrap();
    let hook = control.create_webhook(admin, service).await.unwrap();
    // The service layer allows only remote URLs; the agent here allows local repositories.
    let local = SourceSpec::Git(GitSource {
        repo_url: repo.to_string_lossy().into_owned(),
        branch: "main".to_string(),
        root_directory: None,
    });
    control.store().update_source(service, source.revision, local).await.unwrap();
    let body = json!({
        "ref": "refs/heads/main", "before": "1111111111111111111111111111111111111111", "after": pushed,
        "commits": [{ "added": ["Dockerfile"], "modified": [], "removed": [] }],
    })
    .to_string();
    let signature = {
        use hmac::{Hmac, KeyInit, Mac};
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(hook.secret.expose().as_bytes()).unwrap();
        mac.update(body.as_bytes());
        mac.finalize().into_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>()
    };

    let delivery = PushDelivery { event: Some("push"), signature: Some(&signature), body: body.as_bytes() };
    let outcome = control.receive_push(GitProvider::Gitea, hook.webhook.id, delivery).await.unwrap();
    let PushOutcome::Deployed(deployment) = outcome else { panic!("{outcome:?}") };
    control_plane.wait_for_deployment(deployment.id, DeploymentStatus::Live, Duration::from_secs(20)).await;

    let deployment = control.store().get_deployment(deployment.id).await.unwrap();
    assert_eq!(deployment.commit_sha.as_deref(), Some(pushed.as_str()));
    let (_, dockerfile) = runtime.builds().pop().unwrap();
    assert_eq!(dockerfile, "FROM scratch\nLABEL version=1\n");
}

#[tokio::test]
async fn runtime_logs_come_from_the_agent_and_stop_when_nobody_reads() {
    use spinikus_control::service_logs::{ContainerLogUpdate, LogQuery};
    use tokio_stream::StreamExt;

    let control_plane = ControlPlane::start().await;
    let dir = tempfile::tempdir().unwrap();
    let files = AgentFiles::new(dir.path().join("agent"));
    let server = enroll(&control_plane, &files).await;
    let runtime = Arc::new(MemoryRuntime::new());
    runtime.add_image("ghcr.io/acme/web:1", "ghcr.io/acme/web@sha256:01");
    let (stop, _agent) = start_agent_with(&files, runtime.clone(), None);
    control_plane.wait_for_status(server, ServerStatus::Connected).await;
    let service = control_plane.service(server, "ghcr.io/acme/web:1", json!({ "healthcheck": { "start_period": "0s" } })).await;
    let (control, admin) = (&control_plane.control, &control_plane.admin);
    let deployment = control.create_deployment(admin, service).await.unwrap();
    control_plane.wait_for_deployment(deployment.id, DeploymentStatus::Live, Duration::from_secs(10)).await;
    wait_for_deployment(&control_plane, server, deployment.id, Some(InstanceStatus::Running)).await;
    let container = control.store().list_server_instances(server).await.unwrap()[0].observed.container_id.clone();
    for line in ["booting", "listening on :3000", "ready"] {
        runtime.write_log(&container, false, line);
    }
    runtime.write_log(&container, true, "warning: no cache");
    let within = Duration::from_secs(5);

    let tail = control.service_logs(admin, service, LogQuery { tail: Some(2), ..Default::default() }).await.unwrap();
    let tail: Vec<_> = tokio::time::timeout(within, tail.collect()).await.unwrap();
    let mut followed =
        control.service_logs(admin, service, LogQuery { tail: Some(1), follow: true, ..Default::default() }).await.unwrap();
    let last = tokio::time::timeout(within, followed.next()).await.unwrap().unwrap();
    runtime.write_log(&container, false, "GET / 200");
    let new = tokio::time::timeout(within, followed.next()).await.unwrap().unwrap();
    let following = runtime.log_followers();
    drop(followed);
    let deadline = tokio::time::Instant::now() + within;
    while runtime.log_followers() == following {
        assert!(tokio::time::Instant::now() < deadline, "the agent kept following the log");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let text = |update: &ContainerLogUpdate| match update {
        ContainerLogUpdate::Line { text, stderr, .. } => format!("{}{text}", if *stderr { "! " } else { "" }),
        ContainerLogUpdate::End { error } => format!("end {error:?}"),
    };
    assert_eq!(tail.iter().map(text).collect::<Vec<_>>(), ["ready", "! warning: no cache", "end None"]);
    assert_eq!((text(&last), text(&new)), ("! warning: no cache".to_string(), "GET / 200".to_string()));
    let unknown =
        control.service_logs(admin, service, LogQuery { deployment_id: Some(DeploymentId::new()), ..Default::default() }).await;
    assert!(matches!(unknown, Err(ControlError::NotFound { .. })), "{unknown:?}");
    drop(stop);
    control_plane.wait_for_status(server, ServerStatus::Disconnected).await;
    let away = control.service_logs(admin, service, LogQuery::default()).await;
    assert!(matches!(away, Err(ControlError::Rejected { .. })), "{away:?}");
}
