//! The deployment operations and the runner, against a fake agent: a channel registered in
//! the hub receives the commands, and the tests answer them like an agent would.

use std::time::Duration;

use serde_json::json;
use spinikus_core::{
    actor::Actor,
    deployment::{DeploymentStatus::*, EndReason},
    environment::EnvironmentKind,
    membership::MembershipScope,
    secret::Secret,
    server::Server,
    service::ServiceKind,
    source::{ImageSource, SourceSpec},
    variable::{VariableAvailability, VariableFields, VariableScope},
};
use spinikus_proto::agent::v1::{self as pb, command::Kind, command_result::Outcome, control_message};
use spinikus_store::VariableRepository;
use tokio::sync::mpsc;

use super::*;
use crate::{
    NewEnvironment, NewProject, NewServer, NewService,
    test_support::{control, grant, user},
};

struct Setup {
    _dir: tempfile::TempDir,
    control: Control,
    admin: Principal,
    environment: Environment,
    service: Service,
    server: Server,
}

/// A server with a fake agent, and an app service placed on it with an image source.
async fn setup(timings: DeploymentTimings) -> Setup {
    let (dir, control) = control().await;
    let control = control.with_deployment_timings(timings);
    let admin = user(&control, "admin@example.com", true).await;
    let project = control.create_project(&admin, NewProject { name: "Shop".to_string(), slug: None }).await.unwrap();
    let new = NewEnvironment { name: "Production".to_string(), slug: None, kind: EnvironmentKind::Production };
    let environment = control.create_environment(&admin, project.id, new).await.unwrap();
    let server = control.create_server(&admin, NewServer { name: "web-1".to_string() }).await.unwrap();
    let config = json!({ "runtime": { "port": 3000, "healthcheck": { "path": "/healthz" }, "shutdown_timeout": "20s" } });
    let new = NewService {
        name: "Web".to_string(),
        slug: None,
        kind: ServiceKind::App,
        config: config.as_object().cloned().unwrap(),
        server_id: Some(server.id),
    };
    let service = control.create_service(&admin, environment.id, new).await.unwrap();
    let image = SourceSpec::Image(ImageSource { image: "ghcr.io/acme/web:1".to_string() });
    control.create_source(&admin, service.id, image).await.unwrap();
    Setup { _dir: dir, control, admin, environment, service, server }
}

/// Registers a fake agent for `server`, returning what it receives.
fn connect(control: &Control, server: ServerId) -> mpsc::Receiver<pb::ControlMessage> {
    let (sender, receiver) = mpsc::channel(64);
    let _ = control.agents().register(server, sender);
    receiver
}

/// The commands sent so far.
fn sent(agent: &mut mpsc::Receiver<pb::ControlMessage>) -> Vec<pb::Command> {
    let mut commands = Vec::new();
    while let Ok(message) = agent.try_recv() {
        if let Some(control_message::Message::Command(command)) = message.message {
            commands.push(command);
        }
    }
    commands
}

fn only(mut commands: Vec<pb::Command>) -> pb::Command {
    assert_eq!(commands.len(), 1, "{commands:?}");
    commands.pop().unwrap()
}

fn succeeded(command: &pb::Command, prepared: Option<&str>) -> pb::CommandResult {
    let prepared = prepared.map(|image| pb::PreparedImage { image: image.to_string(), commit_sha: String::new() });
    pb::CommandResult {
        command_id: command.command_id.clone(),
        outcome: Some(Outcome::Succeeded(pb::CommandSucceeded { prepared })),
    }
}

fn failed(command: &pb::Command, reason: pb::FailureReason, message: &str) -> pb::CommandResult {
    let failed = pb::CommandFailed { reason: reason.into(), message: message.to_string() };
    pb::CommandResult { command_id: command.command_id.clone(), outcome: Some(Outcome::Failed(failed)) }
}

async fn status(control: &Control, id: DeploymentId) -> DeploymentStatus {
    control.store().get_deployment(id).await.unwrap().status
}

