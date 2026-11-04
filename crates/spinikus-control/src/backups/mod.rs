//! Backups of databases and of other services' volumes to S3-compatible storage.
//!
//! Storage destinations belong to the instance: instance admins add, change and delete them,
//! and anyone signed in sees them (never the secret access key), to pick one. Backups are
//! made by the agent of the service's server, which dumps a database inside its container,
//! or archives one volume of any other service in a helper container, and uploads the result
//! straight to the destination (`runner.rs`); the control plane only sends the command, with
//! the destination's credentials, and records the result. A database has one kind of backup,
//! its dump; its data volume isn't archived, since a copy of a running database's files may
//! not open.
//!
//! Admins of the service's environment restore its backups (`restores.rs`), after a safety
//! backup of the data the restore replaces.
//!
//! Admins of the service's environment manage its schedules (a cron expression in UTC, and
//! how many of its own successful backups to keep, of each volume); each run backs up the
//! database, or each volume. Developers make a backup by hand, viewers list them, and admins
//! delete them, the object included.

pub mod instance;
mod restores;
mod runner;
pub(crate) mod storage;

use std::sync::Arc;

use croner::{Cron, parser::CronParser};
use jiff::{Timestamp, tz::TimeZone};
use object_store::{ObjectStore, ObjectStoreExt, PutPayload, path::Path};
pub use runner::BackupTimings;
pub(crate) use runner::Backups;
use serde_json::json;

/// Whether a command ID is a backup's or a restore's.
pub(crate) fn is_backup_command(id: &str) -> bool {
    runner::parse_command_id(id).is_some()
}
use spinikus_core::{
    backup::{
        Backup, BackupSchedule, BackupScheduleFields, BackupSource, MAX_KEEP_LAST, NewBackup, StorageDestination,
        StorageDestinationFields, object_key,
    },
    database::Engine,
    environment::Environment,
    id::{BackupId, BackupScheduleId, ServiceId, StorageDestinationId, VolumeId},
    membership::Role,
    secret::Secret,
    service::Service,
};
use spinikus_store::{
    BackupRepository, BackupScheduleRepository, RestoreRepository, StorageDestinationRepository, VolumeRepository,
};

use crate::{
    Control, ControlError, Principal,
    access::not_found,
    audit,
    page::{self, Page},
};

/// How many backups a schedule keeps when it does not say.
pub const DEFAULT_KEEP_LAST: u32 = 7;

const MAX_NAME_LEN: usize = 100;
const MAX_PATH_PREFIX_LEN: usize = 512;
const MAX_CREDENTIAL_LEN: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewStorageDestination {
    pub name: String,
    /// The S3 API's URL; AWS if `None`.
    pub endpoint: Option<String>,
    /// `us-east-1` if `None`.
    pub region: Option<String>,
    pub bucket: String,
    pub path_prefix: Option<String>,
    pub path_style: bool,
    pub access_key_id: String,
    pub secret_access_key: Secret,
}

