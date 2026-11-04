//! Destinations, schedules and backups, against a fake agent and an in-memory bucket.

use std::time::Duration;

use object_store::memory::InMemory;
use spinikus_core::{
    actor::Actor,
    backup::BackupStatus,
    database::Engine,
    deployment::{Deployment, DeploymentStatus},
    membership::MembershipScope,
    server::{InstanceHealth, InstanceStatus, ObservedInstance},
};
use spinikus_proto::agent::v1::{self as pb, command::Kind, command_result::Outcome, control_message};
use spinikus_store::{DeploymentRepository, InstanceRepository};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use super::*;
use crate::{
    DeploymentTimings, NewDatabase,
    deployments::tests::{Setup, setup_with},
    test_support::{grant, user},
};

struct Fixture {
    setup: Setup,
    bucket: Arc<InMemory>,
    agent: mpsc::Receiver<pb::ControlMessage>,
    connection: crate::agents::ConnectionId,
}

async fn fixture(timings: BackupTimings) -> Fixture {
    let bucket = Arc::new(InMemory::new());
    let store = bucket.clone();
    let setup =
        setup_with(DeploymentTimings::default(), |control| control.with_backup_store(store).with_backup_timings(timings)).await;
    let (sender, agent) = mpsc::channel(64);
    let (connection, _) = setup.control.agents().register(setup.server.id, sender);
    Fixture { setup, bucket, agent, connection }
}

/// Every command sent so far, but route tables and the like.
fn commands(agent: &mut mpsc::Receiver<pb::ControlMessage>) -> Vec<pb::Command> {
    let mut commands = Vec::new();
    while let Ok(message) = agent.try_recv() {
        if let Some(control_message::Message::Command(command)) = message.message {
            commands.push(command);
        }
    }
    commands
}

fn backup_commands(agent: &mut mpsc::Receiver<pb::ControlMessage>) -> Vec<(String, pb::BackupDatabase)> {
    commands(agent)
        .into_iter()
        .filter_map(|command| match command.kind {
            Some(Kind::BackupDatabase(backup)) => Some((command.command_id, backup)),
            _ => None,
        })
        .collect()
}

fn result(command_id: &str, outcome: Outcome) -> pb::CommandResult {
    pb::CommandResult { command_id: command_id.to_string(), outcome: Some(outcome) }
}

fn uploaded(size_bytes: u64) -> Outcome {
    Outcome::Succeeded(pb::CommandSucceeded { prepared: None, backup: Some(pb::BackupUploaded { size_bytes }) })
}

/// A Postgres database on the setup's server, live through the fake agent, with its container
/// reported as `pg-container`.
async fn live_database(fixture: &mut Fixture) -> (Service, Deployment) {
    live_engine(fixture, Engine::Postgres).await
}

/// A database of `engine`, live in a container named after it.
async fn live_engine(fixture: &mut Fixture, engine: Engine) -> (Service, Deployment) {
    let control = &fixture.setup.control;
    let new = NewDatabase {
        name: engine.to_string(),
        slug: None,
        engine: Some(engine),
        version: None,
        server_id: Some(fixture.setup.server.id),
        from_backup: None,
        deploy: true,
    };
    let created = control.create_database(&fixture.setup.admin, fixture.setup.service.environment_id, new).await.unwrap();
    for step in ["prepare", "start"] {
        control.advance_deployments().await;
        let command = commands(&mut fixture.agent).into_iter().find(|command| command.command_id.ends_with(step)).unwrap();
        let prepared =
            (step == "prepare").then(|| pb::PreparedImage { image: "postgres@sha256:18".to_string(), ..Default::default() });
        let outcome = Outcome::Succeeded(pb::CommandSucceeded { prepared, backup: None });
        control.deployment_result(fixture.setup.server.id, result(&command.command_id, outcome)).await;
    }
    let deployment = control.store().get_deployment(created.deployment.as_ref().unwrap().id).await.unwrap();
    assert_eq!(deployment.status, DeploymentStatus::Live);
    let container = ObservedInstance {
        service_id: created.service.id,
        deployment_id: deployment.id,
        container_id: if engine == Engine::Postgres { "pg-container".to_string() } else { format!("{engine}-container") },
        status: InstanceStatus::Running,
        health: InstanceHealth::None,
        restart_count: 0,
        started_at: None,
    };
    control.store().upsert_instance(fixture.setup.server.id, container).await.unwrap();
    (created.service, deployment)
}

