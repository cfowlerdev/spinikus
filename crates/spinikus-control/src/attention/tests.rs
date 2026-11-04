//! What needs attention, and who sees it.

use jiff::{SignedDuration, Timestamp};
use serde_json::json;
use spinikus_core::{
    backup::{NewBackup, object_key},
    certificate::NewCertificate,
    database::Engine,
    deployment::{DeploymentStatus, EndReason},
    id::{BackupId, InstanceBackupId},
    instance_backup::NewInstanceBackup,
    membership::{MembershipScope, Role},
    notification::{ChannelSettings, Message, NotificationEvent},
    secret::Secret,
    server::{AgentInfo, ServerFacts},
};
use spinikus_store::{
    Attempt, BackupRepository, CertificateRepository, DeploymentRepository, InstanceBackupRepository, NotificationRepository,
    ServerRepository,
};

use super::*;
use crate::{
    DeploymentTimings, NewDomain,
    backups::NewStorageDestination,
    deployments::tests::{Setup, connect, deploy, setup},
    notifications::NewChannel,
    settings::ACME_EMAIL,
    test_support::{grant, user},
};

fn kinds(items: &[AttentionItem]) -> Vec<String> {
    items.iter().map(|item| serde_json::to_value(item).unwrap()["kind"].as_str().unwrap().to_string()).collect()
}

/// Fails a new deployment of the setup's service, with `message`.
async fn fail_deployment(setup: &Setup, message: &str) {
    let control = &setup.control;
    let deployment = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    let actor = spinikus_core::actor::Actor::System;
    control.store().transition_deployment(deployment.id, DeploymentStatus::Preparing, None, None, actor).await.unwrap();
    control
        .store()
        .transition_deployment(deployment.id, DeploymentStatus::Failed, Some(EndReason::BuildFailed), Some(message), actor)
        .await
        .unwrap();
}

async fn destination(setup: &Setup) -> spinikus_core::id::StorageDestinationId {
    let new = NewStorageDestination {
        name: "S3".to_string(),
        endpoint: Some("https://s3.example.com/".to_string()),
        region: None,
        bucket: "backups".to_string(),
        path_prefix: None,
        path_style: true,
        access_key_id: "AKIA".to_string(),
        secret_access_key: Secret::new("s3cret"),
    };
    setup.control.create_storage_destination(&setup.admin, new).await.unwrap().id
}

#[tokio::test]
async fn shows_a_services_problems_to_whoever_may_see_it() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let viewer = user(control, "viewer@example.com", false).await;
    let outsider = user(control, "outsider@example.com", false).await;
    grant(control, &viewer, MembershipScope::Environment(setup.environment.id), Role::Viewer).await;
    let mut agent = connect(control, setup.server.id);
    deploy(&setup, &mut agent).await;
    assert!(control.attention(&viewer).await.unwrap().is_empty(), "nothing wrong yet");

    // Nothing reported for the live deployment: crashing, once it lasts.
    let base = Timestamp::now();
    control.watch_crashes_at(base).await.unwrap();
    control.watch_crashes_at(base + SignedDuration::from_mins(2)).await.unwrap();
    fail_deployment(&setup, "cannot pull").await;
    let destination = destination(&setup).await;
    let id = BackupId::new();
    let new = NewBackup {
        id,
        service_id: setup.service.id,
        destination_id: destination,
        schedule_id: None,
        engine: Engine::Postgres,
        object_key: object_key("", setup.service.id, id, Engine::Postgres),
    };
    let backup = control.store().create_backup(new).await.unwrap();
    control.store().finish_backup(backup.id, Err("pg_dump: connection refused".to_string())).await.unwrap();

    let seen = control.attention(&viewer).await.unwrap();
    let hidden = control.attention(&outsider).await.unwrap();
    let admin = control.attention(&setup.admin).await.unwrap();

    assert_eq!(kinds(&seen), ["service_crashing", "deployment_failed", "backup_failed"]);
    match &seen[1] {
        AttentionItem::DeploymentFailed { service, reason, message, .. } => {
            assert_eq!(
                (service.name.as_str(), service.environment_name.as_str(), service.project_name.as_str()),
                ("Web", "Production", "Shop")
            );
            assert_eq!((*reason, message.as_deref()), (Some(EndReason::BuildFailed), Some("cannot pull")));
        }
        other => panic!("{other:?}"),
    }
    match &seen[2] {
        AttentionItem::BackupFailed { error, .. } => assert_eq!(error.as_deref(), Some("pg_dump: connection refused")),
        other => panic!("{other:?}"),
    }
    assert!(hidden.is_empty(), "{hidden:?}");
    assert_eq!(admin, seen);
}

