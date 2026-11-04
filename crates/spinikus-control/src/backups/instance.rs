//! Backups of the instance itself, on a schedule or by hand, to a storage destination.
//!
//! Instance admins set the one schedule (destination, cron, how many to keep, and the
//! passphrase, which is write-only), back up now, list the backups and delete them. The
//! control plane makes each backup itself, since it has the database and the master key: a
//! file in the format of `spinikus admin backup` (`spinikus_store::instance_backup`), written
//! in a work directory and then uploaded. One runs at a time, oldest first. A backup that was
//! running when the control plane stopped fails when it starts again.

use std::{path::PathBuf, sync::Arc, time::Duration};

use jiff::Timestamp;
use object_store::{ObjectStore, ObjectStoreExt, WriteMultipart, path::Path};
use serde_json::json;
use spinikus_core::{
    backup::BackupStatus,
    id::{InstanceBackupId, StorageDestinationId},
    instance_backup::{InstanceBackup, InstanceBackupSchedule, InstanceBackupScheduleFields, NewInstanceBackup, object_key},
    secret::Secret,
};
use spinikus_store::{InstanceBackupRepository, MasterKey, StorageDestinationRepository, StoreError, instance_backup};
use tokio::{io::AsyncReadExt, sync::Notify};

use super::{DEFAULT_KEEP_LAST, cron, keep_last, next_run, require_instance_admin};
use crate::{
    Control, ControlError, Principal,
    page::{self, Page},
};

/// The shortest passphrase a schedule takes, as for `spinikus admin backup`.
pub const MIN_PASSPHRASE_LEN: usize = 12;

/// How often the runner looks for a due schedule, besides when woken.
const TICK: Duration = Duration::from_secs(30);

/// How much of the file each upload part carries.
const PART_SIZE: usize = 8 * 1024 * 1024;

/// What the runner needs that the rest of the control plane doesn't.
pub(crate) struct InstanceBackups {
    master_key: Option<MasterKey>,
    work_dir: PathBuf,
    wake: Notify,
}

impl InstanceBackups {
    pub(crate) fn new(master_key: Option<MasterKey>, work_dir: PathBuf) -> Self {
        Self { master_key, work_dir, wake: Notify::new() }
    }
}

impl Default for InstanceBackups {
    fn default() -> Self {
        Self::new(None, std::env::temp_dir())
    }
}

/// The schedule as an instance admin sets it.
#[derive(Debug, Clone)]
pub struct InstanceBackupScheduleInput {
    pub destination_id: StorageDestinationId,
    pub cron: String,
    /// [`DEFAULT_KEEP_LAST`] when `None`.
    pub keep_last: Option<u32>,
    pub enabled: bool,
    /// Required for a new schedule; `None` keeps the current one.
    pub passphrase: Option<Secret>,
}

impl Control {
    /// The schedule, if one was set. Instance admins only.
    pub async fn get_instance_backup_schedule(
        &self,
        principal: &Principal,
    ) -> Result<Option<InstanceBackupSchedule>, ControlError> {
        require_instance_admin(principal)?;
        Ok(self.store().get_instance_backup_schedule().await?)
    }