fn destination(name: &str) -> NewStorageDestination {
    NewStorageDestination {
        name: name.to_string(),
        endpoint: Some("https://s3.example.com/".to_string()),
        region: None,
        bucket: "backups".to_string(),
        path_prefix: Some("/prod/db".to_string()),
        path_style: true,
        access_key_id: "AKIA".to_string(),
        secret_access_key: Secret::new("s3cret"),
    }
}

fn hourly(destination_id: StorageDestinationId, keep_last: u32) -> NewBackupSchedule {
    NewBackupSchedule { destination_id, cron: "0 * * * *".to_string(), keep_last: Some(keep_last), enabled: None }
}

#[tokio::test]
async fn instance_admins_manage_destinations_that_everyone_sees() {
    let fixture = fixture(BackupTimings::default()).await;
    let (control, admin) = (&fixture.setup.control, &fixture.setup.admin);
    let someone = user(control, "ada@example.com", false).await;

    let created = control.create_storage_destination(admin, destination("Primary")).await.unwrap();
    let by_someone = control.create_storage_destination(&someone, destination("Mine")).await;
    let mut bad = destination("Bad");
    bad.bucket = "Not_A_Bucket".to_string();
    let bad_bucket = control.create_storage_destination(admin, bad).await;
    let mut bad = destination("Bad");
    bad.endpoint = Some("ftp://files.example.com".to_string());
    let bad_endpoint = control.create_storage_destination(admin, bad).await;
    let mut bad = destination("Bad");
    bad.path_prefix = Some("a/../b".to_string());
    let bad_prefix = control.create_storage_destination(admin, bad).await;
    let listed = control.list_storage_destinations(&someone).await.unwrap();
    let changes =
        StorageDestinationChanges { endpoint: Some(None), secret_access_key: Some(Secret::new("n3w")), ..Default::default() };
    let updated = control.update_storage_destination(admin, created.id, created.revision, changes).await.unwrap();
    let tested = control.test_storage_destination(admin, created.id).await;

    assert_eq!(created.fields.path_prefix, "prod/db/");
    assert_eq!(created.fields.endpoint.as_deref(), Some("https://s3.example.com"));
    assert_eq!(created.fields.region, "us-east-1");
    assert!(matches!(by_someone, Err(ControlError::Forbidden)), "{by_someone:?}");
    assert!(matches!(bad_bucket, Err(ControlError::Invalid { field: "bucket", .. })), "{bad_bucket:?}");
    assert!(matches!(bad_endpoint, Err(ControlError::Invalid { field: "endpoint", .. })), "{bad_endpoint:?}");
    assert!(matches!(bad_prefix, Err(ControlError::Invalid { field: "path_prefix", .. })), "{bad_prefix:?}");
    assert_eq!(listed, std::slice::from_ref(&created));
    assert_eq!((updated.fields.endpoint, updated.revision), (None, 2));
    assert_eq!(control.store().storage_destination_secret(created.id).await.unwrap().expose(), "n3w");
    assert!(tested.is_ok(), "{tested:?}");
    assert!(fixture.bucket.list(None).next().await.is_none(), "the test object is deleted");
}

#[tokio::test]
async fn admins_schedule_backups_of_databases_only() {
    let mut fixture = fixture(BackupTimings::default()).await;
    let (database, _) = live_database(&mut fixture).await;
    let (control, admin) = (&fixture.setup.control, &fixture.setup.admin);
    let developer = user(control, "ada@example.com", false).await;
    grant(control, &developer, MembershipScope::Environment(database.environment_id), Role::Developer).await;
    let destination = control.create_storage_destination(admin, destination("Primary")).await.unwrap();

    let schedule = control.create_backup_schedule(admin, database.id, hourly(destination.id, 3)).await.unwrap();
    let by_developer = control.create_backup_schedule(&developer, database.id, hourly(destination.id, 3)).await;
    let of_an_app = control.create_backup_schedule(admin, fixture.setup.service.id, hourly(destination.id, 3)).await;
    let mut bad = hourly(destination.id, 3);
    bad.cron = "0 0 * * * *".to_string();
    let six_fields = control.create_backup_schedule(admin, database.id, bad).await;
    let too_many = control.create_backup_schedule(admin, database.id, hourly(destination.id, 5000)).await;
    let in_use = control.delete_storage_destination(admin, destination.id).await;
    let changes = BackupScheduleChanges { enabled: Some(false), ..Default::default() };
    let disabled = control.update_backup_schedule(admin, schedule.id, schedule.revision, changes).await.unwrap();

    let next = schedule.next_run_at.unwrap();
    assert!(next > Timestamp::now() && next.as_second() % 3600 == 0, "{next}");
    assert!(matches!(by_developer, Err(ControlError::Forbidden)), "{by_developer:?}");
    assert!(matches!(of_an_app, Err(ControlError::Rejected { .. })), "{of_an_app:?}");
    assert!(matches!(six_fields, Err(ControlError::Invalid { field: "cron", .. })), "{six_fields:?}");
    assert!(matches!(too_many, Err(ControlError::Invalid { field: "keep_last", .. })), "{too_many:?}");
    assert!(matches!(in_use, Err(ControlError::HasChildren { .. })), "{in_use:?}");
    assert_eq!((disabled.fields.enabled, disabled.next_run_at), (false, None));
}