/// Walks a new deployment to `live` through the fake agent.
async fn deploy(setup: &Setup, agent: &mut mpsc::Receiver<pb::ControlMessage>) -> Deployment {
    let control = &setup.control;
    let deployment = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    control.advance_deployments().await;
    let prepare = only(sent(agent));
    control.deployment_result(setup.server.id, succeeded(&prepare, Some("ghcr.io/acme/web@sha256:01"))).await;
    let start = only(sent(agent));
    control.deployment_result(setup.server.id, succeeded(&start, None)).await;
    let deployment = control.store().get_deployment(deployment.id).await.unwrap();
    assert_eq!(deployment.status, Live);
    deployment
}

#[tokio::test]
async fn prepares_starts_and_verifies_a_deployment() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let variable = |name: &str, availability| VariableFields { name: name.parse().unwrap(), availability, secret: true };
    let scope = VariableScope::Service(setup.service.id);
    control.store().create_variable(scope, variable("API_KEY", VariableAvailability::Runtime), Secret::new("k")).await.unwrap();
    control.store().create_variable(scope, variable("NPM_TOKEN", VariableAvailability::Build), Secret::new("t")).await.unwrap();
    let mut agent = connect(control, setup.server.id);

    let deployment = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    assert_eq!((deployment.status, deployment.server_id), (Queued, setup.server.id));
    control.advance_deployments().await;

    let prepare = only(sent(&mut agent));
    assert_eq!(prepare.command_id, format!("{}/prepare", deployment.id));
    let Some(Kind::PrepareDeployment(prepare_deployment)) = &prepare.kind else { panic!("{prepare:?}") };
    let Some(pb::prepare_deployment::Source::Image(image)) = &prepare_deployment.source else { panic!() };
    assert_eq!(image.image, "ghcr.io/acme/web:1");
    let names: Vec<_> = prepare_deployment.build_variables.iter().map(|variable| variable.name.as_str()).collect();
    assert_eq!(names, ["NPM_TOKEN"]);
    assert_eq!(status(control, deployment.id).await, Preparing);

    // A pass before the result does not send the command again.
    control.advance_deployments().await;
    assert!(sent(&mut agent).is_empty());

    control.deployment_result(setup.server.id, succeeded(&prepare, Some("ghcr.io/acme/web@sha256:01"))).await;
    let start = only(sent(&mut agent));
    let Some(Kind::StartDeployment(start_deployment)) = &start.kind else { panic!("{start:?}") };
    assert_eq!(start_deployment.image, "ghcr.io/acme/web@sha256:01");
    assert_eq!(start_deployment.port, 3000);
    assert_eq!(start_deployment.health_check.as_ref().unwrap().path, "/healthz");
    assert_eq!(start_deployment.health_check.as_ref().unwrap().retries, 3);
    assert_eq!(start_deployment.strategy, pb::DeploymentStrategy::BlueGreen as i32);
    let variables: Vec<_> = start_deployment
        .variables
        .iter()
        .map(|variable| (variable.name.as_str(), variable.value.as_str(), variable.secret))
        .collect();
    assert_eq!(variables, [("API_KEY", "k", true)]);
    assert_eq!(control.store().get_deployment(deployment.id).await.unwrap().image.as_deref(), Some("ghcr.io/acme/web@sha256:01"));

    let progress =
        pb::CommandProgress { command_id: start.command_id.clone(), phase: "verifying".to_string(), log_lines: Vec::new() };
    control.deployment_progress(setup.server.id, progress).await;
    assert_eq!(status(control, deployment.id).await, Verifying);
    control.deployment_result(setup.server.id, succeeded(&start, None)).await;

    let events = control.list_deployment_events(&setup.admin, deployment.id).await.unwrap();
    let path: Vec<_> = events.iter().map(|event| event.to).collect();
    assert_eq!(path, [Queued, Preparing, Starting, Verifying, Live]);
    assert_eq!(events.last().unwrap().actor, Actor::Server(setup.server.id));
}

#[tokio::test]
async fn a_new_live_deployment_drains_the_previous_one() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let mut agent = connect(control, setup.server.id);
    let first = deploy(&setup, &mut agent).await;

    let second = deploy(&setup, &mut agent).await;
    assert_eq!(status(control, first.id).await, Draining);
    control.advance_deployments().await;
    let stop = only(sent(&mut agent));
    let Some(Kind::StopDeployment(stop_deployment)) = &stop.kind else { panic!("{stop:?}") };
    assert_eq!(stop_deployment.deployment_id, first.id.to_string());
    assert_eq!(stop_deployment.grace_period, Some(prost_types::Duration { seconds: 20, nanos: 0 }));

    control.deployment_result(setup.server.id, succeeded(&stop, None)).await;

    assert_eq!(status(control, first.id).await, Superseded);
    assert_eq!(status(control, second.id).await, Live);
    control.advance_deployments().await;
    assert!(sent(&mut agent).is_empty());
}