    /// Sets the schedule if it is still at `revision` (0 for one never set).
    pub async fn set_instance_backup_schedule(
        &self,
        principal: &Principal,
        revision: i64,
        input: InstanceBackupScheduleInput,
    ) -> Result<InstanceBackupSchedule, ControlError> {
        require_instance_admin(principal)?;
        let cron = cron(&input.cron)?;
        let passphrase = input.passphrase.filter(|passphrase| !passphrase.expose().is_empty());
        match &passphrase {
            Some(passphrase) if passphrase.expose().chars().count() < MIN_PASSPHRASE_LEN => {
                return Err(ControlError::Invalid {
                    field: "passphrase",
                    problem: format!("must be at least {MIN_PASSPHRASE_LEN} characters long"),
                });
            }
            None if revision == 0 => {
                return Err(ControlError::Invalid {
                    field: "passphrase",
                    problem: "is required: backups are encrypted with it".to_string(),
                });
            }
            _ => {}
        }
        let fields = InstanceBackupScheduleFields {
            destination_id: input.destination_id,
            cron: cron.pattern.to_string(),
            keep_last: keep_last(input.keep_last.unwrap_or(DEFAULT_KEEP_LAST))?,
            enabled: input.enabled,
        };
        let next_run_at = fields.enabled.then(|| next_run(&cron, Timestamp::now())).flatten();
        let schedule = match self.store().put_instance_backup_schedule(revision, fields, passphrase.as_ref(), next_run_at).await {
            Err(StoreError::NotFound { entity: "storage destination", id }) => {
                return Err(ControlError::Invalid {
                    field: "destination_id",
                    problem: format!("there is no storage destination `{id}`"),
                });
            }
            result => result?,
        };
        let details = json!({
            "destination_id": schedule.fields.destination_id.to_string(),
            "cron": schedule.fields.cron,
            "keep_last": schedule.fields.keep_last,
            "enabled": schedule.fields.enabled,
            "passphrase_changed": passphrase.is_some(),
        });
        self.audit_by(principal, "instance_backup_schedule.update", "instance", None, details).await?;
        self.wake_instance_backups();
        Ok(schedule)
    }

    /// Queues a backup now, to the schedule's destination with its passphrase.
    pub async fn back_up_instance(&self, principal: &Principal) -> Result<InstanceBackup, ControlError> {
        require_instance_admin(principal)?;
        let backup = self.queue_instance_backup(false).await?;
        let details = json!({ "object_key": backup.object_key });
        self.audit_by(principal, "instance_backup.create", backup.id, None, details).await?;
        self.wake_instance_backups();
        Ok(backup)
    }

    async fn queue_instance_backup(&self, scheduled: bool) -> Result<InstanceBackup, ControlError> {
        let Some(schedule) = self.store().get_instance_backup_schedule().await? else {
            return Err(ControlError::Rejected {
                entity: "instance backup",
                id: String::new(),
                problem: "set up instance backups first: where they go and their passphrase".to_string(),
            });
        };
        let destination = self.store().get_storage_destination(schedule.fields.destination_id).await?;
        let id = InstanceBackupId::new();
        let new = NewInstanceBackup {
            id,
            destination_id: destination.id,
            scheduled,
            object_key: object_key(&destination.fields.path_prefix, id),
        };
        Ok(self.store().create_instance_backup(new).await?)
    }

    pub async fn list_instance_backups(
        &self,
        principal: &Principal,
        after: Option<InstanceBackupId>,
        limit: u32,
    ) -> Result<Page<InstanceBackup>, ControlError> {
        require_instance_admin(principal)?;
        let limit = page::limit(limit);
        let items = self.store().list_instance_backups(after, limit + 1).await?;
        Ok(Page::from_overfetch(items, limit))
    }

    pub async fn get_instance_backup(&self, principal: &Principal, id: InstanceBackupId) -> Result<InstanceBackup, ControlError> {
        require_instance_admin(principal)?;
        Ok(self.store().get_instance_backup(id).await?)
    }

    /// Deletes a finished backup and its file.
    pub async fn delete_instance_backup(&self, principal: &Principal, id: InstanceBackupId) -> Result<(), ControlError> {
        require_instance_admin(principal)?;
        let backup = self.store().get_instance_backup(id).await?;
        if !backup.status.is_finished() {
            return Err(ControlError::Rejected {
                entity: "instance backup",
                id: id.to_string(),
                problem: "it hasn't finished yet".to_string(),
            });
        }
        self.remove_instance_backup(&backup).await?;
        let details = json!({ "object_key": backup.object_key });
        self.audit_by(principal, "instance_backup.delete", id, None, details).await
    }