#[tokio::test]
async fn scheduled_backups_run_on_the_databases_agent_and_old_ones_go() {
    let mut fixture = fixture(BackupTimings::default()).await;
    let (database, _) = live_database(&mut fixture).await;
    let (control, admin, server) = (&fixture.setup.control, &fixture.setup.admin, fixture.setup.server.id);
    let destination = control.create_storage_destination(admin, destination("Primary")).await.unwrap();
    let schedule = control.create_backup_schedule(admin, database.id, hourly(destination.id, 1)).await.unwrap();

    let mut made = Vec::new();
    for size in [100, 200] {
        let past = Timestamp::now() - jiff::SignedDuration::from_secs(1);
        control.store().set_backup_schedule_next_run(schedule.id, Some(past)).await.unwrap();
        control.advance_backups().await;
        let (command_id, command) = backup_commands(&mut fixture.agent).pop().unwrap();
        let backup: BackupId = command.backup_id.parse().unwrap();
        assert_eq!(control.store().get_backup(backup).await.unwrap().status, BackupStatus::Running);
        // What the agent would have uploaded.
        fixture.bucket.put(&Path::from(command.object_key.as_str()), PutPayload::from_static(b"dump")).await.unwrap();
        control.backup_result(server, result(&command_id, uploaded(size))).await;
        made.push((backup, command));
    }
    control.advance_backups().await;

    let (first, command) = &made[0];
    assert_eq!(command.container_id, "pg-container");
    assert_eq!(command.command, Engine::Postgres.dump_command());
    assert_eq!(command.object_key, format!("prod/db/{}/{first}.pgdump", database.id));
    let sent = command.destination.as_ref().unwrap();
    assert_eq!((sent.bucket.as_str(), sent.secret_access_key.as_str(), sent.path_style), ("backups", "s3cret", true));
    let newest = control.get_backup(admin, made[1].0).await.unwrap();
    assert_eq!((newest.status, newest.size_bytes, newest.schedule_id), (BackupStatus::Succeeded, Some(200), Some(schedule.id)));
    assert!(matches!(control.get_backup(admin, *first).await, Err(ControlError::NotFound { .. })));
    assert!(fixture.bucket.head(&Path::from(command.object_key.as_str())).await.is_err(), "the old dump is deleted");
    assert!(fixture.bucket.head(&Path::from(made[1].1.object_key.as_str())).await.is_ok());
    let schedule = control.get_backup_schedule(admin, schedule.id).await.unwrap();
    assert!(schedule.next_run_at.unwrap() > Timestamp::now());
}

