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
    NewDomain, NewEnvironment, NewProject, NewServer, NewService,
    test_support::{control, grant, user},
};

pub(crate) struct Setup {
    _dir: tempfile::TempDir,
    pub(crate) control: Control,
    pub(crate) admin: Principal,
    environment: Environment,
    pub(crate) service: Service,
    pub(crate) server: Server,
}

/// A server with a fake agent, and an app service placed on it with an image source.
pub(crate) async fn setup(timings: DeploymentTimings) -> Setup {
    setup_with(timings, |control| control).await
}

/// Like [`setup`], with the control plane changed by `change` first.
pub(crate) async fn setup_with(timings: DeploymentTimings, change: impl FnOnce(Control) -> Control) -> Setup {
    let (dir, control) = control().await;
    let control = change(control.with_deployment_timings(timings));
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
pub(crate) fn connect(control: &Control, server: ServerId) -> mpsc::Receiver<pb::ControlMessage> {
    let (sender, receiver) = mpsc::channel(64);
    let _ = control.agents().register(server, sender);
    receiver
}

/// The commands and route tables sent so far. Cleanups are left out; tests about them
/// read them with [`removals`].
fn received(agent: &mut mpsc::Receiver<pb::ControlMessage>) -> (Vec<pb::Command>, Vec<pb::SetRoutes>) {
    let (mut commands, mut routes) = (Vec::new(), Vec::new());
    while let Ok(message) = agent.try_recv() {
        match message.message {
            Some(control_message::Message::Command(command)) if matches!(command.kind, Some(Kind::RemoveDeployment(_))) => {}
            Some(control_message::Message::Command(command)) => commands.push(command),
            Some(control_message::Message::SetRoutes(set)) => routes.push(set),
            _ => {}
        }
    }
    (commands, routes)
}

/// The commands sent so far.
fn sent(agent: &mut mpsc::Receiver<pb::ControlMessage>) -> Vec<pb::Command> {
    received(agent).0
}

/// The hostnames and target deployments of a route table.
fn table(set: &pb::SetRoutes) -> Vec<(&str, String)> {
    set.routes
        .iter()
        .map(|route| {
            let action = match &route.action {
                Some(pb::route::Action::Proxy(target)) => format!("{}:{}", target.deployment_id, target.port),
                Some(pb::route::Action::RedirectTo(to)) => format!("-> {to}"),
                None => String::new(),
            };
            (route.hostname.as_str(), action)
        })
        .collect()
}

fn only(mut commands: Vec<pb::Command>) -> pb::Command {
    assert_eq!(commands.len(), 1, "{commands:?}");
    commands.pop().unwrap()
}

fn succeeded(command: &pb::Command, prepared: Option<&str>) -> pb::CommandResult {
    let prepared = prepared.map(|image| pb::PreparedImage { image: image.to_string(), ..Default::default() });
    pb::CommandResult {
        command_id: command.command_id.clone(),
        outcome: Some(Outcome::Succeeded(pb::CommandSucceeded { prepared, backup: None })),
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
pub(crate) async fn deploy(setup: &Setup, agent: &mut mpsc::Receiver<pb::ControlMessage>) -> Deployment {
    deploy_image(setup, agent, "ghcr.io/acme/web@sha256:01").await
}

/// Like [`deploy`], with the agent reporting `image` as prepared.
async fn deploy_image(setup: &Setup, agent: &mut mpsc::Receiver<pb::ControlMessage>, image: &str) -> Deployment {
    let control = &setup.control;
    let deployment = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    control.advance_deployments().await;
    let prepare = only(sent(agent));
    control.deployment_result(setup.server.id, succeeded(&prepare, Some(image))).await;
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
    let network = start_deployment.network.as_ref().unwrap();
    assert_eq!(
        (network.name.clone(), network.aliases.clone()),
        (format!("spinikus-{}", setup.environment.id), vec!["web".to_string()])
    );
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
    // The routes move to the new deployment first; the old one drains once they are applied.
    let (commands, routes) = received(&mut agent);
    assert!(commands.is_empty(), "{commands:?}");
    let routes = routes.last().unwrap();
    assert_eq!(table(routes), [("web-production-shop.localhost", format!("{}:3000", second.id))]);
    control.advance_deployments().await;
    assert!(sent(&mut agent).is_empty());
    control.routes_applied(setup.server.id, routes.revision).await;
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

    control.agent_connected(setup.server.id, 0, 0).await;
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

#[tokio::test]
async fn routes_follow_live_deployments_domains_and_the_base_domain() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let mut agent = connect(control, setup.server.id);
    let other = control.create_server(&setup.admin, NewServer { name: "web-2".to_string() }).await.unwrap();
    let mut other_agent = connect(control, other.id);
    let apex = NewDomain { hostname: "shop.example.com".to_string(), redirect_to: None };
    let apex = control.create_domain(&setup.admin, setup.service.id, apex).await.unwrap();
    let www = NewDomain { hostname: "www.shop.example.com".to_string(), redirect_to: Some(apex.id) };
    control.create_domain(&setup.admin, setup.service.id, www).await.unwrap();

    control.advance_deployments().await;
    let (_, before) = received(&mut agent);
    let live = deploy(&setup, &mut agent).await;
    control.advance_deployments().await;
    let (_, after) = received(&mut agent);
    let after = after.last().unwrap();
    control.routes_applied(setup.server.id, after.revision).await;
    control.advance_deployments().await;
    let (_, unchanged) = received(&mut agent);
    control.set_setting(&setup.admin, crate::settings::BASE_DOMAIN, 0, json!("apps.example.com")).await.unwrap();
    control.advance_deployments().await;
    let (_, moved) = received(&mut agent);
    let (_, elsewhere) = received(&mut other_agent);

    assert_eq!(before.iter().map(|set| set.routes.len()).collect::<Vec<_>>(), [0]);
    let target = format!("{}:3000", live.id);
    assert_eq!(
        table(after),
        [
            ("shop.example.com", target.clone()),
            ("web-production-shop.localhost", target.clone()),
            ("www.shop.example.com", "-> shop.example.com".to_string()),
        ]
    );
    assert!(unchanged.is_empty(), "{unchanged:?}");
    assert_eq!(table(&moved[0])[1], ("web-production-shop.apps.example.com", target));
    assert!(moved[0].revision > after.revision);
    assert_eq!(elsewhere.iter().map(|set| set.routes.len()).collect::<Vec<_>>(), [0]);
}

#[tokio::test]
async fn an_agent_with_the_current_routes_needs_none() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let mut agent = connect(control, setup.server.id);
    let first = deploy(&setup, &mut agent).await;
    control.advance_deployments().await;
    let (_, routes) = received(&mut agent);
    let revision = routes.last().unwrap().revision;

    // Reconnects with the routes it has: nothing to send, and they count as applied.
    control.agent_connected(setup.server.id, revision, 0).await;
    control.advance_deployments().await;
    let (_, again) = received(&mut agent);
    let second = deploy(&setup, &mut agent).await;
    control.advance_deployments().await;
    let (_, moved) = received(&mut agent);
    // Reconnects with older routes: they are sent again.
    control.agent_connected(setup.server.id, revision, 0).await;
    control.advance_deployments().await;
    let (commands, resent) = received(&mut agent);

    assert!(again.is_empty(), "{again:?}");
    assert_eq!(table(&moved[0])[0].1, format!("{}:3000", second.id));
    assert_eq!(resent.iter().map(|set| set.revision).collect::<Vec<_>>(), [moved[0].revision]);
    assert!(commands.is_empty(), "the old deployment waits for the routes: {commands:?}");
    assert_eq!(status(control, first.id).await, Draining);
}

/// Switches the service to an SSH Git source, returning the deploy key it got.
async fn use_git_over_ssh(setup: &Setup) -> spinikus_core::source::DeployKey {
    let control = &setup.control;
    let source = control.get_source(&setup.admin, setup.service.id).await.unwrap();
    let git = SourceSpec::Git(spinikus_core::source::GitSource {
        repo_url: "git@github.com:acme/web.git".to_string(),
        branch: "main".to_string(),
        root_directory: Some("apps/web".to_string()),
    });
    control.replace_source(&setup.admin, setup.service.id, source.revision, git).await.unwrap();
    control.get_deploy_key(&setup.admin, setup.service.id).await.unwrap()
}

fn prepared_with_file(command: &pb::Command, app_file: &str) -> pb::CommandResult {
    let prepared =
        pb::PreparedImage { image: "sha256:0123".to_string(), commit_sha: "abc123".to_string(), app_file: app_file.to_string() };
    pb::CommandResult {
        command_id: command.command_id.clone(),
        outcome: Some(Outcome::Succeeded(pb::CommandSucceeded { prepared: Some(prepared), backup: None })),
    }
}

#[tokio::test]
async fn git_over_ssh_gets_a_deploy_key_sent_with_the_build() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let viewer = user(control, "vic@example.com", false).await;
    grant(control, &viewer, MembershipScope::Environment(setup.environment.id), Role::Viewer).await;
    let mut agent = connect(control, setup.server.id);

    let none_for_images = control.get_deploy_key(&setup.admin, setup.service.id).await;
    let key = use_git_over_ssh(&setup).await;
    let seen_by_viewer = control.get_deploy_key(&viewer, setup.service.id).await.unwrap();
    let replaced_by_viewer = control.create_deploy_key(&viewer, setup.service.id).await;
    let deployment = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    let replaced = control.create_deploy_key(&setup.admin, setup.service.id).await.unwrap();
    control.advance_deployments().await;
    let prepare = only(sent(&mut agent));

    assert!(matches!(none_for_images, Err(ControlError::NotFound { entity: "deploy key", .. })), "{none_for_images:?}");
    assert!(key.public_key.starts_with("ssh-ed25519 AAAA") && key.public_key.ends_with(" spinikus"), "{}", key.public_key);
    assert!(key.fingerprint.starts_with("SHA256:"), "{}", key.fingerprint);
    assert_eq!(seen_by_viewer, key);
    assert!(matches!(replaced_by_viewer, Err(ControlError::Forbidden)), "{replaced_by_viewer:?}");
    assert_ne!(replaced.public_key, key.public_key);
    let Some(Kind::PrepareDeployment(prepare)) = prepare.kind else { panic!() };
    let Some(pb::prepare_deployment::Source::Git(git)) = prepare.source else { panic!() };
    assert_eq!((git.repo_url.as_str(), git.root_directory.as_str()), ("git@github.com:acme/web.git", "apps/web"));
    // The key at the time of the build, which can be newer than the deployment.
    let parsed = ssh_key::PrivateKey::from_openssh(&git.ssh_private_key).unwrap();
    assert_eq!(parsed.public_key().to_openssh().unwrap(), replaced.public_key);
    assert_eq!(status(control, deployment.id).await, Preparing);
}

#[tokio::test]
async fn https_repositories_need_no_deploy_key() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let source = control.get_source(&setup.admin, setup.service.id).await.unwrap();
    let git = SourceSpec::Git(spinikus_core::source::GitSource {
        repo_url: "https://github.com/acme/web.git".to_string(),
        branch: "main".to_string(),
        root_directory: None,
    });

    control.replace_source(&setup.admin, setup.service.id, source.revision, git).await.unwrap();

    assert!(control.get_deploy_key(&setup.admin, setup.service.id).await.is_err());
}