/// Changes to a destination; `None` keeps the current value. `endpoint: Some(None)` goes
/// back to AWS.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StorageDestinationChanges {
    pub name: Option<String>,
    pub endpoint: Option<Option<String>>,
    pub region: Option<String>,
    pub bucket: Option<String>,
    pub path_prefix: Option<String>,
    pub path_style: Option<bool>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<Secret>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewBackupSchedule {
    pub destination_id: StorageDestinationId,
    pub cron: String,
    /// [`DEFAULT_KEEP_LAST`] if `None`.
    pub keep_last: Option<u32>,
    /// Enabled if `None`.
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackupScheduleChanges {
    pub destination_id: Option<StorageDestinationId>,
    pub cron: Option<String>,
    pub keep_last: Option<u32>,
    pub enabled: Option<bool>,
}

impl Control {
    pub async fn create_storage_destination(
        &self,
        principal: &Principal,
        new: NewStorageDestination,
    ) -> Result<StorageDestination, ControlError> {
        require_instance_admin(principal)?;
        let fields = StorageDestinationFields {
            name: name(&new.name)?,
            endpoint: new.endpoint.as_deref().map(endpoint).transpose()?,
            region: region(new.region.as_deref().unwrap_or("us-east-1"))?,
            bucket: bucket(&new.bucket)?,
            path_prefix: path_prefix(new.path_prefix.as_deref().unwrap_or(""))?,
            path_style: new.path_style,
            access_key_id: credential("access_key_id", &new.access_key_id)?,
        };
        let secret = Secret::new(credential("secret_access_key", new.secret_access_key.expose())?);
        let destination = self.store().create_storage_destination(fields, &secret).await?;
        let details = json!({ "name": destination.fields.name, "bucket": destination.fields.bucket });
        self.audit_by(principal, "storage_destination.create", destination.id, None, details).await?;
        Ok(destination)
    }

    /// Every destination, for anyone signed in: they choose where their backups go.
    pub async fn list_storage_destinations(&self, _principal: &Principal) -> Result<Vec<StorageDestination>, ControlError> {
        Ok(self.store().list_storage_destinations().await?)
    }

    pub async fn get_storage_destination(
        &self,
        _principal: &Principal,
        id: StorageDestinationId,
    ) -> Result<StorageDestination, ControlError> {
        Ok(self.store().get_storage_destination(id).await?)
    }

    /// Applies `changes` if the destination is still at `revision`. Backups already made stay
    /// where they are, under the old settings' bucket and prefix.
    pub async fn update_storage_destination(
        &self,
        principal: &Principal,
        id: StorageDestinationId,
        revision: i64,
        changes: StorageDestinationChanges,
    ) -> Result<StorageDestination, ControlError> {
        require_instance_admin(principal)?;
        let current = self.store().get_storage_destination(id).await?.fields;
        let fields = StorageDestinationFields {
            name: changes.name.as_deref().map(name).transpose()?.unwrap_or(current.name),
            endpoint: match &changes.endpoint {
                Some(value) => value.as_deref().map(endpoint).transpose()?,
                None => current.endpoint,
            },
            region: changes.region.as_deref().map(region).transpose()?.unwrap_or(current.region),
            bucket: changes.bucket.as_deref().map(bucket).transpose()?.unwrap_or(current.bucket),
            path_prefix: changes.path_prefix.as_deref().map(path_prefix).transpose()?.unwrap_or(current.path_prefix),
            path_style: changes.path_style.unwrap_or(current.path_style),
            access_key_id: changes
                .access_key_id
                .as_deref()
                .map(|value| credential("access_key_id", value))
                .transpose()?
                .unwrap_or(current.access_key_id),
        };
        let secret = changes
            .secret_access_key
            .as_ref()
            .map(|secret| credential("secret_access_key", secret.expose()).map(Secret::new))
            .transpose()?;
        let destination = self.store().update_storage_destination(id, revision, fields, secret.as_ref()).await?;
        let changed = audit::changed(&[
            ("name", changes.name.is_some()),
            ("endpoint", changes.endpoint.is_some()),
            ("region", changes.region.is_some()),
            ("bucket", changes.bucket.is_some()),
            ("path_prefix", changes.path_prefix.is_some()),
            ("path_style", changes.path_style.is_some()),
            ("access_key_id", changes.access_key_id.is_some()),
            ("secret_access_key", changes.secret_access_key.is_some()),
        ]);
        self.audit_by(principal, "storage_destination.update", id, None, changed).await?;
        Ok(destination)
    }

    /// Deletes a destination no schedule uses. Its backups stay in the bucket, and can still
    /// be deleted.
    pub async fn delete_storage_destination(&self, principal: &Principal, id: StorageDestinationId) -> Result<(), ControlError> {
        require_instance_admin(principal)?;
        let destination = self.store().get_storage_destination(id).await?;
        self.store().delete_storage_destination(id).await?;
        let details = json!({ "name": destination.fields.name });
        self.audit_by(principal, "storage_destination.delete", id, None, details).await
    }

    /// Checks that the control plane can write to the destination, by writing a small object
    /// under its prefix and deleting it. What went wrong is `Rejected`, with the storage's
    /// own error.
    pub async fn test_storage_destination(&self, principal: &Principal, id: StorageDestinationId) -> Result<(), ControlError> {
        require_instance_admin(principal)?;
        let destination = self.store().get_storage_destination(id).await?;
        let store = self.destination_store(&destination).await?;
        let path = Path::parse(format!("{}.spinikus-test-{:032x}", destination.fields.path_prefix, rand::random::<u128>()))
            .map_err(|error| ControlError::internal("making a test object's key", error))?;
        let rejected = |action: &str, error: object_store::Error| ControlError::Rejected {
            entity: "storage destination",
            id: id.to_string(),
            problem: format!("could not {action} a test object: {error}"),
        };
        store.put(&path, PutPayload::from_static(b"spinikus")).await.map_err(|error| rejected("write", error))?;
        store.delete(&path).await.map_err(|error| rejected("delete", error))?;
        Ok(())
    }

    /// The destination's bucket, with its credentials.
    pub(crate) async fn destination_store(&self, destination: &StorageDestination) -> Result<Arc<dyn ObjectStore>, ControlError> {
        let secret = self.store().storage_destination_secret(destination.id).await?;
        self.backups.stores.store(&destination.fields, &secret).map_err(|error| ControlError::Rejected {
            entity: "storage destination",
            id: destination.id.to_string(),
            problem: error.to_string(),
        })
    }

    pub async fn create_backup_schedule(
        &self,
        principal: &Principal,
        service_id: ServiceId,
        new: NewBackupSchedule,
    ) -> Result<BackupSchedule, ControlError> {
        let (service, environment) = self.service_with_access(principal, service_id, Role::Admin).await?;
        self.backup_sources(&service).await?;
        let cron = cron(&new.cron)?;
        let fields = BackupScheduleFields {
            destination_id: new.destination_id,
            cron: cron.pattern.to_string(),
            keep_last: keep_last(new.keep_last.unwrap_or(DEFAULT_KEEP_LAST))?,
            enabled: new.enabled.unwrap_or(true),
        };
        let next_run_at = fields.enabled.then(|| next_run(&cron, Timestamp::now())).flatten();
        let schedule = self.store().create_backup_schedule(service_id, fields, next_run_at).await?;
        let details = json!({
            "service_id": service_id.to_string(),
            "destination_id": schedule.fields.destination_id.to_string(),
            "cron": schedule.fields.cron,
        });
        self.audit_by(principal, "backup_schedule.create", schedule.id, Some(environment.project_id), details).await?;
        self.wake_backups();
        Ok(schedule)
    }

    pub async fn list_backup_schedules(
        &self,
        principal: &Principal,
        service_id: ServiceId,
    ) -> Result<Vec<BackupSchedule>, ControlError> {
        self.service_with_access(principal, service_id, Role::Viewer).await?;
        Ok(self.store().list_backup_schedules(service_id).await?)
    }

    pub async fn get_backup_schedule(&self, principal: &Principal, id: BackupScheduleId) -> Result<BackupSchedule, ControlError> {
        Ok(self.schedule_with_access(principal, id, Role::Viewer).await?.0)
    }

    /// Applies `changes` if the schedule is still at `revision`, and works out its next run
    /// again. A smaller `keep_last` deletes the backups beyond it on the runner's next pass.
    pub async fn update_backup_schedule(
        &self,
        principal: &Principal,
        id: BackupScheduleId,
        revision: i64,
        changes: BackupScheduleChanges,
    ) -> Result<BackupSchedule, ControlError> {
        let (current, environment) = self.schedule_with_access(principal, id, Role::Admin).await?;
        let cron = cron(changes.cron.as_deref().unwrap_or(&current.fields.cron))?;
        let fields = BackupScheduleFields {
            destination_id: changes.destination_id.unwrap_or(current.fields.destination_id),
            cron: cron.pattern.to_string(),
            keep_last: keep_last(changes.keep_last.unwrap_or(current.fields.keep_last))?,
            enabled: changes.enabled.unwrap_or(current.fields.enabled),
        };
        let next_run_at = fields.enabled.then(|| next_run(&cron, Timestamp::now())).flatten();
        let schedule = self.store().update_backup_schedule(id, revision, fields, next_run_at).await?;
        let changed = audit::changed(&[
            ("destination_id", changes.destination_id.is_some()),
            ("cron", changes.cron.is_some()),
            ("keep_last", changes.keep_last.is_some()),
            ("enabled", changes.enabled.is_some()),
        ]);
        self.audit_by(principal, "backup_schedule.update", id, Some(environment.project_id), changed).await?;
        self.wake_backups();
        Ok(schedule)
    }

    /// Deletes the schedule. Its backups stay, as if made by hand.
    pub async fn delete_backup_schedule(&self, principal: &Principal, id: BackupScheduleId) -> Result<(), ControlError> {
        let (schedule, environment) = self.schedule_with_access(principal, id, Role::Admin).await?;
        self.store().delete_backup_schedule(id).await?;
        let details = json!({ "service_id": schedule.service_id.to_string() });
        self.audit_by(principal, "backup_schedule.delete", id, Some(environment.project_id), details).await
    }

    async fn schedule_with_access(
        &self,
        principal: &Principal,
        id: BackupScheduleId,
        needed: Role,
    ) -> Result<(BackupSchedule, Environment), ControlError> {
        let schedule = self.store().get_backup_schedule(id).await?;
        match self.service_with_access(principal, schedule.service_id, needed).await {
            Err(ControlError::NotFound { .. }) => Err(not_found("backup schedule", id)),
            Err(error) => Err(error),
            Ok((_, environment)) => Ok((schedule, environment)),
        }
    }

    /// Queues a backup to `destination_id` now: of the database, or of one of the service's
    /// volumes, `volume_id` (which may be left out when it has only one). The runner sends
    /// it to the service's agent; a database's fails if it has nothing live.
    pub async fn create_backup(
        &self,
        principal: &Principal,
        service_id: ServiceId,
        destination_id: StorageDestinationId,
        volume_id: Option<VolumeId>,
    ) -> Result<Backup, ControlError> {
        let (service, environment) = self.service_with_access(principal, service_id, Role::Developer).await?;
        let sources = self.backup_sources(&service).await?;
        let source = match (sources.as_slice(), volume_id) {
            ([BackupSource::Database(_)], Some(_)) => {
                return Err(invalid("volume_id", "a database is backed up whole, with its engine's dump; leave it out"));
            }
            ([source @ BackupSource::Database(_)], None) => *source,
            (_, Some(volume)) => *sources
                .iter()
                .find(|source| **source == BackupSource::Volume(volume))
                .ok_or_else(|| invalid("volume_id", "is not one of the service's volumes"))?,
            ([source], None) => *source,
            (_, None) => return Err(invalid("volume_id", "the service has several volumes; name the one to back up")),
        };
        if matches!(source, BackupSource::Database(_)) && service.stopped_at.is_some() {
            let problem = "the database is stopped; start it first".to_string();
            return Err(ControlError::Rejected { entity: "service", id: service_id.to_string(), problem });
        }
        let backup = self.queue_backup(&service, destination_id, None, source).await?;
        let mut details = json!({ "service_id": service_id.to_string(), "destination_id": destination_id.to_string() });
        if let BackupSource::Volume(volume) = source {
            details["volume_id"] = json!(volume.to_string());
        }
        self.audit_by(principal, "backup.create", backup.id, Some(environment.project_id), details).await?;
        self.wake_backups();
        Ok(backup)
    }

    /// What a backup of the service holds: a database's dump, or each of the service's
    /// volumes. `Rejected` when there is nothing to back up.
    pub(crate) async fn backup_sources(&self, service: &Service) -> Result<Vec<BackupSource>, ControlError> {
        if let Some(engine) = Engine::of(&service.config) {
            return Ok(vec![BackupSource::Database(engine)]);
        }
        let volumes = self.store().list_volumes(service.id).await?;
        if volumes.is_empty() {
            return Err(ControlError::Rejected {
                entity: "service",
                id: service.id.to_string(),
                problem: "it has no volumes, so nothing to back up; only databases and services with volumes have backups"
                    .to_string(),
            });
        }
        Ok(volumes.into_iter().map(|volume| BackupSource::Volume(volume.id)).collect())
    }

    /// Records a queued backup of the service.
    pub(crate) async fn queue_backup(
        &self,
        service: &Service,
        destination_id: StorageDestinationId,
        schedule_id: Option<BackupScheduleId>,
        source: BackupSource,
    ) -> Result<Backup, ControlError> {
        let destination = self.store().get_storage_destination(destination_id).await?;
        let id = BackupId::new();
        let new = NewBackup {
            id,
            service_id: service.id,
            destination_id,
            schedule_id,
            source,
            object_key: object_key(&destination.fields.path_prefix, service.id, id, source),
        };
        Ok(self.store().create_backup(new).await?)
    }

    /// The service's backups, newest first.
    pub async fn list_backups(
        &self,
        principal: &Principal,
        service_id: ServiceId,
        after: Option<BackupId>,
        limit: u32,
    ) -> Result<Page<Backup>, ControlError> {
        self.service_with_access(principal, service_id, Role::Viewer).await?;
        let limit = page::limit(limit);
        let items = self.store().list_backups(service_id, after, limit + 1).await?;
        Ok(Page::from_overfetch(items, limit))
    }

    pub async fn get_backup(&self, principal: &Principal, id: BackupId) -> Result<Backup, ControlError> {
        Ok(self.backup_with_access(principal, id, Role::Viewer).await?.0)
    }

    /// Deletes a finished backup and its object. One still running can't be deleted.
    pub async fn delete_backup(&self, principal: &Principal, id: BackupId) -> Result<(), ControlError> {
        let (backup, environment) = self.backup_with_access(principal, id, Role::Admin).await?;
        if !backup.status.is_finished() {
            return Err(ControlError::Rejected {
                entity: "backup",
                id: id.to_string(),
                problem: "it has not finished yet".to_string(),
            });
        }
        let restoring = self.store().list_active_restores().await?;
        if restoring.iter().any(|restore| restore.backup_id == id) {
            return Err(ControlError::Rejected {
                entity: "backup",
                id: id.to_string(),
                problem: "it is being restored".to_string(),
            });
        }
        self.remove_backup(&backup).await?;
        let details = json!({ "service_id": backup.service_id.to_string(), "object_key": backup.object_key });
        self.audit_by(principal, "backup.delete", id, Some(environment.project_id), details).await
    }

    /// Deletes the backup's object, if it may have one, then marks the backup deleted.
    pub(crate) async fn remove_backup(&self, backup: &Backup) -> Result<(), ControlError> {
        // A failed upload was aborted; a failed backup can't have left an object.
        if backup.status == spinikus_core::backup::BackupStatus::Succeeded {
            let destination = self.store().get_storage_destination_any(backup.destination_id).await?;
            let store = self.destination_store(&destination).await?;
            let path =
                Path::parse(&backup.object_key).map_err(|error| ControlError::internal("reading a backup's key", error))?;
            match store.delete(&path).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                Err(error) => {
                    return Err(ControlError::Rejected {
                        entity: "backup",
                        id: backup.id.to_string(),
                        problem: format!("could not delete its object: {error}"),
                    });
                }
            }
        }
        Ok(self.store().mark_backup_deleted(backup.id).await?)
    }

    pub(crate) async fn backup_with_access(
        &self,
        principal: &Principal,
        id: BackupId,
        needed: Role,
    ) -> Result<(Backup, Environment), ControlError> {
        let backup = self.store().get_backup(id).await?;
        match self.service_with_access(principal, backup.service_id, needed).await {
            Err(ControlError::NotFound { .. }) => Err(not_found("backup", id)),
            Err(error) => Err(error),
            Ok((_, environment)) => Ok((backup, environment)),
        }
    }
}