#[tokio::test]
async fn manual_backups_fail_without_a_live_database_and_report_why() {
    let mut fixture = fixture(BackupTimings::default()).await;
    let (database, deployment) = live_database(&mut fixture).await;
    let (control, admin, server) = (&fixture.setup.control, &fixture.setup.admin, fixture.setup.server.id);
    let viewer = user(control, "vic@example.com", false).await;
    grant(control, &viewer, MembershipScope::Environment(database.environment_id), Role::Viewer).await;
    let destination = control.create_storage_destination(admin, destination("Primary")).await.unwrap();
    let other = control.create_server(admin, crate::NewServer { name: "db-2".to_string() }).await.unwrap();

    let by_viewer = control.create_backup(&viewer, database.id, destination.id, None).await;
    let of_an_app = control.create_backup(admin, fixture.setup.service.id, destination.id, None).await;
    let backup = control.create_backup(admin, database.id, destination.id, None).await.unwrap();
    control.advance_backups().await;
    let (command_id, _) = backup_commands(&mut fixture.agent).pop().unwrap();
    let running = control.delete_backup(admin, backup.id).await;
    control.backup_result(other.id, result(&command_id, uploaded(1))).await;
    let still_running = control.get_backup(&viewer, backup.id).await.unwrap().status;
    let failure = pb::CommandFailed { reason: pb::FailureReason::BackupFailed.into(), message: "pg_dump: error".to_string() };
    control.backup_result(server, result(&command_id, Outcome::Failed(failure))).await;
    let failed = control.get_backup(&viewer, backup.id).await.unwrap();
    let by_viewer_delete = control.delete_backup(&viewer, backup.id).await;
    control.delete_backup(admin, backup.id).await.unwrap();

    let draining = DeploymentStatus::Draining;
    control.store().transition_deployment(deployment.id, draining, None, None, Actor::System).await.unwrap();
    let nothing_live = control.create_backup(admin, database.id, destination.id, None).await.unwrap();
    control.advance_backups().await;
    let nothing_live = control.get_backup(admin, nothing_live.id).await.unwrap();

    assert!(matches!(by_viewer, Err(ControlError::Forbidden)), "{by_viewer:?}");
    assert!(matches!(of_an_app, Err(ControlError::Rejected { .. })), "{of_an_app:?}");
    assert!(matches!(running, Err(ControlError::Rejected { .. })), "{running:?}");
    assert_eq!(still_running, BackupStatus::Running);
    assert_eq!((failed.status, failed.error.as_deref()), (BackupStatus::Failed, Some("pg_dump: error")));
    assert!(matches!(by_viewer_delete, Err(ControlError::Forbidden)), "{by_viewer_delete:?}");
    assert!(matches!(control.get_backup(admin, backup.id).await, Err(ControlError::NotFound { .. })));
    assert_eq!(nothing_live.status, BackupStatus::Failed);
    assert_eq!(nothing_live.error.as_deref(), Some("the database has nothing live"));
    assert_eq!(control.list_backups(&viewer, database.id, None, 10).await.unwrap().items, [nothing_live]);
}

#[tokio::test]
async fn backups_wait_for_the_agent_then_fail_and_are_resent_while_unanswered() {
    let timings =
        BackupTimings { resend_after: Duration::ZERO, agent_lost_after: Duration::from_millis(200), ..Default::default() };
    let mut fixture = fixture(timings).await;
    let (database, _) = live_database(&mut fixture).await;
    let (control, admin, server) = (&fixture.setup.control, &fixture.setup.admin, fixture.setup.server.id);
    let destination = control.create_storage_destination(admin, destination("Primary")).await.unwrap();

    let resent = control.create_backup(admin, database.id, destination.id, None).await.unwrap();
    control.advance_backups().await;
    control.advance_backups().await;
    let sent = backup_commands(&mut fixture.agent);
    control.backup_result(server, result(&sent[0].0, uploaded(5))).await;

    control.agents().unregister(server, fixture.connection);
    let waiting = control.create_backup(admin, database.id, destination.id, None).await.unwrap();
    control.advance_backups().await;
    let before = control.get_backup(admin, waiting.id).await.unwrap().status;
    tokio::time::sleep(Duration::from_millis(300)).await;
    control.advance_backups().await;
    let after = control.get_backup(admin, waiting.id).await.unwrap();

    assert_eq!(sent.len(), 2, "sent again while unanswered");
    assert!(sent.iter().all(|(id, _)| *id == format!("{}/backup", resent.id)));
    assert_eq!(control.get_backup(admin, resent.id).await.unwrap().size_bytes, Some(5));
    assert_eq!(before, BackupStatus::Queued);
    assert_eq!(after.status, BackupStatus::Failed);
    assert!(after.error.unwrap().contains("is not connected"));
}

#[test]
fn schedules_take_five_field_cron_expressions_in_utc() {
    let daily = cron("30 3 * * *").unwrap();
    let after: Timestamp = "2026-10-31T04:00:00Z".parse().unwrap();

    assert_eq!(next_run(&daily, after).unwrap().to_string(), "2026-11-01T03:30:00Z");
    assert!(cron("61 * * * *").is_err());
    assert!(cron("* * * *").is_err());
}

/// What a command list holds, by kind: (command ID, `B`ackup or `R`estore).
fn kinds(commands: &[pb::Command]) -> Vec<(String, char)> {
    commands
        .iter()
        .filter_map(|command| match &command.kind {
            Some(Kind::BackupDatabase(_)) => Some((command.command_id.clone(), 'B')),
            Some(Kind::RestoreDatabase(_)) => Some((command.command_id.clone(), 'R')),
            _ => None,
        })
        .collect()
}

