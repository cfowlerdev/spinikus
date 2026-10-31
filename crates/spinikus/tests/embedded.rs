//! The embedded agent enrolling and connecting over loopback, like any other agent.

mod common;

use std::time::Duration;

use common::ControlPlane;
use spinikus_agent::{AgentFiles, Credentials};
use spinikus_core::server::ServerStatus;
use spinikus_store::AuditLogRepository;
use tokio::{sync::oneshot, task::JoinHandle};

/// Runs the embedded agent in the background. Dropping the sender stops it.
fn start(control_plane: &ControlPlane, files: &AgentFiles) -> (oneshot::Sender<()>, JoinHandle<anyhow::Result<()>>) {
    let (stop, stopped) = oneshot::channel::<()>();
    let run = spinikus::embedded::run(control_plane.control.clone(), files.clone(), control_plane.address, async move {
        let _ = stopped.await;
    });
    (stop, tokio::spawn(run))
}

async fn join_tokens_issued(control_plane: &ControlPlane) -> usize {
    let audit = control_plane.control.store().list_audit_entries(None, None, None, 100).await.unwrap();
    audit.iter().filter(|entry| entry.action.as_str() == "enrollment_token.create").count()
}

async fn stop(stop: oneshot::Sender<()>, agent: JoinHandle<anyhow::Result<()>>) {
    drop(stop);
    tokio::time::timeout(Duration::from_secs(10), agent).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn enrolls_once_then_reconnects_with_its_saved_identity() {
    let control_plane = ControlPlane::start().await;
    let dir = tempfile::tempdir().unwrap();
    let files = AgentFiles::new(dir.path().join("agent"));

    let (stopper, agent) = start(&control_plane, &files);
    let server = loop {
        if let Ok(credentials) = files.load() {
            break credentials.server_id;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    control_plane.wait_for_status(server, ServerStatus::Connected).await;
    let first = files.load().unwrap();
    stop(stopper, agent).await;
    control_plane.wait_for_status(server, ServerStatus::Disconnected).await;

    let (_stop, _agent) = start(&control_plane, &files);
    control_plane.wait_for_status(server, ServerStatus::Connected).await;

    let stored = control_plane.control.store();
    assert!(spinikus_store::ServerRepository::get_server(stored, server).await.unwrap().embedded);
    assert_eq!(first.control_plane, control_plane.url);
    assert_eq!(files.load().unwrap(), first, "the saved identity is reused");
    assert_eq!(join_tokens_issued(&control_plane).await, 1);
}

#[tokio::test]
async fn re_enrolls_when_its_files_are_from_another_control_plane() {
    let control_plane = ControlPlane::start().await;
    let other = ControlPlane::start().await;
    let dir = tempfile::tempdir().unwrap();
    let files = AgentFiles::new(dir.path().join("agent"));
    let server = control_plane.control.embedded_server("host").await.unwrap();
    // Files left over from another installation, for the same server ID.
    let (_, token) = other.join_token().await;
    let foreign = spinikus_agent::join(&other.url, &token, &other.fingerprint, spinikus_agent::server_facts()).await.unwrap();
    files.save(&Credentials { server_id: server.id, ..foreign }).unwrap();

    let (_stop, _agent) = start(&control_plane, &files);
    control_plane.wait_for_status(server.id, ServerStatus::Connected).await;

    assert_eq!(join_tokens_issued(&control_plane).await, 1);
    let ca = spinikus_control::ca::certificate_der(&files.load().unwrap().ca_pem).unwrap();
    assert_eq!(spinikus_control::ca::fingerprint(&ca), control_plane.fingerprint);
}

#[tokio::test]
async fn does_not_run_once_its_server_is_revoked() {
    let control_plane = ControlPlane::start().await;
    let dir = tempfile::tempdir().unwrap();
    let files = AgentFiles::new(dir.path().join("agent"));
    let server = control_plane.control.embedded_server("host").await.unwrap();
    control_plane.control.revoke_server(&control_plane.admin, server.id).await.unwrap();

    let (_stop, agent) = start(&control_plane, &files);
    let result = tokio::time::timeout(Duration::from_secs(5), agent).await.unwrap().unwrap();

    assert!(result.is_ok(), "{result:?}");
    assert!(files.load().is_err(), "nothing was enrolled");
    assert_eq!(join_tokens_issued(&control_plane).await, 0);
}
