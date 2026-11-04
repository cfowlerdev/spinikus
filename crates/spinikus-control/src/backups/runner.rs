//! The backup runner: turns due schedules into queued backups, sends each queued backup or
//! restore to the agent of the server its database is live on, and applies the schedules'
//! retention.
//!
//! The database is the state, like for deployments: a queued run is sent when its
//! database's agent is connected (and fails when the database has nothing live, or the
//! agent stays away too long), a running one is sent again while unanswered, and fails after
//! `run_timeout`. Command IDs are `<backup ID>/backup` and `<restore ID>/restore`, so the
//! agent answers a repeat with the result it already has. A result only counts from the
//! server the run started on. A restore waits for its safety backup, and fails if it fails,
//! and for the database's deployment in progress when it has none live, such as a new
//! database's first, failing if that deployment fails.
//!
//! A schedule that missed runs while the control plane was down makes one backup, not one
//! per missed run.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    time::Duration,
};

use jiff::Timestamp;
use spinikus_core::{
    backup::{Backup, BackupStatus, Restore},
    deployment::DeploymentStatus,
    id::{BackupId, RestoreId, ServerId, ServiceId},
};
use spinikus_proto::agent::v1::{self as pb, command::Kind, command_result::Outcome, control_message};
use spinikus_store::{
    BackupRepository, BackupScheduleRepository, DeploymentRepository, InstanceRepository, RestoreRepository, ServerRepository,
    ServiceRepository, StorageDestinationRepository, StoreError,
};
use tokio::{
    sync::{Mutex, Notify},
    time::Instant,
};

use super::{cron, next_run, storage::BackupStores};
use crate::{Control, ControlError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackupTimings {
    /// How often to look for due schedules and backups to send, besides when something
    /// happens. Schedules have minute precision.
    pub tick: Duration,
    /// How long an unanswered backup command waits before it is sent again.
    pub resend_after: Duration,
    /// How long a backup waits for its database's agent before it fails.
    pub agent_lost_after: Duration,
    /// The longest a backup may run.
    pub run_timeout: Duration,
}

impl Default for BackupTimings {
    fn default() -> Self {
        Self {
            tick: Duration::from_secs(20),
            resend_after: Duration::from_secs(30),
            agent_lost_after: Duration::from_secs(2 * 60),
            run_timeout: Duration::from_secs(6 * 60 * 60),
        }
    }
}

pub(crate) struct Backups {
    timings: BackupTimings,
    wake: Notify,
    /// When each running backup's or restore's command was last sent, by command ID, and to
    /// which server. Held across a whole pass or result, so they never interleave.
    sent: Mutex<Sent>,
    pub(crate) stores: BackupStores,
    /// Waits count from here at the earliest, so a restarted control plane gives agents time
    /// to reconnect.
    started_at: Timestamp,
}

impl Backups {
    pub(crate) fn new(timings: BackupTimings, stores: BackupStores) -> Self {
        Self { timings, wake: Notify::new(), sent: Mutex::default(), stores, started_at: Timestamp::now() }
    }

    pub(crate) fn timings(&self) -> BackupTimings {
        self.timings
    }
}

const BACKUP_STEP: &str = "backup";
const RESTORE_STEP: &str = "restore";

/// What a command ID names: a backup's or a restore's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunId {
    Backup(BackupId),
    Restore(RestoreId),
}

impl RunId {
    fn command_id(self) -> String {
        match self {
            Self::Backup(id) => format!("{id}/{BACKUP_STEP}"),
            Self::Restore(id) => format!("{id}/{RESTORE_STEP}"),
        }
    }
}

/// The backup or restore a command ID names, or `None` if it names neither.
pub(crate) fn parse_command_id(id: &str) -> Option<RunId> {
    match id.split_once('/')? {
        (backup, BACKUP_STEP) => backup.parse().ok().map(RunId::Backup),
        (restore, RESTORE_STEP) => restore.parse().ok().map(RunId::Restore),
        _ => None,
    }
}

type Sent = HashMap<String, (ServerId, Instant)>;

/// Where a database runs, or why it can't be reached yet or ever.
enum Located {
    Ready {
        server: ServerId,
        container_id: String,
    },
    /// It may become ready: the agent is away, or the container not reported yet.
    Waiting(String),
    Never(String),
}

/// A backup or restore, as far as sending it goes.
struct Run {
    id: RunId,
    status: BackupStatus,
    /// Since when a queued run has been ready to go, for how long it waits for the agent.
    waiting_since: Timestamp,
    started_at: Option<Timestamp>,
    server_id: Option<ServerId>,
}