/// A backup of the database that succeeded, made through the fake agent.
async fn backed_up(fixture: &mut Fixture, database: ServiceId, destination: StorageDestinationId) -> Backup {
    let control = &fixture.setup.control;
    let backup = control.create_backup(&fixture.setup.admin, database, destination, None).await.unwrap();
    control.advance_backups().await;
    let (command_id, _) = backup_commands(&mut fixture.agent).pop().unwrap();
    control.backup_result(fixture.setup.server.id, result(&command_id, uploaded(42))).await;
    control.store().get_backup(backup.id).await.unwrap()
}

fn done() -> Outcome {
    Outcome::Succeeded(pb::CommandSucceeded::default())
}

#[tokio::test]
async fn admins_restore_after_a_safety_backup() {
    let mut fixture = fixture(BackupTimings::default()).await;
    let (database, _) = live_database(&mut fixture).await;
    let (control, admin, server) = (&fixture.setup.control, &fixture.setup.admin, fixture.setup.server.id);
    let developer = user(control, "ada@example.com", false).await;
    grant(control, &developer, MembershipScope::Environment(database.environment_id), Role::Developer).await;
    let destination = control.create_storage_destination(admin, destination("Primary")).await.unwrap();
    let backup = backed_up(&mut fixture, database.id, destination.id).await;
    let (control, admin) = (&fixture.setup.control, &fixture.setup.admin);
    let other = control.create_server(admin, crate::NewServer { name: "db-2".to_string() }).await.unwrap();

    let by_developer = control.restore_backup(&developer, backup.id).await;
    let restore = control.restore_backup(admin, backup.id).await.unwrap();
    let twice = control.restore_backup(admin, backup.id).await;
    control.advance_backups().await;
    let first_pass = kinds(&commands(&mut fixture.agent));
    let deleting = control.delete_backup(admin, backup.id).await;
    let safety = restore.safety_backup_id.unwrap();
    control.backup_result(server, result(&format!("{safety}/backup"), uploaded(7))).await;
    control.advance_backups().await;
    let sent = commands(&mut fixture.agent);
    let running = control.get_restore(&developer, restore.id).await.unwrap();
    control.backup_result(other.id, result(&format!("{}/restore", restore.id), done())).await;
    let still_running = control.get_restore(admin, restore.id).await.unwrap().status;
    control.backup_result(server, result(&format!("{}/restore", restore.id), done())).await;
    let restored = control.get_restore(admin, restore.id).await.unwrap();
    let listed = control.list_restores(&developer, database.id, None, 10).await.unwrap().items;

    assert!(matches!(by_developer, Err(ControlError::Forbidden)), "{by_developer:?}");
    assert_eq!((restore.status, restore.backup_id), (BackupStatus::Queued, backup.id));
    assert!(matches!(twice, Err(ControlError::Rejected { .. })), "{twice:?}");
    assert_eq!(first_pass, [(format!("{safety}/backup"), 'B')], "the safety backup goes first, alone");
    assert!(matches!(deleting, Err(ControlError::Rejected { .. })), "{deleting:?}");
    let Some(Kind::RestoreDatabase(command)) = sent.into_iter().next().and_then(|command| command.kind) else {
        panic!("no restore sent");
    };
    assert_eq!(command.container_id, "pg-container");
    let plan = Engine::Postgres.restore_plan();
    assert_eq!((command.command, command.restart, command.ready_command), (plan.command, false, Vec::new()));
    assert_eq!(command.object_key, backup.object_key);
    assert_eq!(command.destination.unwrap().secret_access_key, "s3cret");
    assert_eq!((running.status, running.server_id), (BackupStatus::Running, Some(server)));
    assert_eq!(still_running, BackupStatus::Running);
    assert_eq!(restored.status, BackupStatus::Succeeded);
    assert_eq!(listed, [restored]);
    let safety = control.get_backup(admin, safety).await.unwrap();
    assert_eq!((safety.status, safety.schedule_id), (BackupStatus::Succeeded, None));
}