#[tokio::test]
async fn the_app_file_settings_go_into_the_snapshot() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    use_git_over_ssh(&setup).await;
    let mut agent = connect(control, setup.server.id);
    let deployment = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    control.advance_deployments().await;
    let prepare = only(sent(&mut agent));

    let app_file = r#"{ /* from the repo */ "runtime": { "port": 8080, "healthcheck": { "path": "/up" } } }"#;
    control.deployment_result(setup.server.id, prepared_with_file(&prepare, app_file)).await;
    let start = only(sent(&mut agent));
    let stored = control.store().get_deployment(deployment.id).await.unwrap();

    let Some(Kind::StartDeployment(start)) = start.kind else { panic!() };
    assert_eq!((start.image.as_str(), start.port), ("sha256:0123", 8080));
    assert_eq!(start.health_check.unwrap().path, "/up");
    assert_eq!(stored.commit_sha.as_deref(), Some("abc123"));
    assert_eq!(stored.snapshot.from_file, ["runtime.healthcheck", "runtime.port"]);
    assert_eq!(stored.snapshot.config["runtime"]["shutdown_timeout"], "20s");
}

#[tokio::test]
async fn an_app_file_that_cannot_apply_fails_the_build() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    use_git_over_ssh(&setup).await;
    let mut agent = connect(control, setup.server.id);
    let deployment = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    control.advance_deployments().await;
    let prepare = only(sent(&mut agent));

    control.deployment_result(setup.server.id, prepared_with_file(&prepare, r#"{ "runtime": { "port": 0 } }"#)).await;

    let failed = control.store().get_deployment(deployment.id).await.unwrap();
    assert_eq!((failed.status, failed.reason), (Failed, Some(EndReason::BuildFailed)));
    let events = control.list_deployment_events(&setup.admin, deployment.id).await.unwrap();
    let message = events.last().unwrap().message.clone().unwrap();
    assert!(message.contains("spinikus.json: runtime: port must be between 1 and 65535"), "{message}");
}

#[tokio::test]
async fn keeps_the_logs_of_the_deployments_own_server() {
    let logs = tempfile::tempdir().unwrap();
    let dir = logs.path().to_path_buf();
    let setup = setup_with(DeploymentTimings::default(), |control| control.with_log_dir(dir)).await;
    let control = &setup.control;
    let other = control.create_server(&setup.admin, NewServer { name: "web-2".to_string() }).await.unwrap();
    let deployment = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    let command_id = format!("{}/prepare", deployment.id);
    let line = |text: &str| pb::LogLine {
        at: Some(prost_types::Timestamp { seconds: 1_793_443_983, nanos: 0 }),
        stream: pb::LogStream::Stdout.into(),
        text: text.to_string(),
    };
    let progress = |phase: &str, lines: Vec<pb::LogLine>| pb::CommandProgress {
        command_id: command_id.clone(),
        phase: phase.to_string(),
        log_lines: lines,
    };

    let empty = control.deployment_logs(&setup.admin, deployment.id).await.unwrap();
    control.deployment_progress(setup.server.id, progress("cloning", Vec::new())).await;
    control.deployment_progress(setup.server.id, progress("cloning", vec![line("Cloning into 'repo'...")])).await;
    control.deployment_progress(other.id, progress("cloning", vec![line("not from this server")])).await;
    control.deployment_progress(setup.server.id, progress("building", vec![line("#1 FROM alpine"), line("done\n")])).await;
    let logs = control.deployment_logs(&setup.admin, deployment.id).await.unwrap();

    assert_eq!(empty, "");
    let lines: Vec<_> = logs.lines().map(|line| line.split_once(' ').unwrap().1).collect();
    assert_eq!(lines, ["==> cloning", "Cloning into 'repo'...", "#1 FROM alpine", "done"]);
    assert!(logs.lines().nth(1).unwrap().starts_with("2026-10-31T"), "{logs}");
}

/// Finishes draining the previous live deployment: the agent applies the new routes, then
/// stops it.
async fn drain(setup: &Setup, agent: &mut mpsc::Receiver<pb::ControlMessage>) {
    let control = &setup.control;
    control.advance_deployments().await;
    let (_, routes) = received(agent);
    control.routes_applied(setup.server.id, routes.last().unwrap().revision).await;
    control.advance_deployments().await;
    let stop = only(sent(agent));
    assert!(matches!(stop.kind, Some(Kind::StopDeployment(_))), "{stop:?}");
    control.deployment_result(setup.server.id, succeeded(&stop, None)).await;
}

#[tokio::test]
async fn rolls_back_to_the_last_release_without_building() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let mut agent = connect(control, setup.server.id);
    let first = deploy_image(&setup, &mut agent, "web@sha256:01").await;
    let fields = VariableFields { name: "ADDED".parse().unwrap(), availability: VariableAvailability::Runtime, secret: false };
    control.store().create_variable(VariableScope::Service(setup.service.id), fields, Secret::new("later")).await.unwrap();
    let second = deploy_image(&setup, &mut agent, "web@sha256:02").await;
    drain(&setup, &mut agent).await;

    let live = control.rollback_service(&setup.admin, setup.service.id, Some(second.id)).await;
    let rollback = control.rollback_service(&setup.admin, setup.service.id, None).await.unwrap();
    let in_progress = control.rollback_service(&setup.admin, setup.service.id, Some(rollback.id)).await;
    let other = control.rollback_service(&setup.admin, setup.service.id, Some(DeploymentId::new())).await;
    control.advance_deployments().await;
    let start = only(sent(&mut agent));
    control.deployment_result(setup.server.id, succeeded(&start, None)).await;

    assert!(matches!(&live, Err(ControlError::Invalid { field: "deployment_id", problem }) if problem.contains("live already")));
    assert!(matches!(&in_progress, Err(ControlError::Invalid { field: "deployment_id", .. })), "{in_progress:?}");
    assert!(matches!(other, Err(ControlError::NotFound { entity: "deployment", .. })), "{other:?}");
    assert_eq!((rollback.trigger, rollback.rollback_of), (DeploymentTrigger::Rollback, Some(first.id)));
    assert_eq!((rollback.image.as_deref(), &rollback.snapshot), (Some("web@sha256:01"), &first.snapshot));
    let Some(Kind::StartDeployment(start)) = &start.kind else { panic!("{start:?}") };
    assert_eq!(start.image, "web@sha256:01");
    // The variables of the deployment rolled back to, not the service's current ones.
    assert!(start.variables.is_empty(), "{:?}", start.variables);
    let path: Vec<_> =
        control.list_deployment_events(&setup.admin, rollback.id).await.unwrap().iter().map(|event| event.to).collect();
    assert_eq!(path, [Queued, Starting, Verifying, Live]);
    assert_eq!(status(control, second.id).await, Draining);
}

