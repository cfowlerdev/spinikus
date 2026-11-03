//! Scheduled backups of the Spinikus instance itself: its database and master key, in one
//! file encrypted with a passphrase, uploaded by the control plane to a storage destination
//! as `<path prefix>instance/<ID>.tar.gz.age`.
//!
//! There is one schedule per instance. It holds the passphrase too (sealed in the database
//! like other secrets), since the control plane needs it to encrypt; a backup file needs only
//! the passphrase to be opened, by `spinikus admin restore`.

use jiff::Timestamp;
use serde::Serialize;

use crate::{
    backup::BackupStatus,
    id::{InstanceBackupId, StorageDestinationId},
};

/// When the instance is backed up, where to, and how many backups to keep. The passphrase is
/// never included: it is write-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstanceBackupSchedule {
    #[serde(flatten)]
    pub fields: InstanceBackupScheduleFields,
    /// When the next backup is due; `None` while the schedule is disabled.
    pub next_run_at: Option<Timestamp>,
    pub revision: i64,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstanceBackupScheduleFields {
    pub destination_id: StorageDestinationId,
    /// A five-field cron expression, in UTC, such as `0 3 * * *`.
    pub cron: String,
    /// How many of the schedule's successful backups to keep; older ones are deleted.
    pub keep_last: u32,
    pub enabled: bool,
}

/// One backup of the instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstanceBackup {
    pub id: InstanceBackupId,
    pub destination_id: StorageDestinationId,
    /// Made by the schedule, rather than asked for by hand. Only the schedule's own count
    /// toward what it keeps.
    pub scheduled: bool,
    pub status: BackupStatus,
    /// The key of the file in the destination's bucket.
    pub object_key: String,
    pub size_bytes: Option<u64>,
    /// Why it failed.
    pub error: Option<String>,
    pub created_at: Timestamp,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
}

/// A new backup to record, queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewInstanceBackup {
    pub id: InstanceBackupId,
    pub destination_id: StorageDestinationId,
    pub scheduled: bool,
    pub object_key: String,
}

/// Where a backup goes in its destination's bucket.
pub fn object_key(path_prefix: &str, id: InstanceBackupId) -> String {
    format!("{path_prefix}instance/{id}.tar.gz.age")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_backups_under_instance() {
        let id = InstanceBackupId::new();
        assert_eq!(object_key("spinikus/", id), format!("spinikus/instance/{id}.tar.gz.age"));
        assert_eq!(object_key("", id), format!("instance/{id}.tar.gz.age"));
    }
}
