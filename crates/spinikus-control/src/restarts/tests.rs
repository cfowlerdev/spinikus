//! Restarts against a fake agent that answers them.

use spinikus_core::{
    membership::MembershipScope,
    server::{InstanceHealth, InstanceStatus, ObservedInstance},
};
use spinikus_store::AuditLogRepository;
use tokio::sync::mpsc;

use super::*;
use crate::{
    DeploymentTimings,
    deployments::tests::{Setup, connect, deploy, setup},
    test_support::{grant, user},
};

/// The service's deployment live on its server, and the agent's receiver.
async fn live(setup: &Setup) -> (Deployment, mpsc::Receiver<pb::ControlMessage>) {
    let mut agent = connect(&setup.control, setup.server.id);
    let deployment = deploy(setup, &mut agent).await;
    (deployment, agent)
}

async fn record_container(setup: &Setup, deployment: &Deployment) {
    let container = ObservedInstance {
        service_id: setup.service.id,
        deployment_id: deployment.id,
        container_id: "web-container".to_string(),
        status: InstanceStatus::Running,
        health: InstanceHealth::None,
        restart_count: 0,
        started_at: None,
    };
    setup.control.store().upsert_instance(setup.server.id, container).await.unwrap();
}

/// Answers the first restart the agent gets with `outcome`, from `server`, and returns it.
fn answer(
    control: &Control,
    mut agent: mpsc::Receiver<pb::ControlMessage>,
    server: ServerId,
    outcome: Outcome,
) -> tokio::task::JoinHandle<pb::Command> {
    let control = control.clone();
    tokio::spawn(async move {
        loop {
            let message = agent.recv().await.expect("the agent stays connected");
            if let Some(control_message::Message::Command(command)) = message.message
                && let Some(Kind::RestartInstance(_)) = &command.kind
            {
                let result = pb::CommandResult { command_id: command.command_id.clone(), outcome: Some(outcome) };
                control.restart_result(server, result);
                return command;
            }
        }
    })
}

fn succeeded() -> Outcome {
    Outcome::Succeeded(pb::CommandSucceeded::default())
}

#[tokio::test]
async fn developers_restart_the_live_container() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let viewer = user(control, "viewer@example.com", false).await;
    grant(control, &viewer, MembershipScope::Environment(setup.environment.id), Role::Viewer).await;

    let nothing_live = control.restart_service(&setup.admin, setup.service.id).await;
    let (deployment, agent) = live(&setup).await;
    let no_container = control.restart_service(&setup.admin, setup.service.id).await;
    record_container(&setup, &deployment).await;
    let by_viewer = control.restart_service(&viewer, setup.service.id).await;
    let answering = answer(control, agent, setup.server.id, succeeded());
    let restarted = control.restart_service(&setup.admin, setup.service.id).await.unwrap();
    let command = answering.await.unwrap();

    assert!(matches!(nothing_live, Err(ControlError::Rejected { entity: "service", .. })), "{nothing_live:?}");
    assert!(matches!(no_container, Err(ControlError::Rejected { entity: "deployment", .. })), "{no_container:?}");
    assert!(matches!(by_viewer, Err(ControlError::Forbidden)), "{by_viewer:?}");
    assert_eq!(restarted.id, deployment.id);
    let Some(Kind::RestartInstance(restart)) = command.kind else { unreachable!() };
    assert_eq!(restart.container_id, "web-container");
    // The service's shutdown timeout.
    assert_eq!(restart.grace_period.map(|grace| grace.seconds), Some(20));
    let audit = control.store().list_audit_entries(None, None, None, 10).await.unwrap();
    assert_eq!(audit[0].action.as_str(), "service.restart");
    assert_eq!(audit[0].details["deployment_id"], deployment.id.to_string());
}

#[tokio::test]
async fn reports_what_the_agent_could_not_do() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let (deployment, agent) = live(&setup).await;
    record_container(&setup, &deployment).await;

    let failure = pb::CommandFailed { reason: pb::FailureReason::Internal.into(), message: "Docker is unreachable".to_string() };
    let answering = answer(control, agent, setup.server.id, Outcome::Failed(failure));
    let failed = control.restart_service(&setup.admin, setup.service.id).await;
    answering.await.unwrap();
    // A new connection replaces the fake agent's, then ends.
    let (connection, _) = control.agents().register(setup.server.id, mpsc::channel(1).0);
    control.agents().unregister(setup.server.id, connection);
    let disconnected = control.restart_service(&setup.admin, setup.service.id).await;

    match failed {
        Err(ControlError::Rejected { problem, .. }) => {
            assert_eq!(problem, "the agent could not restart it: Docker is unreachable")
        }
        other => panic!("{other:?}"),
    }
    assert!(
        matches!(&disconnected, Err(ControlError::Rejected { problem, .. }) if problem.contains("not connected")),
        "{disconnected:?}"
    );
}

#[tokio::test]
async fn only_the_server_it_was_sent_to_answers() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let (deployment, mut agent) = live(&setup).await;
    record_container(&setup, &deployment).await;

    let restarting = {
        let control = control.clone();
        let (admin, service) = (setup.admin.clone(), setup.service.id);
        tokio::spawn(async move { control.restart_service(&admin, service).await })
    };
    let command = loop {
        let message = agent.recv().await.unwrap();
        if let Some(control_message::Message::Command(command)) = message.message
            && matches!(command.kind, Some(Kind::RestartInstance(_)))
        {
            break command;
        }
    };
    let result = |outcome| pb::CommandResult { command_id: command.command_id.clone(), outcome: Some(outcome) };
    control.restart_result(ServerId::new(), result(succeeded()));
    tokio::task::yield_now().await;
    assert!(!restarting.is_finished());
    control.restart_result(setup.server.id, result(succeeded()));

    assert_eq!(restarting.await.unwrap().unwrap().id, deployment.id);
    assert!(is_restart_command(&command.command_id));
}