/// The `RemoveDeployment` commands sent so far, as (deployment, images), sorted.
fn removals(agent: &mut mpsc::Receiver<pb::ControlMessage>) -> Vec<(String, Vec<String>)> {
    let mut removals = Vec::new();
    while let Ok(message) = agent.try_recv() {
        if let Some(control_message::Message::Command(pb::Command { kind: Some(Kind::RemoveDeployment(remove)), .. })) =
            message.message
        {
            removals.push((remove.deployment_id, remove.images));
        }
    }
    removals.sort();
    removals
}

/// Timings that send unanswered commands again on every pass, so a pass shows what is
/// still to do.
fn resending() -> DeploymentTimings {
    DeploymentTimings { resend_after: Duration::ZERO, ..Default::default() }
}

#[tokio::test]
async fn cleans_up_what_is_no_longer_kept_but_not_images_still_needed() {
    let setup = setup(resending()).await;
    let control = &setup.control;
    control.set_setting(&setup.admin, crate::settings::RELEASES_KEPT, 0, json!(1)).await.unwrap();
    let mut agent = connect(control, setup.server.id);
    let first = deploy_image(&setup, &mut agent, "web@sha256:01").await;
    let second = deploy_image(&setup, &mut agent, "web@sha256:02").await;
    drain(&setup, &mut agent).await;
    // The same image as the second: it stays when the second is cleaned up.
    let third = deploy_image(&setup, &mut agent, "web@sha256:02").await;
    drain(&setup, &mut agent).await;
    let fourth = deploy_image(&setup, &mut agent, "web@sha256:04").await;
    drain(&setup, &mut agent).await;

    // Kept: the fourth (live) and the third (the one release kept).
    control.advance_deployments().await;
    let removed = removals(&mut agent);
    let remove = pb::Command { command_id: format!("{}/remove", first.id), kind: None };
    control.deployment_result(setup.server.id, succeeded(&remove, None)).await;
    control.advance_deployments().await;
    let left = removals(&mut agent);

    let mut expected = vec![(first.id.to_string(), vec!["web@sha256:01".to_string()]), (second.id.to_string(), Vec::new())];
    expected.sort();
    assert_eq!(removed, expected);
    assert_eq!(left, [(second.id.to_string(), Vec::new())]);

    let cleaned = control.store().get_deployment(first.id).await.unwrap();
    assert!(cleaned.cleaned_at.is_some());
    assert!(control.store().get_deployment(second.id).await.unwrap().cleaned_at.is_none());
    let gone = control.rollback_service(&setup.admin, setup.service.id, Some(first.id)).await;
    assert!(matches!(&gone, Err(ControlError::Invalid { problem, .. }) if problem.contains("no longer kept")), "{gone:?}");
    let default = control.rollback_service(&setup.admin, setup.service.id, None).await.unwrap();
    assert_eq!(default.rollback_of, Some(third.id));
    assert_eq!(status(control, fourth.id).await, Live);
}

