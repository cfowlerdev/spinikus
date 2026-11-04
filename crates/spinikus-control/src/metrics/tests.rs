//! Metrics pushed by a fake agent.

use spinikus_core::{
    membership::MembershipScope,
    notification::ChannelSettings,
    server::{InstanceHealth, InstanceStatus, ObservedInstance},
};

use super::*;
use crate::{
    DeploymentTimings,
    deployments::tests::{Setup, connect, deploy, setup},
    notifications::NewChannel,
    test_support::{grant, user},
};

/// The start of the current minute, less `minutes`.
fn minutes_ago(minutes: i64) -> Timestamp {
    let minute = SignedDuration::from_mins(1);
    metrics::period_end(Timestamp::now(), minute) - minute * (minutes as i32 + 1)
}

fn cpu(at: Timestamp, cores: f64) -> MetricPoint {
    MetricPoint { at, cpu_cores: Some(cores), memory_bytes: Some(1000), ..MetricPoint::default() }
}

fn disk(at: Timestamp, percent: u64) -> MetricPoint {
    MetricPoint { at, disk_used_bytes: Some(percent), disk_total_bytes: Some(100), ..MetricPoint::default() }
}

/// The service live on its server, in a container the agent reported.
async fn live(setup: &Setup) -> tokio::sync::mpsc::Receiver<spinikus_proto::agent::v1::ControlMessage> {
    let mut agent = connect(&setup.control, setup.server.id);
    let deployment = deploy(setup, &mut agent).await;
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
    agent
}

#[tokio::test]
async fn stores_the_servers_containers_and_itself() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let _agent = live(&setup).await;
    let viewer = user(control, "viewer@example.com", false).await;
    grant(control, &viewer, MembershipScope::Environment(setup.environment.id), Role::Viewer).await;
    let stranger = user(control, "stranger@example.com", false).await;

    let points = vec![
        ("web-container".to_string(), MetricPoint { disk_used_bytes: Some(1), ..cpu(minutes_ago(2), 0.5) }),
        ("web-container".to_string(), cpu(minutes_ago(1), 1.5)),
        ("someone-elses".to_string(), cpu(minutes_ago(1), 8.0)),
        ("web-container".to_string(), cpu(Timestamp::now() + SignedDuration::from_hours(1), 9.0)),
        ("web-container".to_string(), cpu(minutes_ago(25 * 60), 9.0)),
        (String::new(), MetricPoint { cpu_cores: Some(3.0), ..disk(minutes_ago(1), 40) }),
    ];
    control.record_stats(setup.server.id, points).await.unwrap();

    let service = control.service_metrics(&viewer, setup.service.id, MetricRange::Hour).await.unwrap();
    let summary: Vec<_> = service.points.iter().map(|point| (point.at, point.cpu_cores, point.disk_used_bytes)).collect();
    assert_eq!(summary, [(minutes_ago(2), Some(0.5), None), (minutes_ago(1), Some(1.5), None)]);
    assert_eq!(service.step_seconds, 60);
    let server = control.server_metrics(&setup.admin, setup.server.id, MetricRange::Hour).await.unwrap();
    assert_eq!(server.points.len(), 1);
    assert_eq!((server.points[0].cpu_cores, server.points[0].disk_percent()), (Some(3.0), Some(40.0)));

    let by_stranger = control.service_metrics(&stranger, setup.service.id, MetricRange::Hour).await;
    assert!(matches!(by_stranger, Err(ControlError::NotFound { .. })), "{by_stranger:?}");
    let server_by_viewer = control.server_metrics(&viewer, setup.server.id, MetricRange::Hour).await;
    assert!(matches!(server_by_viewer, Err(ControlError::Forbidden)), "{server_by_viewer:?}");

    // Hours are rolled up once they are complete; a week's graph reads them.
    control.maintain_metrics_at(Timestamp::now() + SignedDuration::from_hours(2)).await.unwrap();
    let week = control.service_metrics(&viewer, setup.service.id, MetricRange::Week).await.unwrap();
    // Both minutes in one hour, or each in its own if an hour began between them.
    let cores: Vec<_> = week.points.iter().map(|point| point.cpu_cores).collect();
    assert!(cores == [Some(1.0)] || cores == [Some(0.5), Some(1.5)], "{cores:?}");
}

#[tokio::test]
async fn a_server_low_on_disk_is_notified_once_until_it_has_room_again() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let channel = NewChannel {
        name: "Ops".to_string(),
        settings: ChannelSettings::Webhook { url: "http://127.0.0.1:9/".to_string() },
        events: vec![NotificationEvent::ServerDiskLow],
        enabled: true,
        secret: None,
    };
    let channel = control.create_notification_channel(&setup.admin, channel).await.unwrap();
    let push = |percent, minutes| {
        let control = control.clone();
        let server = setup.server.id;
        async move { control.record_stats(server, vec![(String::new(), disk(minutes_ago(minutes), percent))]).await.unwrap() }
    };

    push(89, 6).await;
    let none = control.servers_low_on_disk();
    push(93, 5).await;
    let low = control.servers_low_on_disk();
    push(88, 4).await;
    push(84, 3).await;
    let room = control.servers_low_on_disk();
    push(91, 2).await;

    assert!(none.is_empty());
    assert_eq!(low, HashSet::from([setup.server.id]));
    assert!(room.is_empty());
    let sent = control.list_notifications(&setup.admin, channel.id, None, 10).await.unwrap().items;
    assert_eq!(sent.len(), 2, "at 93%, then at 91% after it had room");
    assert_eq!(sent[1].title, "Server web-1 is low on disk");
    assert!(sent[1].text.starts_with("Its disk is 93% full (93 B of 100 B)."), "{}", sent[1].text);
    assert_eq!(sent[1].details["server_id"], setup.server.id.to_string());
}
