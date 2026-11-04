//! Crash loops noticed from what a fake agent reports.

use spinikus_core::{
    crash::CrashReason,
    deployment::Deployment,
    membership::MembershipScope,
    server::{InstanceHealth, InstanceStatus, ObservedInstance},
};
use tokio::sync::mpsc;

use super::*;
use crate::{
    DeploymentTimings,
    deployments::tests::{Setup, connect, deploy, setup},
    test_support::{grant, user},
};

/// Minutes after `base`.
fn at(base: Timestamp, minutes: i64) -> Timestamp {
    base + jiff::SignedDuration::from_mins(minutes)
}

async fn report(setup: &Setup, deployment: &Deployment, status: InstanceStatus, restart_count: u32) {
    let container = ObservedInstance {
        service_id: setup.service.id,
        deployment_id: deployment.id,
        container_id: "web-container".to_string(),
        status,
        health: InstanceHealth::None,
        restart_count,
        started_at: None,
    };
    setup.control.store().upsert_instance(setup.server.id, container).await.unwrap();
}

async fn live(setup: &Setup) -> (Deployment, mpsc::Receiver<spinikus_proto::agent::v1::ControlMessage>) {
    let mut agent = connect(&setup.control, setup.server.id);
    let deployment = deploy(setup, &mut agent).await;
    (deployment, agent)
}

async fn lasting(setup: &Setup) -> Option<CrashLoop> {
    setup.control.lasting_crash_loop(&setup.admin, setup.service.id).await.unwrap()
}

#[tokio::test]
async fn restarts_begin_a_crash_loop_that_ends_once_the_container_runs_again() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let (deployment, _agent) = live(&setup).await;
    let base = Timestamp::now();

    report(&setup, &deployment, InstanceStatus::Running, 0).await;
    control.watch_crashes_at(base).await.unwrap();
    for (minute, count) in [(1, 1), (2, 2)] {
        report(&setup, &deployment, InstanceStatus::Running, count).await;
        control.watch_crashes_at(at(base, minute)).await.unwrap();
    }
    let before = lasting(&setup).await;
    report(&setup, &deployment, InstanceStatus::Running, 3).await;
    control.watch_crashes_at(at(base, 3)).await.unwrap();
    let begun = lasting(&setup).await.expect("a crash loop");
    report(&setup, &deployment, InstanceStatus::Running, 5).await;
    control.watch_crashes_at(at(base, 4)).await.unwrap();
    let going_on = lasting(&setup).await.expect("still crashing");
    control.watch_crashes_at(at(base, 14)).await.unwrap();
    let recovered = control.list_crash_loops(&setup.admin, setup.service.id, None, 10).await.unwrap();

    assert_eq!(before, None, "two restarts are not a loop");
    assert_eq!((begun.reason, begun.restarts, begun.deployment_id), (CrashReason::Restarting, 3, deployment.id));
    assert_eq!((going_on.id, going_on.restarts), (begun.id, 5), "counted from before the loop began");
    assert_eq!(recovered.items.len(), 1);
    assert!(recovered.items[0].ended_at.is_some(), "{recovered:?}");
}

#[tokio::test]
async fn a_container_that_stays_down_is_crashing_until_its_service_is_stopped() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let (deployment, _agent) = live(&setup).await;
    let base = Timestamp::now();

    // Nothing reported for the live deployment counts as down.
    control.watch_crashes_at(base).await.unwrap();
    control.watch_crashes_at(at(base, 2)).await.unwrap();
    let missing = lasting(&setup).await.expect("a crash loop");
    control.store().set_service_stopped(setup.service.id, true).await.unwrap();
    control.watch_crashes_at(at(base, 3)).await.unwrap();

    assert_eq!((missing.reason, missing.deployment_id), (CrashReason::NotRunning, deployment.id));
    assert_eq!(lasting(&setup).await, None, "a stopped service is not crashing");
}

#[tokio::test]
async fn a_server_whose_agent_is_away_keeps_its_crash_loops() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let (deployment, _agent) = live(&setup).await;
    let base = Timestamp::now();
    report(&setup, &deployment, InstanceStatus::Exited, 0).await;
    control.watch_crashes_at(base).await.unwrap();
    control.watch_crashes_at(at(base, 2)).await.unwrap();
    let begun = lasting(&setup).await.expect("a crash loop");

    let (connection, _) = control.agents().register(setup.server.id, mpsc::channel(1).0);
    control.agents().unregister(setup.server.id, connection);
    report(&setup, &deployment, InstanceStatus::Running, 0).await;
    control.watch_crashes_at(at(base, 30)).await.unwrap();

    assert_eq!(lasting(&setup).await.map(|crash| crash.id), Some(begun.id));
}

#[tokio::test]
async fn viewers_see_which_services_crash() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let viewer = user(control, "viewer@example.com", false).await;
    let outsider = user(control, "outsider@example.com", false).await;
    grant(control, &viewer, MembershipScope::Environment(setup.environment.id), Role::Viewer).await;
    let (_, _agent) = live(&setup).await;
    let base = Timestamp::now();
    control.watch_crashes_at(base).await.unwrap();
    control.watch_crashes_at(at(base, 2)).await.unwrap();

    let crashing = control.crashing_services(&viewer, setup.environment.id).await.unwrap();
    let listed = control.list_crash_loops(&viewer, setup.service.id, None, 10).await.unwrap();
    let hidden = control.list_crash_loops(&outsider, setup.service.id, None, 10).await;

    assert_eq!(crashing, HashSet::from([setup.service.id]));
    assert_eq!(listed.items.len(), 1);
    assert!(matches!(hidden, Err(ControlError::NotFound { .. })), "{hidden:?}");
}