#[tokio::test]
async fn keeps_failures_until_a_newer_deployment_goes_live() {
    let setup = setup(resending()).await;
    let control = &setup.control;
    let mut agent = connect(control, setup.server.id);
    let first = deploy_image(&setup, &mut agent, "web@sha256:01").await;
    let broken = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    control.advance_deployments().await;
    let prepare = only(sent(&mut agent));
    control.deployment_result(setup.server.id, succeeded(&prepare, Some("web@sha256:bad"))).await;
    let start = only(sent(&mut agent));
    control.deployment_result(setup.server.id, failed(&start, pb::FailureReason::HealthCheckFailed, "status 500")).await;

    control.advance_deployments().await;
    let while_broken = removals(&mut agent);
    let fixed = deploy_image(&setup, &mut agent, "web@sha256:03").await;
    drain(&setup, &mut agent).await;
    control.advance_deployments().await;
    let after_the_fix = removals(&mut agent);

    assert!(while_broken.is_empty(), "{while_broken:?}");
    assert_eq!(after_the_fix, [(broken.id.to_string(), vec!["web@sha256:bad".to_string()])]);
    assert_eq!((status(control, first.id).await, status(control, fixed.id).await), (Superseded, Live));
}

#[tokio::test]
async fn a_failure_ends_the_log_with_why() {
    let logs = tempfile::tempdir().unwrap();
    let dir = logs.path().to_path_buf();
    let setup = setup_with(DeploymentTimings::default(), |control| control.with_log_dir(dir)).await;
    let control = &setup.control;
    let mut agent = connect(control, setup.server.id);

    let deployment = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    control.advance_deployments().await;
    let prepare = only(sent(&mut agent));
    control.deployment_result(setup.server.id, failed(&prepare, pb::FailureReason::BuildFailed, "cannot pull: not found")).await;
    let canceled = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    control.cancel_deployment(&setup.admin, canceled.id).await.unwrap();

    let log = control.deployment_logs(&setup.admin, deployment.id).await.unwrap();
    assert!(log.trim_end().ends_with("==> failed (build_failed): cannot pull: not found"), "{log}");
    let log = control.deployment_logs(&setup.admin, canceled.id).await.unwrap();
    assert!(log.trim_end().ends_with("==> canceled"), "{log}");
}