/// What to do with a run on this pass.
enum Next {
    /// Send its command; `first` if the run is queued and starts with it.
    Send {
        server: ServerId,
        container_id: String,
        first: bool,
    },
    Fail(String),
    Wait,
}

/// How long ago `since` was, zero if in the future.
fn elapsed(since: Timestamp) -> Duration {
    Duration::try_from(Timestamp::now().duration_since(since)).unwrap_or_default()
}

impl Control {
    /// Runs backups and restores until `shutdown` resolves. One runner per control plane.
    pub async fn run_backups(&self, shutdown: impl Future<Output = ()>) {
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            self.advance_backups().await;
            tokio::select! {
                () = shutdown.as_mut() => return,
                () = self.backups.wake.notified() => {}
                () = tokio::time::sleep(self.backups.timings.tick) => {}
            }
        }
    }

    pub(crate) fn wake_backups(&self) {
        self.backups.wake.notify_one();
    }

    /// The agent may have lost what it was sent: send it again on the next pass.
    pub(crate) async fn backups_agent_connected(&self, server: ServerId) {
        self.backups.sent.lock().await.retain(|_, (to, _)| *to != server);
        self.wake_backups();
    }

    pub(crate) async fn advance_backups(&self) {
        let mut sent = self.backups.sent.lock().await;
        self.queue_scheduled_backups().await;
        let (backups, restores) = match (self.store().list_active_backups().await, self.store().list_active_restores().await) {
            (Ok(backups), Ok(restores)) => (backups, restores),
            (Err(error), _) | (_, Err(error)) => {
                tracing::error!(%error, "failed to list the active backups and restores");
                return;
            }
        };
        let active: HashSet<String> = backups
            .iter()
            .map(|backup| RunId::Backup(backup.id).command_id())
            .chain(restores.iter().map(|restore| RunId::Restore(restore.id).command_id()))
            .collect();
        sent.retain(|id, _| active.contains(id));
        // Backups first, so a restore's safety backup goes out before it.
        for backup in backups {
            if let Err(error) = self.advance_backup(&mut sent, &backup).await {
                tracing::error!(backup = %backup.id, %error, "failed to advance a backup");
            }
        }
        for restore in restores {
            if let Err(error) = self.advance_restore(&mut sent, &restore).await {
                tracing::error!(restore = %restore.id, %error, "failed to advance a restore");
            }
        }
        drop(sent);
        self.apply_retention().await;
    }

    /// Queues a backup for each due schedule, and moves the schedule to its next run first,
    /// so a failure is not repeated on every pass.
    async fn queue_scheduled_backups(&self) {
        let now = Timestamp::now();
        let due = match self.store().list_due_backup_schedules(now).await {
            Ok(due) => due,
            Err(error) => {
                tracing::error!(%error, "failed to list the due backup schedules");
                return;
            }
        };
        for schedule in due {
            let next = cron(&schedule.fields.cron).ok().and_then(|cron| next_run(&cron, now));
            if let Err(error) = self.store().set_backup_schedule_next_run(schedule.id, next).await {
                tracing::error!(schedule = %schedule.id, %error, "failed to move a backup schedule on");
                continue;
            }
            let queued = match self.store().get_service(schedule.service_id).await {
                Ok(service) => self.queue_backup(&service, schedule.fields.destination_id, Some(schedule.id)).await,
                Err(error) => Err(error.into()),
            };
            match queued {
                Ok(backup) => tracing::info!(schedule = %schedule.id, backup = %backup.id, "queued a scheduled backup"),
                Err(error) => tracing::warn!(schedule = %schedule.id, %error, "could not queue a scheduled backup"),
            }
        }
    }

    async fn advance_backup(&self, sent: &mut Sent, backup: &Backup) -> Result<(), ControlError> {
        let run = Run {
            id: RunId::Backup(backup.id),
            status: backup.status,
            waiting_since: backup.created_at,
            started_at: backup.started_at,
            server_id: backup.server_id,
        };
        let destination = match self.store().get_storage_destination(backup.destination_id).await {
            Ok(destination) => Some(destination),
            Err(StoreError::NotFound { .. }) => None,
            Err(error) => return Err(error.into()),
        };
        let located = match &destination {
            Some(_) => self.locate_database(backup.service_id).await?,
            None => Located::Never("its storage destination was deleted".to_string()),
        };
        match self.next_step(sent, &run, located).await? {
            Next::Send { server, container_id, first } => {
                let destination = destination.expect("located only with a destination");
                let secret = self.store().storage_destination_secret(destination.id).await?;
                let command = pb::BackupDatabase {
                    backup_id: backup.id.to_string(),
                    container_id,
                    command: backup.engine.dump_command(),
                    destination: Some(super::storage::proto(&destination.fields, &secret)),
                    object_key: backup.object_key.clone(),
                };
                let started = |server| async move { self.store().start_backup(backup.id, server).await };
                self.send_run(sent, run.id, server, Kind::BackupDatabase(command), first.then_some(started)).await?;
            }
            Next::Fail(problem) => self.fail_backup(backup, &problem).await?,
            Next::Wait => {}
        }
        Ok(())
    }

    async fn advance_restore(&self, sent: &mut Sent, restore: &Restore) -> Result<(), ControlError> {
        let mut waiting_since = restore.created_at;
        // The safety backup comes first: wait for it, and give up if it failed.
        if restore.status == BackupStatus::Queued
            && let Some(safety) = restore.safety_backup_id
        {
            match self.store().get_backup(safety).await {
                Ok(safety) if safety.status == BackupStatus::Failed => {
                    let problem = format!("the safety backup failed: {}", safety.error.as_deref().unwrap_or("no reason"));
                    return self.fail_restore(restore, &problem).await;
                }
                Ok(safety) if !safety.status.is_finished() => return Ok(()),
                Ok(safety) => waiting_since = safety.finished_at.unwrap_or(waiting_since).max(waiting_since),
                // Only finished backups can be deleted.
                Err(StoreError::NotFound { .. }) => {}
                Err(error) => return Err(error.into()),
            }
        }
        // A database with nothing live yet, such as a new one restored into, is waited for
        // while its deployment runs, and counts as waiting from when it went live.
        if restore.status == BackupStatus::Queued {
            match self.store().live_deployment(restore.service_id).await? {
                Some(live) => waiting_since = waiting_since.max(live.updated_at),
                None => {
                    let newest = self.store().list_deployments(restore.service_id, None, 1).await?;
                    match newest.first() {
                        Some(deployment) if deployment.status.is_in_progress() => return Ok(()),
                        Some(deployment)
                            if matches!(deployment.status, DeploymentStatus::Failed | DeploymentStatus::Canceled) =>
                        {
                            let problem = format!("the database's deployment {} ended as {}", deployment.id, deployment.status);
                            return self.fail_restore(restore, &problem).await;
                        }
                        // Left to `locate_database`, which explains it.
                        _ => {}
                    }
                }
            }
        }
        let run = Run {
            id: RunId::Restore(restore.id),
            status: restore.status,
            waiting_since,
            started_at: restore.started_at,
            server_id: restore.server_id,
        };
        let backup = match self.store().get_backup(restore.backup_id).await {
            Ok(backup) => Some(backup),
            Err(StoreError::NotFound { .. }) => None,
            Err(error) => return Err(error.into()),
        };
        let located = match &backup {
            None => Located::Never("the backup was deleted".to_string()),
            Some(_) => self.locate_database(restore.service_id).await?,
        };
        match self.next_step(sent, &run, located).await? {
            Next::Send { server, container_id, first } => {
                let backup = backup.expect("located with a backup");
                let plan = backup.engine.restore_plan();
                // A deleted destination still reaches the backups it holds.
                let destination = self.store().get_storage_destination_any(backup.destination_id).await?;
                let secret = self.store().storage_destination_secret(destination.id).await?;
                let command = pb::RestoreDatabase {
                    restore_id: restore.id.to_string(),
                    container_id,
                    command: plan.command,
                    destination: Some(super::storage::proto(&destination.fields, &secret)),
                    object_key: backup.object_key,
                    restart: plan.restart,
                    ready_command: plan.ready_command,
                };
                let started = |server| async move { self.store().start_restore(restore.id, server).await };
                self.send_run(sent, run.id, server, Kind::RestoreDatabase(command), first.then_some(started)).await?;
            }
            Next::Fail(problem) => self.fail_restore(restore, &problem).await?,
            Next::Wait => {}
        }
        Ok(())
    }

    /// Whether to send a run's command now, fail it, or wait: a queued run goes as soon as
    /// its database can be reached, and fails if it can't for too long; a running one is sent
    /// again while unanswered, and fails if it runs too long or its agent stays away.
    async fn next_step(&self, sent: &Sent, run: &Run, located: Located) -> Result<Next, ControlError> {
        let timings = self.backups.timings;
        let started = self.backups.started_at;
        Ok(match run.status {
            BackupStatus::Queued => match located {
                Located::Ready { server, container_id } => Next::Send { server, container_id, first: true },
                Located::Waiting(problem) if elapsed(run.waiting_since.max(started)) > timings.agent_lost_after => {
                    Next::Fail(problem)
                }
                Located::Waiting(_) => Next::Wait,
                Located::Never(problem) => Next::Fail(problem),
            },
            BackupStatus::Running => {
                let running_since = run.started_at.unwrap_or(run.waiting_since).max(started);
                if elapsed(running_since) > timings.run_timeout {
                    return Ok(Next::Fail(format!("it ran for more than {}s", timings.run_timeout.as_secs())));
                }
                if sent.get(&run.id.command_id()).is_some_and(|(_, at)| at.elapsed() < timings.resend_after) {
                    return Ok(Next::Wait);
                }
                let Some(server) = run.server_id else {
                    return Ok(Next::Fail("it has no server".to_string()));
                };
                match located {
                    Located::Ready { server: target, container_id } if target == server => {
                        Next::Send { server, container_id, first: false }
                    }
                    Located::Ready { .. } => Next::Fail("its database moved to another server".to_string()),
                    Located::Waiting(problem) => {
                        let seen = self.store().get_server(server).await?.last_seen_at;
                        let since = seen.map_or(running_since, |seen| seen.max(running_since));
                        if elapsed(since) > timings.agent_lost_after { Next::Fail(problem) } else { Next::Wait }
                    }
                    Located::Never(problem) => Next::Fail(problem),
                }
            }
            BackupStatus::Succeeded | BackupStatus::Failed => Next::Wait,
        })
    }

    /// The server and container of the database's live deployment.
    async fn locate_database(&self, service_id: ServiceId) -> Result<Located, ControlError> {
        match self.store().get_service(service_id).await {
            Ok(service) if service.stopped_at.is_some() => return Ok(Located::Never("the database is stopped".to_string())),
            Ok(_) => {}
            Err(StoreError::NotFound { .. }) => return Ok(Located::Never("its database was deleted".to_string())),
            Err(error) => return Err(error.into()),
        }
        let Some(deployment) = self.store().live_deployment(service_id).await? else {
            return Ok(Located::Never("the database has nothing live".to_string()));
        };
        let server = self.store().get_server(deployment.server_id).await?;
        if server.revoked_at.is_some() {
            return Ok(Located::Never(format!("server `{}` was revoked", server.name)));
        }
        if !self.agents().is_connected(server.id) {
            return Ok(Located::Waiting(format!("the agent of server `{}` is not connected", server.name)));
        }
        let instances = self.store().list_server_instances(server.id).await?;
        let Some(container) = instances.into_iter().find(|instance| instance.observed.deployment_id == deployment.id) else {
            return Ok(Located::Waiting("the database's container has not been reported".to_string()));
        };
        Ok(Located::Ready { server: server.id, container_id: container.observed.container_id })
    }

    /// Sends a run's command, then records it as sent; a first send also starts the run with
    /// `start`, and counts only if the run was still queued.
    async fn send_run<F, Fut>(
        &self,
        sent: &mut Sent,
        id: RunId,
        server: ServerId,
        kind: Kind,
        start: Option<F>,
    ) -> Result<(), ControlError>
    where
        F: FnOnce(ServerId) -> Fut,
        Fut: Future<Output = Result<bool, StoreError>>,
    {
        let command_id = id.command_id();
        let command = pb::Command { command_id: command_id.clone(), kind: Some(kind) };
        let message = pb::ControlMessage { message: Some(control_message::Message::Command(command)) };
        if self.agents().send(server, message).await.is_err() {
            return Ok(());
        }
        tracing::debug!(server = %server, command = command_id, "sent a command");
        if let Some(start) = start
            && !start(server).await?
        {
            return Ok(());
        }
        sent.insert(command_id, (server, Instant::now()));
        Ok(())
    }

    async fn fail_backup(&self, backup: &Backup, problem: &str) -> Result<(), ControlError> {
        tracing::warn!(backup = %backup.id, problem, "backup failed");
        if self.store().finish_backup(backup.id, Err(problem.to_string())).await? {
            self.notify_backup_failed(backup, problem).await;
        }
        Ok(())
    }

    async fn notify_backup_failed(&self, backup: &Backup, problem: &str) {
        let (subject, mut details) = self.service_subject(backup.service_id).await;
        details.insert("backup_id".to_string(), backup.id.to_string());
        let what = if backup.schedule_id.is_some() { "A scheduled backup" } else { "A backup" };
        self.notify(spinikus_core::notification::Message {
            event: spinikus_core::notification::NotificationEvent::BackupFailed,
            title: format!("{what} of {subject} failed"),
            text: problem.to_string(),
            path: Some(format!("/services/{}/backups", backup.service_id)),
            details,
        })
        .await;
    }

    async fn fail_restore(&self, restore: &Restore, problem: &str) -> Result<(), ControlError> {
        tracing::warn!(restore = %restore.id, problem, "restore failed");
        self.store().finish_restore(restore.id, Err(problem.to_string())).await?;
        Ok(())
    }

    /// A backup or restore command's result from an agent. It only counts from the server
    /// the run started on.
    pub(crate) async fn backup_result(&self, server: ServerId, result: pb::CommandResult) {
        let Some(id) = parse_command_id(&result.command_id) else {
            return;
        };
        let mut sent = self.backups.sent.lock().await;
        let ran_on = match id {
            RunId::Backup(id) => self.store().get_backup(id).await.map(|backup| backup.server_id),
            RunId::Restore(id) => self.store().get_restore(id).await.map(|restore| restore.server_id),
        };
        match ran_on {
            Ok(ran_on) if ran_on == Some(server) => {}
            Ok(_) => {
                tracing::warn!(server = %server, command = result.command_id, "ignored a result from another server");
                return;
            }
            Err(StoreError::NotFound { .. }) => return,
            Err(error) => {
                tracing::error!(command = result.command_id, %error, "failed to apply a result");
                return;
            }
        }
        sent.remove(&result.command_id);
        let outcome = match result.outcome {
            Some(Outcome::Succeeded(succeeded)) => Ok(succeeded),
            Some(Outcome::Failed(failed)) => Err(failed.message),
            None => Err("the agent sent no outcome".to_string()),
        };
        let recorded = match id {
            RunId::Backup(id) => {
                let outcome = outcome.map(|succeeded| succeeded.backup.map_or(0, |backup| backup.size_bytes));
                match &outcome {
                    Ok(size) => tracing::info!(backup = %id, size, "backup succeeded"),
                    Err(problem) => tracing::warn!(backup = %id, problem, "backup failed"),
                }
                let problem = outcome.as_ref().err().cloned();
                let finished = self.store().finish_backup(id, outcome).await;
                if let (Ok(true), Some(problem)) = (&finished, problem)
                    && let Ok(backup) = self.store().get_backup(id).await
                {
                    self.notify_backup_failed(&backup, &problem).await;
                }
                finished.map(|_| ())
            }
            RunId::Restore(id) => {
                let outcome = outcome.map(|_| ());
                match &outcome {
                    Ok(()) => tracing::info!(restore = %id, "restore succeeded"),
                    Err(problem) => tracing::warn!(restore = %id, problem, "restore failed"),
                }
                self.store().finish_restore(id, outcome).await.map(|_| ())
            }
        };
        if let Err(error) = recorded {
            tracing::error!(command = result.command_id, %error, "failed to record a result");
        }
        drop(sent);
        self.wake_backups();
    }

    /// Deletes each schedule's successful backups beyond its newest `keep_last`. One that
    /// can't be deleted is tried again on the next pass.
    async fn apply_retention(&self) {
        let schedules = match self.store().list_all_backup_schedules().await {
            Ok(schedules) => schedules,
            Err(error) => {
                tracing::error!(%error, "failed to list the backup schedules");
                return;
            }
        };
        for schedule in schedules {
            let beyond = match self.store().list_backups_beyond(schedule.id, schedule.fields.keep_last).await {
                Ok(beyond) => beyond,
                Err(error) => {
                    tracing::error!(schedule = %schedule.id, %error, "failed to list a schedule's old backups");
                    continue;
                }
            };
            for backup in beyond {
                match self.remove_backup(&backup).await {
                    Ok(()) => tracing::info!(schedule = %schedule.id, backup = %backup.id, "deleted an old backup"),
                    Err(error) => tracing::warn!(backup = %backup.id, %error, "could not delete an old backup"),
                }
            }
        }
    }
}
