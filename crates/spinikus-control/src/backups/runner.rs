//! The backup runner: turns due schedules into queued backups, sends each queued backup to
//! the agent of the server its database is live on, and applies the schedules' retention.
//!
//! The database is the state, like for deployments: a queued backup is sent when its
//! database's agent is connected (and fails when the database has nothing live, or the
//! agent stays away too long), a running one is sent again while unanswered, and fails after
//! `run_timeout`. The command ID is `<backup ID>/backup`, so the agent answers a repeat with
//! the result it already has. A result only counts from the server the backup runs on.
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
    backup::{Backup, BackupStatus},
    id::{BackupId, ServerId},
};
use spinikus_proto::agent::v1::{self as pb, command::Kind, command_result::Outcome, control_message};
use spinikus_store::{
    BackupRepository, BackupScheduleRepository, DeploymentRepository, InstanceRepository, ServerRepository, ServiceRepository,
    StorageDestinationRepository, StoreError,
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
    /// When each running backup's command was last sent, and to which server. Held across a
    /// whole pass or result, so they never interleave.
    sent: Mutex<HashMap<BackupId, (ServerId, Instant)>>,
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

const STEP: &str = "backup";

fn command_id(backup: BackupId) -> String {
    format!("{backup}/{STEP}")
}

/// The backup a command ID names, or `None` if it is not a backup's.
pub(crate) fn parse_command_id(id: &str) -> Option<BackupId> {
    match id.split_once('/') {
        Some((backup, STEP)) => backup.parse().ok(),
        _ => None,
    }
}

/// Where a backup can run, or why it can't yet or ever.
enum Target {
    Ready {
        server: ServerId,
        command: Box<pb::BackupDatabase>,
    },
    /// It may become ready: the agent is away, or the container not reported yet.
    Waiting(String),
    Never(String),
}

/// How long ago `since` was, zero if in the future.
fn elapsed(since: Timestamp) -> Duration {
    Duration::try_from(Timestamp::now().duration_since(since)).unwrap_or_default()
}

impl Control {
    /// Runs backups until `shutdown` resolves. One runner per control plane.
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
        let active = match self.store().list_active_backups().await {
            Ok(active) => active,
            Err(error) => {
                tracing::error!(%error, "failed to list the active backups");
                return;
            }
        };
        let ids: HashSet<BackupId> = active.iter().map(|backup| backup.id).collect();
        sent.retain(|id, _| ids.contains(id));
        for backup in active {
            if let Err(error) = self.advance_backup(&mut sent, &backup).await {
                tracing::error!(backup = %backup.id, %error, "failed to advance a backup");
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

    async fn advance_backup(
        &self,
        sent: &mut HashMap<BackupId, (ServerId, Instant)>,
        backup: &Backup,
    ) -> Result<(), ControlError> {
        let timings = self.backups.timings;
        let started = self.backups.started_at;
        match backup.status {
            BackupStatus::Queued => match self.backup_target(backup).await? {
                Target::Ready { server, command } => {
                    if self.send_backup(server, *command).await && self.store().start_backup(backup.id, server).await? {
                        sent.insert(backup.id, (server, Instant::now()));
                    }
                }
                Target::Waiting(problem) if elapsed(backup.created_at.max(started)) > timings.agent_lost_after => {
                    self.fail_backup(backup, &problem).await?;
                }
                Target::Waiting(_) => {}
                Target::Never(problem) => self.fail_backup(backup, &problem).await?,
            },
            BackupStatus::Running => {
                let running_since = backup.started_at.unwrap_or(backup.created_at).max(started);
                if elapsed(running_since) > timings.run_timeout {
                    let problem = format!("it ran for more than {}s", timings.run_timeout.as_secs());
                    return self.fail_backup(backup, &problem).await;
                }
                if sent.get(&backup.id).is_some_and(|(_, at)| at.elapsed() < timings.resend_after) {
                    return Ok(());
                }
                let Some(server) = backup.server_id else {
                    return self.fail_backup(backup, "it has no server").await;
                };
                match self.backup_target(backup).await? {
                    Target::Ready { server: target, command } if target == server => {
                        if self.send_backup(server, *command).await {
                            sent.insert(backup.id, (server, Instant::now()));
                        }
                    }
                    Target::Ready { .. } => self.fail_backup(backup, "its database moved to another server").await?,
                    Target::Waiting(problem) => {
                        let seen = self.store().get_server(server).await?.last_seen_at;
                        let since = seen.map_or(running_since, |seen| seen.max(running_since));
                        if elapsed(since) > timings.agent_lost_after {
                            self.fail_backup(backup, &problem).await?;
                        }
                    }
                    Target::Never(problem) => self.fail_backup(backup, &problem).await?,
                }
            }
            BackupStatus::Succeeded | BackupStatus::Failed => {}
        }
        Ok(())
    }

    /// The command for the backup, and the server to send it to.
    async fn backup_target(&self, backup: &Backup) -> Result<Target, ControlError> {
        let service = match self.store().get_service(backup.service_id).await {
            Ok(service) => service,
            Err(StoreError::NotFound { .. }) => return Ok(Target::Never("its database was deleted".to_string())),
            Err(error) => return Err(error.into()),
        };
        let Some(deployment) = self.store().live_deployment(service.id).await? else {
            return Ok(Target::Never("the database has nothing live".to_string()));
        };
        let server = self.store().get_server(deployment.server_id).await?;
        if server.revoked_at.is_some() {
            return Ok(Target::Never(format!("server `{}` was revoked", server.name)));
        }
        let destination = match self.store().get_storage_destination(backup.destination_id).await {
            Ok(destination) => destination,
            Err(StoreError::NotFound { .. }) => return Ok(Target::Never("its storage destination was deleted".to_string())),
            Err(error) => return Err(error.into()),
        };
        if !self.agents().is_connected(server.id) {
            return Ok(Target::Waiting(format!("the agent of server `{}` is not connected", server.name)));
        }
        let instances = self.store().list_server_instances(server.id).await?;
        let Some(container) = instances.into_iter().find(|instance| instance.observed.deployment_id == deployment.id) else {
            return Ok(Target::Waiting("the database's container has not been reported".to_string()));
        };
        let secret = self.store().storage_destination_secret(destination.id).await?;
        let command = pb::BackupDatabase {
            backup_id: backup.id.to_string(),
            container_id: container.observed.container_id,
            command: backup.engine.dump_command(),
            destination: Some(super::storage::proto(&destination.fields, &secret)),
            object_key: backup.object_key.clone(),
        };
        Ok(Target::Ready { server: server.id, command: Box::new(command) })
    }

    async fn send_backup(&self, server: ServerId, command: pb::BackupDatabase) -> bool {
        let id = command_id(command.backup_id.parse().expect("made from a backup ID"));
        let command = pb::Command { command_id: id.clone(), kind: Some(Kind::BackupDatabase(command)) };
        let message = pb::ControlMessage { message: Some(control_message::Message::Command(command)) };
        let sent = self.agents().send(server, message).await.is_ok();
        if sent {
            tracing::debug!(server = %server, command = id, "sent a backup command");
        }
        sent
    }

    async fn fail_backup(&self, backup: &Backup, problem: &str) -> Result<(), ControlError> {
        tracing::warn!(backup = %backup.id, problem, "backup failed");
        self.store().finish_backup(backup.id, Err(problem.to_string())).await?;
        Ok(())
    }

    /// A backup command's result from an agent.
    pub(crate) async fn backup_result(&self, server: ServerId, result: pb::CommandResult) {
        let Some(id) = parse_command_id(&result.command_id) else {
            return;
        };
        let mut sent = self.backups.sent.lock().await;
        let backup = match self.store().get_backup(id).await {
            Ok(backup) => backup,
            Err(StoreError::NotFound { .. }) => return,
            Err(error) => {
                tracing::error!(backup = %id, %error, "failed to apply a backup's result");
                return;
            }
        };
        if backup.server_id != Some(server) {
            tracing::warn!(server = %server, backup = %id, "ignored a result for a backup on another server");
            return;
        }
        sent.remove(&id);
        let outcome = match result.outcome {
            Some(Outcome::Succeeded(succeeded)) => Ok(succeeded.backup.map_or(0, |backup| backup.size_bytes)),
            Some(Outcome::Failed(failed)) => Err(failed.message),
            None => Err("the agent sent no outcome".to_string()),
        };
        match &outcome {
            Ok(size) => tracing::info!(backup = %id, size, "backup succeeded"),
            Err(problem) => tracing::warn!(backup = %id, problem, "backup failed"),
        }
        if let Err(error) = self.store().finish_backup(id, outcome).await {
            tracing::error!(backup = %id, %error, "failed to record a backup's result");
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