#[tokio::test]
async fn restores_fail_with_their_safety_backup_and_need_something_to_restore() {
    let mut fixture = fixture(BackupTimings::default()).await;
    let (database, deployment) = live_database(&mut fixture).await;
    let (control, admin, server) = (&fixture.setup.control, &fixture.setup.admin, fixture.setup.server.id);
    let destination = control.create_storage_destination(admin, destination("Primary")).await.unwrap();
    let backup = backed_up(&mut fixture, database.id, destination.id).await;
    let (control, admin) = (&fixture.setup.control, &fixture.setup.admin);

    let restore = control.restore_backup(admin, backup.id).await.unwrap();
    control.advance_backups().await;
    let safety = restore.safety_backup_id.unwrap();
    let failure = pb::CommandFailed { reason: pb::FailureReason::BackupFailed.into(), message: "disk full".to_string() };
    control.backup_result(server, result(&format!("{safety}/backup"), Outcome::Failed(failure))).await;
    control.advance_backups().await;
    let failed = control.get_restore(admin, restore.id).await.unwrap();
    let restored_anything = kinds(&commands(&mut fixture.agent)).iter().any(|(_, kind)| *kind == 'R');

    let failed_backup = control.get_backup(admin, safety).await.unwrap();
    let of_a_failed_backup = control.restore_backup(admin, failed_backup.id).await;
    let draining = DeploymentStatus::Draining;
    control.store().transition_deployment(deployment.id, draining, None, None, Actor::System).await.unwrap();
    let nothing_live = control.restore_backup(admin, backup.id).await;

    assert_eq!(failed.status, BackupStatus::Failed);
    assert_eq!(failed.error.as_deref(), Some("the safety backup failed: disk full"));
    assert!(!restored_anything);
    assert!(matches!(of_a_failed_backup, Err(ControlError::Rejected { .. })), "{of_a_failed_backup:?}");
    assert!(matches!(nothing_live, Err(ControlError::Rejected { .. })), "{nothing_live:?}");
}

#[tokio::test]
async fn redis_restores_by_restarting_and_waiting_until_it_answers() {
    let mut fixture = fixture(BackupTimings::default()).await;
    let (redis, _) = live_engine(&mut fixture, Engine::Redis).await;
    let admin = fixture.setup.admin.clone();
    let destination = fixture.setup.control.create_storage_destination(&admin, destination("Primary")).await.unwrap();
    let backup = backed_up(&mut fixture, redis.id, destination.id).await;
    let (control, server) = (&fixture.setup.control, fixture.setup.server.id);

    let restore = control.restore_backup(&admin, backup.id).await.unwrap();
    control.advance_backups().await;
    let safety = restore.safety_backup_id.unwrap();
    control.backup_result(server, result(&format!("{safety}/backup"), uploaded(7))).await;
    control.advance_backups().await;
    let sent = commands(&mut fixture.agent);

    let restores = sent.into_iter().filter_map(|command| match command.kind {
        Some(Kind::RestoreDatabase(restore)) => Some(restore),
        _ => None,
    });
    let [command] = restores.collect::<Vec<_>>().try_into().expect("one restore sent");
    let plan = Engine::Redis.restore_plan();
    assert_eq!(command.container_id, "redis-container");
    assert_eq!(command.object_key, backup.object_key);
    assert!(command.object_key.ends_with(".rdb"));
    assert_eq!((command.command, command.restart, command.ready_command), (plan.command, true, plan.ready_command));
}