pub(crate) fn require_instance_admin(principal: &Principal) -> Result<(), ControlError> {
    if principal.is_instance_admin() { Ok(()) } else { Err(ControlError::Forbidden) }
}

/// Parses a five-field cron expression.
pub(crate) fn cron(value: &str) -> Result<Cron, ControlError> {
    CronParser::builder()
        .seconds(croner::parser::Seconds::Disallowed)
        .year(croner::parser::Year::Disallowed)
        .build()
        .parse(value.trim())
        .map_err(|error| ControlError::Invalid { field: "cron", problem: error.to_string() })
}

/// The first time after `after` that matches, in UTC; `None` if none ever does.
pub(crate) fn next_run(cron: &Cron, after: Timestamp) -> Option<Timestamp> {
    let after = after.to_zoned(TimeZone::UTC);
    cron.find_next_occurrence(&after, false).ok().map(|next| next.timestamp())
}

pub(crate) fn keep_last(value: u32) -> Result<u32, ControlError> {
    match value {
        1..=MAX_KEEP_LAST => Ok(value),
        _ => Err(ControlError::Invalid { field: "keep_last", problem: format!("must be between 1 and {MAX_KEEP_LAST}") }),
    }
}

fn invalid(field: &'static str, problem: impl Into<String>) -> ControlError {
    ControlError::Invalid { field, problem: problem.into() }
}