    async fn remove_instance_backup(&self, backup: &InstanceBackup) -> Result<(), ControlError> {
        // A failed backup's upload was aborted, or never started.
        if backup.status == BackupStatus::Succeeded {
            let destination = self.store().get_storage_destination_any(backup.destination_id).await?;
            let store = self.destination_store(&destination).await?;
            let path =
                Path::parse(&backup.object_key).map_err(|error| ControlError::internal("reading a backup's key", error))?;
            match store.delete(&path).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                Err(error) => {
                    return Err(ControlError::Rejected {
                        entity: "instance backup",
                        id: backup.id.to_string(),
                        problem: format!("could not delete its file: {error}"),
                    });
                }
            }
        }
        Ok(self.store().mark_instance_backup_deleted(backup.id).await?)
    }

    /// Makes instance backups until `shutdown` resolves. One runner per control plane.
    pub async fn run_instance_backups(&self, shutdown: impl Future<Output = ()>) {
        self.fail_interrupted_instance_backups().await;
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            // A backup cut short by the shutdown fails on the next start.
            tokio::select! {
                () = shutdown.as_mut() => return,
                () = self.advance_instance_backups() => {}
            }
            tokio::select! {
                () = shutdown.as_mut() => return,
                () = self.instance_backups.wake.notified() => {}
                () = tokio::time::sleep(TICK) => {}
            }
        }
    }

    async fn notify_instance_backup_failed(&self, backup: &InstanceBackup, problem: &str) {
        let what = if backup.scheduled { "A scheduled backup of Spinikus failed" } else { "A backup of Spinikus failed" };
        self.notify(spinikus_core::notification::Message {
            event: spinikus_core::notification::NotificationEvent::InstanceBackupFailed,
            title: what.to_string(),
            text: problem.to_string(),
            path: Some("/settings/backups".to_string()),
            details: std::collections::BTreeMap::from([("instance_backup_id".to_string(), backup.id.to_string())]),
        })
        .await;
    }

    fn wake_instance_backups(&self) {
        self.instance_backups.wake.notify_one();
    }

    /// Backups left running by a control plane that stopped can't finish: they fail.
    async fn fail_interrupted_instance_backups(&self) {
        let active = match self.store().list_active_instance_backups().await {
            Ok(active) => active,
            Err(error) => return tracing::error!(%error, "could not list instance backups"),
        };
        for backup in active.iter().filter(|backup| backup.status == BackupStatus::Running) {
            let problem = "Spinikus stopped while it ran";
            match self.store().finish_instance_backup(backup.id, Err(problem.to_string())).await {
                Ok(true) => self.notify_instance_backup_failed(backup, problem).await,
                Ok(false) => {}
                Err(error) => tracing::error!(backup = %backup.id, %error, "could not record an interrupted instance backup"),
            }
        }
    }

    /// One pass: queues a backup if the schedule is due, then makes every queued backup, one
    /// at a time, and applies the schedule's retention. Tests call it directly.
    pub async fn advance_instance_backups(&self) {
        if let Err(error) = self.queue_due_instance_backup().await {
            tracing::error!(%error, "could not queue the scheduled instance backup");
        }
        let queued = match self.store().list_active_instance_backups().await {
            Ok(active) => active.into_iter().filter(|backup| backup.status == BackupStatus::Queued),
            Err(error) => return tracing::error!(%error, "could not list instance backups"),
        };
        for backup in queued {
            self.make_instance_backup(&backup).await;
        }
    }

    async fn queue_due_instance_backup(&self) -> Result<(), ControlError> {
        let Some(schedule) = self.store().get_instance_backup_schedule().await? else { return Ok(()) };
        let now = Timestamp::now();
        if !schedule.fields.enabled || schedule.next_run_at.is_none_or(|due| due > now) {
            return Ok(());
        }
        // Moved on first, so a failure never makes it run on every pass; missed runs are one.
        let cron = cron(&schedule.fields.cron)?;
        self.store().set_instance_backup_next_run(next_run(&cron, now)).await?;
        let active = self.store().list_active_instance_backups().await?;
        if active.iter().any(|backup| backup.scheduled) {
            tracing::warn!("skipping a scheduled instance backup: the previous one hasn't finished");
            return Ok(());
        }
        let backup = self.queue_instance_backup(true).await?;
        tracing::info!(backup = %backup.id, "queued the scheduled instance backup");
        Ok(())
    }

    async fn make_instance_backup(&self, backup: &InstanceBackup) {
        match self.store().start_instance_backup(backup.id).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => return tracing::error!(backup = %backup.id, %error, "could not start an instance backup"),
        }
        let outcome = self.write_instance_backup(backup).await;
        match &outcome {
            Ok(size) => tracing::info!(backup = %backup.id, size, key = backup.object_key, "backed up the instance"),
            Err(error) => tracing::warn!(backup = %backup.id, %error, "an instance backup failed"),
        }
        let problem = outcome.as_ref().err().cloned();
        match self.store().finish_instance_backup(backup.id, outcome).await {
            Ok(finished) => {
                if let (true, Some(problem)) = (finished, problem) {
                    self.notify_instance_backup_failed(backup, &problem).await;
                }
            }
            Err(error) => return tracing::error!(backup = %backup.id, %error, "could not record an instance backup"),
        }
        if backup.scheduled
            && let Err(error) = self.apply_instance_backup_retention().await
        {
            tracing::warn!(%error, "could not delete old instance backups");
        }
    }

    /// Writes the backup file and uploads it; its size, or why it failed.
    async fn write_instance_backup(&self, backup: &InstanceBackup) -> Result<u64, String> {
        let Some(master_key) = &self.instance_backups.master_key else {
            return Err("this control plane can't back up the instance: it has no master key to include".to_string());
        };
        let destination = match self.store().get_storage_destination(backup.destination_id).await {
            Ok(destination) => destination,
            Err(StoreError::NotFound { .. }) => return Err("its storage destination was deleted".to_string()),
            Err(error) => return Err(format!("could not read its storage destination: {error}")),
        };
        let passphrase =
            self.store().instance_backup_passphrase().await.map_err(|error| format!("could not read the passphrase: {error}"))?;
        let store = self.destination_store(&destination).await.map_err(|error| error.to_string())?;

        let work_dir = &self.instance_backups.work_dir;
        tokio::fs::create_dir_all(work_dir).await.map_err(|error| format!("could not create {}: {error}", work_dir.display()))?;
        let work = tempfile::Builder::new()
            .prefix(".spinikus-instance-backup-")
            .tempdir_in(work_dir)
            .map_err(|error| format!("could not create a work directory in {}: {error}", work_dir.display()))?;
        let file = work.path().join("backup.tar.gz.age");
        instance_backup::create_backup(self.store().pool(), master_key, env!("CARGO_PKG_VERSION"), &passphrase, &file)
            .await
            .map_err(|error| format!("could not write the backup: {error}"))?;
        upload(store, &backup.object_key, &file).await
    }

    /// Deletes the schedule's successful backups beyond what it keeps.
    async fn apply_instance_backup_retention(&self) -> Result<(), ControlError> {
        let Some(schedule) = self.store().get_instance_backup_schedule().await? else { return Ok(()) };
        for old in self.store().list_instance_backups_beyond(schedule.fields.keep_last).await? {
            self.remove_instance_backup(&old).await?;
            tracing::info!(backup = %old.id, key = old.object_key, "deleted an old instance backup");
        }
        Ok(())
    }
}