#[tokio::test]
async fn on_a_revoked_server_cleaning_up_is_only_recorded() {
    let setup = setup(resending()).await;
    let control = &setup.control;
    control.set_setting(&setup.admin, crate::settings::RELEASES_KEPT, 0, json!(0)).await.unwrap();
    let mut agent = connect(control, setup.server.id);
    let first = deploy(&setup, &mut agent).await;
    deploy(&setup, &mut agent).await;
    drain(&setup, &mut agent).await;
    removals(&mut agent);

    control.revoke_server(&setup.admin, setup.server.id).await.unwrap();
    control.advance_deployments().await;

    assert!(removals(&mut agent).is_empty());
    assert!(control.store().get_deployment(first.id).await.unwrap().cleaned_at.is_some());
}

/// Everything a follower gets until the stream ends.
async fn follow_to_end(control: &Control, admin: &Principal, id: DeploymentId, offset: u64) -> Vec<stream::DeploymentUpdate> {
    use tokio_stream::StreamExt;
    let updates = control.follow_deployment(admin, id, offset).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), updates.collect()).await.expect("the stream ended")
}

fn progress(command: &pb::Command, phase: &str, lines: &[&str]) -> pb::CommandProgress {
    pb::CommandProgress {
        command_id: command.command_id.clone(),
        phase: phase.to_string(),
        log_lines: lines.iter().map(|text| pb::LogLine { text: text.to_string(), ..Default::default() }).collect(),
    }
}