#[tokio::test]
async fn failures_end_the_deployment_with_a_reason_and_message() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let mut agent = connect(control, setup.server.id);

    let pull = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    control.advance_deployments().await;
    let prepare = only(sent(&mut agent));
    control.deployment_result(setup.server.id, failed(&prepare, pb::FailureReason::BuildFailed, "cannot pull: not found")).await;

    let check = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    control.advance_deployments().await;
    let prepare = only(sent(&mut agent));
    control.deployment_result(setup.server.id, succeeded(&prepare, Some("web@sha256:02"))).await;
    let start = only(sent(&mut agent));
    let problem = "GET /healthz failed 4 times after the start period, last with: status 500";
    control.deployment_result(setup.server.id, failed(&start, pb::FailureReason::HealthCheckFailed, problem)).await;

    let pull = control.store().get_deployment(pull.id).await.unwrap();
    assert_eq!((pull.status, pull.reason), (Failed, Some(EndReason::BuildFailed)));
    let events = control.list_deployment_events(&setup.admin, pull.id).await.unwrap();
    assert_eq!(events.last().unwrap().message.as_deref(), Some("cannot pull: not found"));
    let check = control.store().get_deployment(check.id).await.unwrap();
    assert_eq!((check.status, check.reason), (Failed, Some(EndReason::HealthCheckFailed)));
    let path: Vec<_> =
        control.list_deployment_events(&setup.admin, check.id).await.unwrap().iter().map(|event| event.to).collect();
    assert_eq!(path, [Queued, Preparing, Starting, Verifying, Failed]);
}

#[tokio::test]
async fn only_the_deployments_own_agent_reports_on_it() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let other = control.create_server(&setup.admin, NewServer { name: "web-2".to_string() }).await.unwrap();
    let mut agent = connect(control, setup.server.id);
    let deployment = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    control.advance_deployments().await;
    let prepare = only(sent(&mut agent));

    control.deployment_result(other.id, failed(&prepare, pb::FailureReason::BuildFailed, "spoofed")).await;

    assert_eq!(status(control, deployment.id).await, Preparing);
}

#[tokio::test]
async fn one_deployment_of_a_service_runs_at_a_time() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let mut agent = connect(control, setup.server.id);
    let first = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    control.advance_deployments().await;
    let prepare = only(sent(&mut agent));

    let second = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    let third = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    control.advance_deployments().await;

    assert!(sent(&mut agent).is_empty());
    assert_eq!(status(control, second.id).await, Canceled);
    assert_eq!(status(control, third.id).await, Queued);
    control.deployment_result(setup.server.id, failed(&prepare, pb::FailureReason::BuildFailed, "no")).await;
    control.advance_deployments().await;
    assert_eq!(only(sent(&mut agent)).command_id, format!("{}/prepare", third.id));
    assert_eq!(status(control, first.id).await, Failed);
}

#[tokio::test]
async fn commands_are_sent_again_when_the_agent_reconnects() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let deployment = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    // Not connected: nothing to send to yet, and not lost so soon.
    control.advance_deployments().await;
    let mut agent = connect(control, setup.server.id);
    control.advance_deployments().await;
    assert_eq!(only(sent(&mut agent)).command_id, format!("{}/prepare", deployment.id));

    control.agent_connected(setup.server.id).await;
    control.advance_deployments().await;

    assert_eq!(only(sent(&mut agent)).command_id, format!("{}/prepare", deployment.id));
}