#[tokio::test]
async fn shows_certificates_that_failed_or_expire_soon_while_https_is_on() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let mut agent = connect(control, setup.server.id);
    deploy(&setup, &mut agent).await;
    for hostname in ["shop.example.com", "www.example.com", "fine.example.com"] {
        let new = NewDomain { hostname: hostname.to_string(), redirect_to: None };
        control.create_domain(&setup.admin, setup.service.id, new).await.unwrap();
    }
    let hostname = |name: &str| name.parse::<Hostname>().unwrap();
    let retry_at = Timestamp::now() + SignedDuration::from_mins(5);
    control.store().record_certificate_failure(&hostname("shop.example.com"), "DNS problem", retry_at).await.unwrap();
    let issued = |name: &str, days: i64| NewCertificate {
        hostnames: vec![hostname(name)],
        chain_pem: "-----BEGIN CERTIFICATE-----".to_string(),
        private_key_pem: Secret::new("key"),
        not_before: Timestamp::now() - SignedDuration::from_hours(24 * 60),
        not_after: Timestamp::now() + SignedDuration::from_hours(24 * days),
        acme_account_id: None,
    };
    control.store().record_certificate_issued(&hostname("www.example.com"), issued("www.example.com", 3)).await.unwrap();
    control.store().record_certificate_issued(&hostname("fine.example.com"), issued("fine.example.com", 60)).await.unwrap();
    let off = control.attention(&setup.admin).await.unwrap();

    control.set_setting(&setup.admin, ACME_EMAIL, 0, json!("ops@example.com")).await.unwrap();
    let on = control.attention(&setup.admin).await.unwrap();

    assert!(off.is_empty(), "HTTPS is off: {off:?}");
    assert_eq!(kinds(&on), ["certificate_failed", "certificate_expiring"]);
    match &on[0] {
        AttentionItem::CertificateFailed { hostname, error, failures, expires_at, .. } => {
            assert_eq!(hostname.as_str(), "shop.example.com");
            assert_eq!((error.as_deref(), *failures, *expires_at), (Some("DNS problem"), 1, None));
        }
        other => panic!("{other:?}"),
    }
    match &on[1] {
        AttentionItem::CertificateExpiring { hostname, service, .. } => {
            assert_eq!((hostname.as_str(), service.id), ("www.example.com", setup.service.id));
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn shows_the_instances_problems_to_instance_admins_only() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let member = user(control, "member@example.com", false).await;
    grant(control, &member, MembershipScope::Environment(setup.environment.id), Role::Admin).await;

    // A server whose agent was connected and isn't, and one that never connected.
    let facts = ServerFacts {
        hostname: "web-1".to_string(),
        os: "linux".to_string(),
        arch: "x86_64".to_string(),
        cpu_count: 2,
        memory_bytes: 1 << 30,
    };
    let info = AgentInfo { agent_version: "0.1.0".to_string(), protocol_version: 1, facts };
    control.store().record_agent_hello(setup.server.id, info).await.unwrap();
    control.store().record_disconnect(setup.server.id).await.unwrap();
    control.create_server(&setup.admin, crate::NewServer { name: "pending".to_string() }).await.unwrap();

    let destination = destination(&setup).await;
    for outcome in [Err("upload failed".to_string()), Ok(1)] {
        let id = InstanceBackupId::new();
        let new = NewInstanceBackup { id, destination_id: destination, scheduled: true, object_key: format!("{id}") };
        control.store().create_instance_backup(new).await.unwrap();
        control.store().finish_instance_backup(id, outcome).await.unwrap();
    }
    let succeeded_last = control.attention(&setup.admin).await.unwrap();
    let id = InstanceBackupId::new();
    let new = NewInstanceBackup { id, destination_id: destination, scheduled: false, object_key: format!("{id}") };
    control.store().create_instance_backup(new).await.unwrap();
    control.store().finish_instance_backup(id, Err("upload failed".to_string())).await.unwrap();

    let channel = NewChannel {
        name: "Ops".to_string(),
        settings: ChannelSettings::Webhook { url: "http://127.0.0.1:9/".to_string() },
        events: Vec::new(),
        enabled: true,
        secret: None,
    };
    let channel = control.create_notification_channel(&setup.admin, channel).await.unwrap();
    let message = Message {
        event: NotificationEvent::DeploymentFailed,
        title: "Failed".to_string(),
        text: String::new(),
        path: None,
        details: Default::default(),
    };
    let queued = control.store().queue_notifications(&message, None, &[channel.id]).await.unwrap();
    let failed = Attempt::Failed { error: "connection refused".to_string(), retry_at: None };
    control.store().record_notification_attempt(queued[0].id, failed).await.unwrap();

    let admin = control.attention(&setup.admin).await.unwrap();
    let scoped = crate::test_support::with_token(
        control,
        &setup.admin,
        Role::Admin,
        Some(MembershipScope::Environment(setup.environment.id)),
    )
    .await;

    assert_eq!(kinds(&succeeded_last), ["server_disconnected"], "the latest backup succeeded");
    assert_eq!(kinds(&admin), ["server_disconnected", "instance_backup_failed", "notifications_failing"]);
    match &admin[2] {
        AttentionItem::NotificationsFailing { channel_name, event, error, .. } => {
            assert_eq!((channel_name.as_str(), *event), ("Ops", NotificationEvent::DeploymentFailed));
            assert_eq!(error.as_deref(), Some("connection refused"));
        }
        other => panic!("{other:?}"),
    }
    assert!(control.attention(&member).await.unwrap().is_empty());
    assert!(control.attention(&scoped).await.unwrap().is_empty(), "a scoped token isn't the instance's admin");
}