#[tokio::test]
async fn followers_get_the_log_and_status_as_they_happen() {
    use stream::DeploymentUpdate::*;
    use tokio_stream::StreamExt;
    let logs = tempfile::tempdir().unwrap();
    let dir = logs.path().to_path_buf();
    let setup = setup_with(DeploymentTimings::default(), |control| control.with_log_dir(dir)).await;
    let control = &setup.control;
    let mut agent = connect(control, setup.server.id);
    let deployment = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    control.advance_deployments().await;
    let prepare = only(sent(&mut agent));
    control.deployment_progress(setup.server.id, progress(&prepare, "pulling", &[])).await;
    control.deployment_progress(setup.server.id, progress(&prepare, "pulling", &["Pulling web:1"])).await;

    let mut live = control.follow_deployment(&setup.admin, deployment.id, 0).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), live.next()).await.unwrap().unwrap();
    control.deployment_progress(setup.server.id, progress(&prepare, "pulling", &["Pulled web:1"])).await;
    control.deployment_result(setup.server.id, succeeded(&prepare, Some("web@sha256:01"))).await;
    let start = only(sent(&mut agent));
    control.deployment_result(setup.server.id, succeeded(&start, None)).await;
    let rest: Vec<_> = tokio::time::timeout(Duration::from_secs(5), live.collect::<Vec<_>>()).await.unwrap();
    let offset = rest.iter().find_map(|update| if let Log { offset, .. } = update { Some(*offset) } else { None }).unwrap();
    let resumed = follow_to_end(control, &setup.admin, deployment.id, offset).await;
    let mid_line = follow_to_end(control, &setup.admin, deployment.id, offset - 3).await;

    let lines = |updates: &[stream::DeploymentUpdate]| -> Vec<String> {
        updates
            .iter()
            .filter_map(|update| match update {
                Log { line, .. } => Some(line.split_once(' ').unwrap().1.to_string()),
                _ => None,
            })
            .collect()
    };
    let all: Vec<_> = [first.clone()].into_iter().chain(rest.clone()).collect();
    assert_eq!(lines(&all), ["==> pulling", "Pulling web:1", "Pulled web:1"]);
    let statuses: Vec<_> =
        all.iter().filter_map(|update| if let Status { status, .. } = update { Some(*status) } else { None }).collect();
    assert_eq!((statuses.first(), statuses.last()), (Some(&Preparing), Some(&Live)));
    assert!(matches!(first, Status { status: Preparing, .. }), "{first:?}");
    assert_eq!(all.last(), Some(&End(Live)));
    // Resuming after the first line starts at the second, and so does an offset inside it.
    assert_eq!(lines(&resumed), ["Pulling web:1", "Pulled web:1"]);
    assert_eq!(lines(&mid_line), lines(&resumed));
}

