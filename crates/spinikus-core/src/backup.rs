//! Backups of database services to S3-compatible storage.
//!
//! A storage destination is a bucket (on AWS, R2, B2, MinIO, …) with credentials, shared by
//! the instance. A backup is one run: the agent of the database's server dumps it with the
//! engine's own tool and uploads the dump straight to the destination, as
//! `<path prefix><service ID>/<backup ID>.<extension>`. Schedules create backups on a cron
//! expression and keep the newest few of their own.
//!
//! A restore loads a backup back into its database, replacing what is there, after taking
//! a safety backup of the current data. Only Postgres can be restored so far.

use jiff::Timestamp;
use serde::Serialize;

use crate::{
    database::Engine,
    id::{BackupId, BackupScheduleId, RestoreId, ServerId, ServiceId, StorageDestinationId},
    text::impl_text,
};

/// A bucket backups go to. The secret access key is never included: it is write-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StorageDestination {
    pub id: StorageDestinationId,
    #[serde(flatten)]
    pub fields: StorageDestinationFields,
    pub revision: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// What a user sets on a destination, besides the secret access key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StorageDestinationFields {
    pub name: String,
    /// The S3 API's URL, such as `https://<account>.r2.cloudflarestorage.com`; AWS if `None`.
    pub endpoint: Option<String>,
    pub region: String,
    pub bucket: String,
    /// Prepended to every object key: empty, or ending with `/`.
    pub path_prefix: String,
    /// Addresses the bucket in the path (`<endpoint>/<bucket>/…`) rather than the host,
    /// which MinIO and some other servers need.
    pub path_style: bool,
    pub access_key_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackupSchedule {
    pub id: BackupScheduleId,
    pub service_id: ServiceId,
    #[serde(flatten)]
    pub fields: BackupScheduleFields,
    /// When the next backup is due; `None` while the schedule is disabled.
    pub next_run_at: Option<Timestamp>,
    pub revision: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackupScheduleFields {
    pub destination_id: StorageDestinationId,
    /// A five-field cron expression, in UTC, such as `0 3 * * *`.
    pub cron: String,
    /// How many of its own successful backups the schedule keeps; older ones are deleted.
    pub keep_last: u32,
    pub enabled: bool,
}

/// The most backups a schedule can keep.
pub const MAX_KEEP_LAST: u32 = 1000;

/// One backup of a database service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Backup {
    pub id: BackupId,
    pub service_id: ServiceId,
    pub destination_id: StorageDestinationId,
    /// The schedule that made it; `None` for one asked for by hand.
    pub schedule_id: Option<BackupScheduleId>,
    pub status: BackupStatus,
    pub engine: Engine,
    /// The key of the dump in the destination's bucket.
    pub object_key: String,
    /// The server that made it, once it started.
    pub server_id: Option<ServerId>,
    pub size_bytes: Option<u64>,
    /// Why it failed.
    pub error: Option<String>,
    pub created_at: Timestamp,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
}

/// A new backup to record, queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewBackup {
    pub id: BackupId,
    pub service_id: ServiceId,
    pub destination_id: StorageDestinationId,
    pub schedule_id: Option<BackupScheduleId>,
    pub engine: Engine,
    pub object_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupStatus {
    /// Waiting for the database's agent.
    Queued,
    /// The agent is dumping and uploading it.
    Running,
    Succeeded,
    Failed,
}

impl BackupStatus {
    pub const ALL: [BackupStatus; 4] = [Self::Queued, Self::Running, Self::Succeeded, Self::Failed];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    pub fn is_finished(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }
}

impl_text!(BackupStatus, "backup status");

/// A restore goes through the same states as a backup.
pub type RestoreStatus = BackupStatus;

/// One restore of a backup into its database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Restore {
    pub id: RestoreId,
    pub service_id: ServiceId,
    /// The backup being restored.
    pub backup_id: BackupId,
    /// The backup of the data the restore replaces, taken first. The restore waits for it,
    /// and fails if it fails.
    pub safety_backup_id: Option<BackupId>,
    pub status: RestoreStatus,
    /// The server that ran it, once it started.
    pub server_id: Option<ServerId>,
    /// Why it failed.
    pub error: Option<String>,
    pub created_at: Timestamp,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
}

/// A new restore to record, queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRestore {
    pub service_id: ServiceId,
    pub backup_id: BackupId,
    pub safety_backup_id: Option<BackupId>,
}

impl Engine {
    /// The command, run in the database's container, that writes a dump to stdout. It runs
    /// with the container's environment, so it uses the database's own credentials.
    pub fn dump_command(self) -> Vec<String> {
        let script = match self {
            // The custom format is compressed and restores selectively with pg_restore.
            Self::Postgres => r#"exec pg_dump --format=custom --username="$POSTGRES_USER" "$POSTGRES_DB""#,
            // A snapshot in Redis's own RDB format, streamed from the server.
            Self::Redis => r#"exec redis-cli -a "$REDIS_PASSWORD" --no-auth-warning --rdb -"#,
        };
        vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()]
    }

    /// The command, run in the database's container, that loads a dump from stdin, replacing
    /// what the dump contains, in one transaction. `None` for engines that can't be restored
    /// yet.
    pub fn restore_command(self) -> Option<Vec<String>> {
        let script = match self {
            // pg_restore needs a seekable file for a dump written to a pipe (it has no data
            // offsets), so stdin goes to a temporary file in the container first.
            Self::Postgres => {
                r#"set -e; dump=$(mktemp); trap 'rm -f "$dump"' EXIT; cat > "$dump"; pg_restore --clean --if-exists --no-owner --single-transaction --exit-on-error --username="$POSTGRES_USER" --dbname="$POSTGRES_DB" "$dump""#
            }
            Self::Redis => return None,
        };
        Some(vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()])
    }

    /// The extension of a dump's object key.
    pub fn dump_extension(self) -> &'static str {
        match self {
            Self::Postgres => "pgdump",
            Self::Redis => "rdb",
        }
    }
}

/// Where a backup goes in its destination's bucket.
pub fn object_key(path_prefix: &str, service: ServiceId, backup: BackupId, engine: Engine) -> String {
    format!("{path_prefix}{service}/{backup}.{}", engine.dump_extension())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_backups_by_service_and_id() {
        let (service, backup) = (ServiceId::new(), BackupId::new());

        assert_eq!(object_key("spinikus/", service, backup, Engine::Postgres), format!("spinikus/{service}/{backup}.pgdump"));
        assert_eq!(object_key("", service, backup, Engine::Redis), format!("{service}/{backup}.rdb"));
    }

    #[test]
    fn dumps_with_the_containers_credentials() {
        let postgres = Engine::Postgres.dump_command();
        let redis = Engine::Redis.dump_command();

        assert_eq!(postgres[..2], ["/bin/sh", "-c"]);
        assert!(postgres[2].contains(r#"--username="$POSTGRES_USER""#));
        assert!(redis[2].contains("--rdb -"));
    }

    #[test]
    fn restores_postgres_in_one_transaction_and_not_redis_yet() {
        let postgres = Engine::Postgres.restore_command().unwrap();

        assert!(postgres[2].contains("--single-transaction") && postgres[2].contains("--clean"));
        assert!(postgres[2].contains(r#"--dbname="$POSTGRES_DB""#));
        assert_eq!(Engine::Redis.restore_command(), None);
    }

    #[test]
    fn text_round_trips() {
        for status in BackupStatus::ALL {
            assert_eq!(status.to_string().parse(), Ok(status));
        }
        assert!(BackupStatus::Failed.is_finished() && !BackupStatus::Running.is_finished());
    }
}
