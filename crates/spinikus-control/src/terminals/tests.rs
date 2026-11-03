//! Terminals up to the agent: who may open one, in which container, and what the agent is
//! asked. The sessions themselves are tested with a real agent, in the binary's tests.

use spinikus_core::{
    membership::MembershipScope,
    server::{InstanceHealth, ObservedInstance},
};
use spinikus_proto::agent::v1::{command::Kind, control_message};
use spinikus_store::InstanceRepository;

use super::*;
use crate::{
    DeploymentTimings,
    deployments::tests::{Setup, connect, deploy, setup},
    test_support::{grant, user},
};

async fn record_container(setup: &Setup, deployment: &Deployment, status: InstanceStatus) {
    let container = ObservedInstance {
        service_id: setup.service.id,
        deployment_id: deployment.id,
        container_id: "web-container".to_string(),
        status,
        health: InstanceHealth::None,
        restart_count: 0,
        started_at: None,
    };
    setup.control.store().upsert_instance(setup.server.id, container).await.unwrap();
}

#[test]
fn commands_are_checked() {
    assert!(check_command(&[]).is_ok());
    assert!(check_command(&["psql".to_string(), "-U".to_string(), "app".to_string()]).is_ok());
    assert!(check_command(&[" ".to_string()]).is_err());
    assert!(check_command(&["a\0b".to_string()]).is_err());
    assert!(check_command(&vec!["x".to_string(); MAX_COMMAND_ARGS + 1]).is_err());
    assert!(check_command(&["x".repeat(MAX_COMMAND_LEN + 1)]).is_err());
}

#[test]
fn sizes_are_bounded() {
    assert_eq!(bounded(0, 0), (0, 0));
    assert_eq!(bounded(40, 120), (40, 120));
    assert_eq!(bounded(u16::MAX, u16::MAX), (500, 1000));
}

#[tokio::test]
async fn only_admins_open_terminals_in_running_containers() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let developer = user(control, "developer@example.com", false).await;
    grant(control, &developer, MembershipScope::Environment(setup.environment.id), Role::Developer).await;
    let shell = TerminalOptions::default();

    let nothing_live = control.open_terminal(&setup.admin, setup.service.id, shell.clone()).await.err();
    let mut agent = connect(control, setup.server.id);
    let deployment = deploy(&setup, &mut agent).await;
    let no_container = control.open_terminal(&setup.admin, setup.service.id, shell.clone()).await.err();
    record_container(&setup, &deployment, InstanceStatus::Exited).await;
    let stopped = control.open_terminal(&setup.admin, setup.service.id, shell.clone()).await.err();
    record_container(&setup, &deployment, InstanceStatus::Running).await;
    let by_developer = control.open_terminal(&developer, setup.service.id, shell.clone()).await.err();
    let bad_command = TerminalOptions { command: vec![String::new()], ..Default::default() };
    let bad_command = control.open_terminal(&setup.admin, setup.service.id, bad_command).await.err();
    let psql = TerminalOptions {
        deployment_id: Some(deployment.id),
        command: vec!["psql".to_string(), "-U".to_string(), "app".to_string()],
        rows: 40,
        columns: 2000,
    };
    let opening = tokio::spawn({
        let (control, admin, service) = (control.clone(), setup.admin.clone(), setup.service.id);
        async move { control.open_terminal(&admin, service, psql).await.err() }
    });
    let asked = loop {
        let message = agent.recv().await.expect("the agent stays connected");
        if let Some(control_message::Message::Command(command)) = message.message
            && let Some(Kind::OpenSession(open)) = command.kind
        {
            break open;
        }
    };

    assert!(matches!(nothing_live, Some(ControlError::Invalid { field: "deployment_id", .. })), "{nothing_live:?}");
    assert!(matches!(no_container, Some(ControlError::Rejected { entity: "deployment", .. })), "{no_container:?}");
    assert!(matches!(&stopped, Some(ControlError::Rejected { problem, .. }) if problem.contains("not running")), "{stopped:?}");
    assert!(matches!(by_developer, Some(ControlError::Forbidden)), "{by_developer:?}");
    assert!(matches!(bad_command, Some(ControlError::Invalid { field: "command", .. })), "{bad_command:?}");
    let Some(open_session::Kind::Terminal(terminal)) = asked.kind else { panic!("not a terminal: {asked:?}") };
    assert_eq!(terminal.container_id, "web-container");
    assert_eq!((terminal.rows, terminal.columns), (40, 1000));
    assert_eq!(terminal.command, ["psql", "-U", "app"]);
    opening.abort();
}