#[tokio::test]
async fn a_follower_hears_why_a_deployment_failed() {
    use stream::DeploymentUpdate::*;
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let mut agent = connect(control, setup.server.id);
    let deployment = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    control.advance_deployments().await;
    let prepare = only(sent(&mut agent));
    control.deployment_result(setup.server.id, failed(&prepare, pb::FailureReason::BuildFailed, "cannot pull")).await;

    let updates = follow_to_end(control, &setup.admin, deployment.id, 0).await;

    let failed = Status { status: Failed, reason: Some(EndReason::BuildFailed), message: Some("cannot pull".to_string()) };
    assert_eq!(updates, [failed, End(Failed)]);
}

/// Everything sent so far: commands other than removals, route tables, and removals as
/// (deployment, images), sorted.
#[derive(Default)]
struct Everything {
    commands: Vec<pb::Command>,
    routes: Vec<pb::SetRoutes>,
    removed: Vec<(String, Vec<String>)>,
}

fn everything(agent: &mut mpsc::Receiver<pb::ControlMessage>) -> Everything {
    let mut all = Everything::default();
    while let Ok(message) = agent.try_recv() {
        match message.message {
            Some(control_message::Message::Command(pb::Command { kind: Some(Kind::RemoveDeployment(remove)), .. })) => {
                all.removed.push((remove.deployment_id, remove.images));
            }
            Some(control_message::Message::Command(command)) => all.commands.push(command),
            Some(control_message::Message::SetRoutes(set)) => all.routes.push(set),
            _ => {}
        }
    }
    all.removed.sort();
    all
}