fn name(value: &str) -> Result<String, ControlError> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > MAX_NAME_LEN || value.chars().any(char::is_control) {
        return Err(invalid("name", format!("must be 1 to {MAX_NAME_LEN} characters, without control characters")));
    }
    Ok(value.to_string())
}

/// An `http` or `https` URL without a path, query or credentials, such as
/// `https://<account>.r2.cloudflarestorage.com`.
fn endpoint(value: &str) -> Result<String, ControlError> {
    let value = value.trim().trim_end_matches('/');
    let rest = value.strip_prefix("https://").or_else(|| value.strip_prefix("http://"));
    let fine = rest.is_some_and(|host| {
        !host.is_empty()
            && host.len() <= 253
            && host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
    });
    if !fine {
        return Err(invalid("endpoint", "must be an http or https URL with only a host and an optional port"));
    }
    Ok(value.to_string())
}

fn region(value: &str) -> Result<String, ControlError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 64 || !value.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')) {
        return Err(invalid("region", "must be letters, digits, `-` and `_`, such as `us-east-1` or `auto`"));
    }
    Ok(value.to_string())
}

/// S3's bucket naming rules, loosely: 3 to 63 lowercase letters, digits, `.` and `-`.
fn bucket(value: &str) -> Result<String, ControlError> {
    let value = value.trim();
    let fine = (3..=63).contains(&value.len())
        && value.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-'))
        && value.starts_with(|c: char| c.is_ascii_alphanumeric())
        && value.ends_with(|c: char| c.is_ascii_alphanumeric());
    if !fine {
        return Err(invalid("bucket", "must be 3 to 63 lowercase letters, digits, `.` and `-`"));
    }
    Ok(value.to_string())
}

/// Empty, or segments separated by `/` and ending with one, such as `backups/prod/`.
fn path_prefix(value: &str) -> Result<String, ControlError> {
    let value = value.trim().trim_matches('/');
    if value.is_empty() {
        return Ok(String::new());
    }
    let fine = value.len() <= MAX_PATH_PREFIX_LEN
        && value.split('/').all(|segment| {
            !segment.is_empty() && segment != "." && segment != ".." && !segment.chars().any(|c| c.is_control() || c == '\\')
        });
    if !fine {
        return Err(invalid("path_prefix", "must be path segments separated by `/`, without empty, `.` or `..` segments"));
    }
    Ok(format!("{value}/"))
}

fn credential(field: &'static str, value: &str) -> Result<String, ControlError> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_CREDENTIAL_LEN || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(invalid(field, format!("must be 1 to {MAX_CREDENTIAL_LEN} characters, without spaces")));
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests;