#[tokio::test]
async fn a_lost_agent_or_a_slow_phase_fails_the_deployment() {
    let timings = DeploymentTimings { agent_lost_after: Duration::ZERO, ..Default::default() };
    let setup = setup(timings).await;
    let control = &setup.control;
    let lost = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    control.advance_deployments().await;

    let lost = control.store().get_deployment(lost.id).await.unwrap();
    assert_eq!((lost.status, lost.reason), (Failed, Some(EndReason::AgentLost)));

    let timings = DeploymentTimings { prepare_timeout: Duration::ZERO, ..Default::default() };
    let setup = self::setup(timings).await;
    let control = &setup.control;
    let _agent = connect(control, setup.server.id);
    let slow = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    control.advance_deployments().await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    control.advance_deployments().await;

    let slow = control.store().get_deployment(slow.id).await.unwrap();
    assert_eq!((slow.status, slow.reason), (Failed, Some(EndReason::Timeout)));
    let events = control.list_deployment_events(&setup.admin, slow.id).await.unwrap();
    assert_eq!(events.last().unwrap().message.as_deref(), Some("it was preparing for longer than 0s"));
}

#[tokio::test]
async fn canceling_stops_what_the_deployment_started() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let mut agent = connect(control, setup.server.id);
    let live = deploy(&setup, &mut agent).await;
    control.advance_deployments().await;
    let deployment = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    control.advance_deployments().await;
    let prepare = only(sent(&mut agent));
    control.deployment_result(setup.server.id, succeeded(&prepare, Some("web@sha256:02"))).await;
    let start = only(sent(&mut agent));

    let canceled = control.cancel_deployment(&setup.admin, deployment.id).await.unwrap();
    let stop = only(sent(&mut agent));
    // The start finishes anyway; what it started is stopped again.
    control.deployment_result(setup.server.id, succeeded(&start, None)).await;
    let stop_again = only(sent(&mut agent));
    let cancel_live = control.cancel_deployment(&setup.admin, live.id).await;

    assert_eq!((canceled.status, canceled.reason), (Canceled, Some(EndReason::CanceledByUser)));
    assert_eq!(stop.command_id, format!("{}/stop", deployment.id));
    assert_eq!(stop_again.command_id, format!("{}/cleanup", deployment.id));
    assert_eq!(status(control, live.id).await, Live);
    assert!(matches!(cancel_live, Err(ControlError::InvalidTransition(_))), "{cancel_live:?}");
}

#[tokio::test]
async fn checks_access_the_source_and_the_placement() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let viewer = user(control, "vic@example.com", false).await;
    grant(control, &viewer, MembershipScope::Environment(setup.environment.id), Role::Viewer).await;
    let outsider = user(control, "eve@example.com", false).await;
    let deployment = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();

    let by_viewer = control.create_deployment(&viewer, setup.service.id).await;
    let listed = control.list_deployments(&viewer, setup.service.id, None, 50).await.unwrap();
    let hidden = control.get_deployment(&outsider, deployment.id).await;
    let cancel_by_viewer = control.cancel_deployment(&viewer, deployment.id).await;

    let new = |name: &str| NewService {
        name: name.to_string(),
        slug: None,
        kind: ServiceKind::App,
        config: Default::default(),
        server_id: None,
    };
    let unplaced = control.create_service(&setup.admin, setup.environment.id, new("Worker")).await.unwrap();
    let no_source = control.create_deployment(&setup.admin, unplaced.id).await;
    let image = SourceSpec::Image(ImageSource { image: "worker:1".to_string() });
    control.create_source(&setup.admin, unplaced.id, image).await.unwrap();
    let no_server = control.create_deployment(&setup.admin, unplaced.id).await;
    let embedded = control.embedded_server("box").await.unwrap();
    let on_embedded = control.create_deployment(&setup.admin, unplaced.id).await.unwrap();

    assert!(matches!(by_viewer, Err(ControlError::Forbidden)), "{by_viewer:?}");
    assert_eq!(listed.items.iter().map(|deployment| deployment.id).collect::<Vec<_>>(), [deployment.id]);
    assert!(matches!(hidden, Err(ControlError::NotFound { entity: "deployment", .. })), "{hidden:?}");
    assert!(matches!(cancel_by_viewer, Err(ControlError::Forbidden)), "{cancel_by_viewer:?}");
    assert!(matches!(no_source, Err(ControlError::Invalid { field: "source", .. })), "{no_source:?}");
    assert!(matches!(no_server, Err(ControlError::Invalid { field: "server_id", .. })), "{no_server:?}");
    assert_eq!(on_embedded.server_id, embedded.id);
}