#[tokio::test]
async fn deleting_a_service_stops_and_removes_its_deployments() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let mut agent = connect(control, setup.server.id);
    let first = deploy_image(&setup, &mut agent, "web@sha256:01").await;
    let second = deploy_image(&setup, &mut agent, "web@sha256:02").await;
    drain(&setup, &mut agent).await;
    let building = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    control.advance_deployments().await;
    assert!(matches!(only(sent(&mut agent)).kind, Some(Kind::PrepareDeployment(_))));

    control.delete_service(&setup.admin, setup.service.id).await.unwrap();
    control.advance_deployments().await;

    // The build is canceled and removed with the old release; the live one drains first.
    let Everything { commands, routes, removed } = everything(&mut agent);
    assert!(commands.is_empty(), "nothing to stop until the routes are applied: {commands:?}");
    let routes = routes.last().unwrap();
    assert!(routes.routes.is_empty(), "{routes:?}");
    let mut expected = vec![(first.id.to_string(), vec!["web@sha256:01".to_string()]), (building.id.to_string(), Vec::new())];
    expected.sort();
    assert_eq!(removed, expected);
    let canceled = control.store().get_deployment(building.id).await.unwrap();
    assert_eq!((canceled.status, canceled.reason), (Canceled, Some(EndReason::CanceledByUser)));
    let events = control.store().list_deployment_events(building.id).await.unwrap();
    assert_eq!(events.last().unwrap().message.as_deref(), Some("its service was deleted"));
    assert_eq!(status(control, second.id).await, Draining);

    control.routes_applied(setup.server.id, routes.revision).await;
    control.advance_deployments().await;
    let stop = only(sent(&mut agent));
    let Some(Kind::StopDeployment(stopping)) = &stop.kind else { panic!("{stop:?}") };
    assert_eq!(stopping.deployment_id, second.id.to_string());
    control.deployment_result(setup.server.id, succeeded(&stop, None)).await;
    control.advance_deployments().await;
    let removed = everything(&mut agent).removed;
    assert_eq!(removed, [(second.id.to_string(), vec!["web@sha256:02".to_string()])]);

    for id in [first.id, second.id, building.id] {
        let remove = pb::Command { command_id: format!("{id}/remove"), kind: None };
        control.deployment_result(setup.server.id, succeeded(&remove, None)).await;
    }
    assert!(control.store().list_deleted_services_with_deployments().await.unwrap().is_empty());
    control.advance_deployments().await;
    assert!(everything(&mut agent).removed.is_empty(), "nothing left to remove");
}