#[tokio::test]
async fn backups_restore_into_a_new_database_once_it_runs() {
    let mut fixture = fixture(BackupTimings::default()).await;
    let (database, _) = live_database(&mut fixture).await;
    let (control, admin, server) = (&fixture.setup.control, &fixture.setup.admin, fixture.setup.server.id);
    let developer = user(control, "ada@example.com", false).await;
    grant(control, &developer, MembershipScope::Environment(database.environment_id), Role::Developer).await;
    let destination = control.create_storage_destination(admin, destination("Primary")).await.unwrap();
    let backup = backed_up(&mut fixture, database.id, destination.id).await;
    let (control, admin) = (&fixture.setup.control, &fixture.setup.admin);
    let copy = |name: &str, engine: Option<Engine>, version: Option<&str>| NewDatabase {
        name: name.to_string(),
        slug: None,
        engine,
        version: version.map(str::to_string),
        server_id: Some(server),
        from_backup: Some(backup.id),
        deploy: true,
    };
    let environment = database.environment_id;

    let by_developer = control.create_database(&developer, environment, copy("Copy", None, None)).await;
    let older = control.create_database(admin, environment, copy("Copy", None, Some("17"))).await;
    let other_engine = control.create_database(admin, environment, copy("Copy", Some(Engine::Redis), None)).await;
    let created = control.create_database(admin, environment, copy("Copy", None, None)).await.unwrap();
    control.advance_backups().await;
    let before_it_runs = kinds(&commands(&mut fixture.agent));

    assert!(matches!(by_developer, Err(ControlError::Forbidden)), "{by_developer:?}");
    assert!(matches!(older, Err(ControlError::Invalid { field: "version", .. })), "{older:?}");
    assert!(matches!(other_engine, Err(ControlError::Invalid { field: "engine", .. })), "{other_engine:?}");
    assert_eq!(created.service.config, database.config, "the same engine and version");
    let restore = created.restore.clone().unwrap();
    assert_eq!((restore.service_id, restore.backup_id, restore.safety_backup_id), (created.service.id, backup.id, None));
    assert!(before_it_runs.is_empty(), "{before_it_runs:?}");

    // Once it is live, the restore goes to its container.
    for step in ["prepare", "start"] {
        control.advance_deployments().await;
        let command = commands(&mut fixture.agent).into_iter().find(|command| command.command_id.ends_with(step)).unwrap();
        let prepared =
            (step == "prepare").then(|| pb::PreparedImage { image: "postgres@sha256:18".to_string(), ..Default::default() });
        let outcome = Outcome::Succeeded(pb::CommandSucceeded { prepared, backup: None });
        control.deployment_result(server, result(&command.command_id, outcome)).await;
    }
    let container = ObservedInstance {
        service_id: created.service.id,
        deployment_id: created.deployment.as_ref().unwrap().id,
        container_id: "copy-container".to_string(),
        status: InstanceStatus::Running,
        health: InstanceHealth::None,
        restart_count: 0,
        started_at: None,
    };
    control.store().upsert_instance(server, container).await.unwrap();
    control.advance_backups().await;
    let Some(Kind::RestoreDatabase(command)) = commands(&mut fixture.agent).into_iter().find_map(|command| match command.kind {
        Some(Kind::RestoreDatabase(_)) => command.kind,
        _ => None,
    }) else {
        panic!("no restore sent");
    };
    assert_eq!((command.container_id.as_str(), command.object_key.as_str()), ("copy-container", backup.object_key.as_str()));

    // A new database whose deployment fails gets no restore.
    let failing = control.create_database(admin, environment, copy("Broken", None, None)).await.unwrap();
    control.advance_deployments().await;
    let prepare = commands(&mut fixture.agent).into_iter().find(|command| command.command_id.ends_with("prepare")).unwrap();
    let failure = pb::CommandFailed { reason: pb::FailureReason::BuildFailed.into(), message: "pull failed".to_string() };
    control.deployment_result(server, result(&prepare.command_id, Outcome::Failed(failure))).await;
    control.advance_backups().await;
    let failed = control.get_restore(admin, failing.restore.unwrap().id).await.unwrap();
    assert_eq!(failed.status, BackupStatus::Failed);
    let error = failed.error.unwrap();
    assert!(error.starts_with("the database's deployment") && error.ends_with("ended as failed"), "{error}");
}

