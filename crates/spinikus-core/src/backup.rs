//! Backups of databases and of app volumes to S3-compatible storage.
//!
//! A storage destination is a bucket (on AWS, R2, B2, MinIO, …) with credentials, shared by
//! the instance. A backup is one run, of one [`BackupSource`]: a database, which the agent of
//! its server dumps with the engine's own tool, or one volume of an app, which the agent
//! archives with `tar` in a helper container ([`HELPER_IMAGE`]) that mounts it. Either goes
//! straight to the destination, as `<path prefix><service ID>/<backup ID>.<extension>`.
//! Schedules create backups on a cron expression (one per volume, for an app) and keep the
//! newest few of their own, per volume.
//!
//! A restore loads a backup back where it came from, replacing what is there, after taking
//! a safety backup of the current data.

use jiff::Timestamp;
use serde::Serialize;

use crate::{
    database::Engine,
    id::{BackupId, BackupScheduleId, RestoreId, ServerId, ServiceId, StorageDestinationId, VolumeId},
    text::impl_text,
};

/// A bucket backups go to. The secret access key is never included: it is write-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
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
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
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
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
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
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
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
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Backup {
    pub id: BackupId,
    pub service_id: ServiceId,
    pub destination_id: StorageDestinationId,
    /// The schedule that made it; `None` for one asked for by hand.
    pub schedule_id: Option<BackupScheduleId>,
    pub status: BackupStatus,
    /// The database's engine, for a dump; `None` for a volume's archive.
    pub engine: Option<Engine>,
    /// The volume archived; `None` for a database dump.
    pub volume_id: Option<VolumeId>,
    /// The key of the dump or archive in the destination's bucket.
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
    pub source: BackupSource,
    pub object_key: String,
}

/// What a backup holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackupSource {
    /// A dump of a database service, made with its engine's tool.
    Database(Engine),
    /// A gzipped tar of one volume of a service.
    Volume(VolumeId),
}

impl BackupSource {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Database(engine) => engine.dump_extension(),
            Self::Volume(_) => "tar.gz",
        }
    }
}