/// Uploads the file to `key` in parts; its size.
async fn upload(store: Arc<dyn ObjectStore>, key: &str, file: &std::path::Path) -> Result<u64, String> {
    let path = Path::parse(key).map_err(|error| format!("invalid object key `{key}`: {error}"))?;
    let mut reader = tokio::fs::File::open(file).await.map_err(|error| format!("could not read the backup: {error}"))?;
    let upload = store.put_multipart(&path).await.map_err(|error| format!("could not start the upload: {error}"))?;
    let mut upload = WriteMultipart::new_with_chunk_size(upload, PART_SIZE);
    let mut buffer = vec![0; PART_SIZE];
    let mut size = 0u64;
    let read = loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break Ok(()),
            Ok(read) => {
                size += read as u64;
                upload.write(&buffer[..read]);
                if let Err(error) = upload.wait_for_capacity(2).await {
                    break Err(format!("the upload failed: {error}"));
                }
            }
            Err(error) => break Err(format!("could not read the backup: {error}")),
        }
    };
    match read {
        Ok(()) => {
            upload.finish().await.map_err(|error| format!("the upload failed: {error}"))?;
            Ok(size)
        }
        Err(error) => {
            if let Err(abort) = upload.abort().await {
                tracing::warn!(error = %abort, "could not abort an instance backup's upload");
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests;