/// The volume commands sent so far, by command ID.
fn volume_commands(agent: &mut mpsc::Receiver<pb::ControlMessage>) -> Vec<(String, Kind)> {
    commands(agent)
        .into_iter()
        .filter_map(|command| match command.kind {
            Some(kind @ (Kind::BackupVolume(_) | Kind::RestoreVolume(_))) => Some((command.command_id, kind)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn backs_up_and_restores_an_apps_volumes_in_a_helper() {
    let mut fixture = fixture(BackupTimings::default()).await;
    let control = fixture.setup.control.clone();
    let (admin, server, app) = (fixture.setup.admin.clone(), fixture.setup.server.id, fixture.setup.service.id);
    let volume = |name: &str| crate::NewVolume { name: name.to_string(), mount_path: format!("/app/{name}") };
    let uploads = control.create_volume(&admin, app, volume("uploads")).await.unwrap();
    let destination = control.create_storage_destination(&admin, destination("Primary")).await.unwrap();

    // Never deployed: its volume is on no server yet.
    let early = control.create_backup(&admin, app, destination.id, None).await.unwrap();
    control.advance_backups().await;
    let early = control.get_backup(&admin, early.id).await.unwrap();

    let live = crate::deployments::tests::deploy(&fixture.setup, &mut fixture.agent).await;
    let container = ObservedInstance {
        service_id: app,
        deployment_id: live.id,
        container_id: "web-container".to_string(),
        status: InstanceStatus::Running,
        health: InstanceHealth::None,
        restart_count: 0,
        started_at: None,
    };
    control.store().upsert_instance(server, container).await.unwrap();
    let cache = control.create_volume(&admin, app, volume("cache")).await.unwrap();
    let unnamed = control.create_backup(&admin, app, destination.id, None).await;
    let other = VolumeId::new();
    let not_its = control.create_backup(&admin, app, destination.id, Some(other)).await;
    let backup = control.create_backup(&admin, app, destination.id, Some(uploads.id)).await.unwrap();
    control.advance_backups().await;
    let sent = volume_commands(&mut fixture.agent);
    control.backup_result(server, result(&sent[0].0, uploaded(2048))).await;
    let backup = control.get_backup(&admin, backup.id).await.unwrap();

    assert_eq!(early.status, BackupStatus::Failed);
    assert_eq!(early.error.as_deref(), Some("the service has never run, so its volumes aren't on any server yet"));
    assert!(matches!(unnamed, Err(ControlError::Invalid { field: "volume_id", .. })), "{unnamed:?}");
    assert!(matches!(not_its, Err(ControlError::Invalid { field: "volume_id", .. })), "{not_its:?}");
    match &sent[..] {
        [(_, Kind::BackupVolume(command))] => {
            assert_eq!((command.volume_id.as_str(), command.service_id.as_str()), (&*uploads.id.to_string(), &*app.to_string()));
            assert_eq!(command.image, spinikus_core::backup::HELPER_IMAGE);
            assert_eq!(command.command, spinikus_core::backup::volume_archive_command());
        }
        other => panic!("{other:?}"),
    }
    assert_eq!((backup.status, backup.volume_id, backup.engine), (BackupStatus::Succeeded, Some(uploads.id), None));
    assert!(backup.object_key.ends_with(".tar.gz"), "{}", backup.object_key);

    // Restoring: a safety backup of the same volume first, then the archive goes back with
    // the app's container stopped meanwhile.
    let restore = control.restore_backup(&admin, backup.id).await.unwrap();
    control.advance_backups().await;
    let safety = volume_commands(&mut fixture.agent);
    control.backup_result(server, result(&safety[0].0, uploaded(4096))).await;
    control.advance_backups().await;
    let restoring = volume_commands(&mut fixture.agent);
    control.backup_result(server, result(&restoring[0].0, done())).await;
    let restored = control.get_restore(&admin, restore.id).await.unwrap();
    let safety_backup = control.get_backup(&admin, restore.safety_backup_id.unwrap()).await.unwrap();

    assert!(matches!(&safety[..], [(_, Kind::BackupVolume(command))] if command.volume_id == uploads.id.to_string()));
    assert_eq!(safety_backup.volume_id, Some(uploads.id));
    match &restoring[..] {
        [(id, Kind::RestoreVolume(command))] => {
            assert_eq!(*id, format!("{}/restore", restore.id));
            assert_eq!((command.container_id.as_str(), command.volume_id.clone()), ("web-container", uploads.id.to_string()));
            assert_eq!(command.grace_period, Some(prost_types::Duration { seconds: 20, nanos: 0 }));
            assert_eq!(command.command, spinikus_core::backup::volume_restore_command());
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(restored.status, BackupStatus::Succeeded);

    // A schedule backs up every volume each run; a deleted volume can't be restored into.
    let schedule = control.create_backup_schedule(&admin, app, hourly(destination.id, 3)).await.unwrap();
    let due = Timestamp::now() - jiff::SignedDuration::from_mins(1);
    control.store().set_backup_schedule_next_run(schedule.id, Some(due)).await.unwrap();
    control.advance_backups().await;
    let mut scheduled: Vec<String> = volume_commands(&mut fixture.agent)
        .into_iter()
        .filter_map(|(_, kind)| match kind {
            Kind::BackupVolume(command) => Some(command.volume_id),
            _ => None,
        })
        .collect();
    scheduled.sort();
    control.delete_volume(&admin, uploads.id).await.unwrap();
    let into_deleted = control.restore_backup(&admin, backup.id).await;

    let mut expected = vec![uploads.id.to_string(), cache.id.to_string()];
    expected.sort();
    assert_eq!(scheduled, expected);
    match into_deleted {
        Err(ControlError::Rejected { problem, .. }) => assert!(problem.contains("its volume was deleted"), "{problem}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(control.get_backup(&admin, backup.id).await.unwrap().volume_id, Some(uploads.id), "kept with its volume gone");
}
