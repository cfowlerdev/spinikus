//! A real agent against a real control plane over mutual TLS: joining, staying connected,
//! renewing the certificate, being revoked, reporting its containers and running commands.

use std::{sync::Arc, time::Duration};

mod common;

use common::ControlPlane;
use jiff::{SignedDuration, Timestamp};
use spinikus_agent::{
    AgentError, AgentFiles, Backoff, RunOptions,
    runtime::{ContainerRuntime, MemoryRuntime},
};
use spinikus_control::ca::AgentCsr;
use spinikus_core::{
    id::{DeploymentId, ServerId, ServiceId},
    secret::Secret,
    server::{InstanceHealth, InstanceStatus, ObservedInstance, ServerStatus},
};
use spinikus_proto::agent::v1::{self as pb, command::Kind, control_message};
use spinikus_store::{InstanceRepository, ServerRepository};
use tokio::{sync::oneshot, task::JoinHandle};

/// Starts the agent in the background, with no containers. Dropping the sender stops it.
fn start_agent(files: &AgentFiles) -> (oneshot::Sender<()>, JoinHandle<Result<(), AgentError>>) {
    start_agent_with(files, Arc::new(MemoryRuntime::new()))
}

fn start_agent_with(
    files: &AgentFiles,
    runtime: Arc<dyn ContainerRuntime>,
) -> (oneshot::Sender<()>, JoinHandle<Result<(), AgentError>>) {
    let files = files.clone();
    let (stop, stopped) = oneshot::channel::<()>();
    let options = RunOptions {
        facts: spinikus_agent::server_facts(),
        backoff: Backoff::new(Duration::from_millis(50), Duration::from_millis(200)),
        runtime,
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

    let (_stop, _agent) = start_agent_with(&files, runtime.clone());
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