impl Backup {
    pub fn source(&self) -> BackupSource {
        match (self.engine, self.volume_id) {
            (_, Some(volume)) => BackupSource::Volume(volume),
            (Some(engine), None) => BackupSource::Database(engine),
            (None, None) => unreachable!("the store keeps exactly one of a backup's engine and volume"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
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
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
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

/// How a dump is loaded back into an engine's database, by commands run in its container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePlan {
    /// Reads the dump from stdin and puts it in place; a non-zero exit leaves the data as
    /// it was.
    pub command: Vec<String>,
    /// Whether the container is restarted once `command` succeeded, for engines that load
    /// their data only on start.
    pub restart: bool,
    /// After the restart, exits 0 once the database serves again; empty for none.
    pub ready_command: Vec<String>,
}

/// The Redis script that replaces the data with an RDB dump read from stdin. The dump
/// becomes the base of a new append-only file (Redis loads the AOF, not `dump.rdb`, when it
/// is on); it is checked as such before anything changes. Persistence is then switched off
/// in the running server, so nothing it writes before the restart replaces the new files,
/// and the restart (by the agent) loads them, with the configuration from the command line
/// again. Writes between the swap and the restart are lost, as a restore implies.
const REDIS_RESTORE: &str = r#"set -eu
r() { redis-cli -a "$REDIS_PASSWORD" --no-auth-warning "$@"; }
staged=/data/appendonlydir.spinikus-restore
trap 'rm -rf "$staged" /data/dump.rdb.spinikus-restore' EXIT
rm -rf "$staged"
mkdir "$staged"
cat > "$staged/appendonly.aof.1.base.rdb"
printf 'file appendonly.aof.1.base.rdb seq 1 type b\n' > "$staged/appendonly.aof.manifest"
redis-check-aof "$staged/appendonly.aof.manifest" >&2
cp "$staged/appendonly.aof.1.base.rdb" /data/dump.rdb.spinikus-restore
r CONFIG SET save '' > /dev/null
r CONFIG SET appendonly no > /dev/null
while r INFO persistence | grep -Eq '^(rdb_bgsave_in_progress|aof_rewrite_in_progress):1'; do sleep 0.1; done
rm -rf /data/appendonlydir
mv "$staged" /data/appendonlydir
mv -f /data/dump.rdb.spinikus-restore /data/dump.rdb"#;

fn shell(script: &str) -> Vec<String> {
    vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()]
}

impl Engine {
    /// The command, run in the database's container, that writes a dump to stdout. It runs
    /// with the container's environment, so it uses the database's own credentials.
    pub fn dump_command(self) -> Vec<String> {
        shell(match self {
            // The custom format is compressed and restores selectively with pg_restore.
            Self::Postgres => r#"exec pg_dump --format=custom --username="$POSTGRES_USER" "$POSTGRES_DB""#,
            // A snapshot in Redis's own RDB format, through a file: written to stdout,
            // redis-cli ends a diskless transfer with its end-of-file marker, which isn't RDB.
            Self::Redis => {
                r#"set -e; dump=$(mktemp); trap 'rm -f "$dump"' EXIT; redis-cli -a "$REDIS_PASSWORD" --no-auth-warning --rdb "$dump" >&2; cat "$dump""#
            }
        })
    }

    /// How a dump from [`Engine::dump_command`] replaces the database's data.
    pub fn restore_plan(self) -> RestorePlan {
        match self {
            // pg_restore needs a seekable file for a dump written to a pipe (it has no data
            // offsets), so stdin goes to a temporary file in the container first. It runs in
            // one transaction, so a failure changes nothing.
            Self::Postgres => RestorePlan {
                command: shell(
                    r#"set -e; dump=$(mktemp); trap 'rm -f "$dump"' EXIT; cat > "$dump"; pg_restore --clean --if-exists --no-owner --single-transaction --exit-on-error --username="$POSTGRES_USER" --dbname="$POSTGRES_DB" "$dump""#,
                ),
                restart: false,
                ready_command: Vec::new(),
            },
            // Redis replaces its data only by loading files on start; it answers PING with
            // an error while loading.
            Self::Redis => RestorePlan {
                command: shell(REDIS_RESTORE),
                restart: true,
                ready_command: shell(r#"[ "$(redis-cli -a "$REDIS_PASSWORD" --no-auth-warning PING)" = PONG ]"#),
            },
        }
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
pub fn object_key(path_prefix: &str, service: ServiceId, backup: BackupId, source: BackupSource) -> String {
    format!("{path_prefix}{service}/{backup}.{}", source.extension())
}

/// The image of the helper containers that archive and restore volumes: small, and with
/// `tar`, `gzip` and a shell, which app images may lack.
pub const HELPER_IMAGE: &str = "busybox:1.37.0";

/// Where a helper container mounts the volume.
pub const HELPER_MOUNT: &str = "/volume";

/// Writes the volume's contents to stdout as a gzipped tar, paths relative to its root.
pub fn volume_archive_command() -> Vec<String> {
    ["tar", "-czf", "-", "-C", HELPER_MOUNT, "."].map(String::from).to_vec()
}

/// Replaces the volume's contents with the gzipped tar on stdin. It is unpacked beside the
/// current contents first, so a broken or cut-off archive changes nothing; only then are the
/// current contents removed and the unpacked ones moved in, renames on the same filesystem.
pub fn volume_restore_command() -> Vec<String> {
    let script = format!(
        r#"set -eu
stage={HELPER_MOUNT}/.spinikus-restore
rm -rf "$stage"
trap 'rm -rf "$stage"' EXIT
mkdir "$stage"
tar -xzf - -C "$stage"
cd {HELPER_MOUNT}
for entry in * .[!.]* ..?*; do
  if [ -e "$entry" ] || [ -L "$entry" ]; then
    [ "$entry" = .spinikus-restore ] || rm -rf "$entry"
  fi
done
cd "$stage"
for entry in * .[!.]* ..?*; do
  if [ -e "$entry" ] || [ -L "$entry" ]; then mv "$entry" {HELPER_MOUNT}/; fi
done"#
    );
    vec!["/bin/sh".to_string(), "-c".to_string(), script]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_backups_by_service_and_id() {
        let (service, backup) = (ServiceId::new(), BackupId::new());

        let (postgres, redis) = (BackupSource::Database(Engine::Postgres), BackupSource::Database(Engine::Redis));
        let volume = BackupSource::Volume(VolumeId::new());
        assert_eq!(object_key("spinikus/", service, backup, postgres), format!("spinikus/{service}/{backup}.pgdump"));
        assert_eq!(object_key("", service, backup, redis), format!("{service}/{backup}.rdb"));
        assert_eq!(object_key("", service, backup, volume), format!("{service}/{backup}.tar.gz"));
    }

    #[test]
    fn dumps_with_the_containers_credentials() {
        let postgres = Engine::Postgres.dump_command();
        let redis = Engine::Redis.dump_command();

        assert_eq!(postgres[..2], ["/bin/sh", "-c"]);
        assert!(postgres[2].contains(r#"--username="$POSTGRES_USER""#));
        assert!(redis[2].contains(r#"--rdb "$dump""#) && redis[2].ends_with(r#"cat "$dump""#));
    }

    #[test]
    fn restores_postgres_in_one_transaction_and_redis_by_restarting() {
        let postgres = Engine::Postgres.restore_plan();
        let redis = Engine::Redis.restore_plan();

        assert!(postgres.command[2].contains("--single-transaction") && postgres.command[2].contains("--clean"));
        assert!(postgres.command[2].contains(r#"--dbname="$POSTGRES_DB""#));
        assert!(!postgres.restart && postgres.ready_command.is_empty());
        assert_eq!(redis.command[..2], ["/bin/sh", "-c"]);
        let data = crate::database::DatabaseSpec::new(Engine::Redis, None).unwrap().data_path();
        assert!(redis.command[2].contains(&format!("{}/appendonlydir", data.as_str())));
        // The dump is checked before anything changes.
        let script = &redis.command[2];
        assert!(script.find("redis-check-aof").unwrap() < script.find("CONFIG SET").unwrap());
        assert!(redis.restart && redis.ready_command[2].contains("PING"));
    }

    #[test]
    fn text_round_trips() {
        for status in BackupStatus::ALL {
            assert_eq!(status.to_string().parse(), Ok(status));
        }
        assert!(BackupStatus::Failed.is_finished() && !BackupStatus::Running.is_finished());
    }
}
