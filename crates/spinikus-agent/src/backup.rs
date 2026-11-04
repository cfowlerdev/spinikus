//! Database backups: the dump command runs in the database's container, and what it writes to
//! stdout goes straight to the destination as a multipart upload, a part at a time, so a dump
//! never has to fit in memory or on disk. A dump that exits with an error aborts the upload,
//! and its stderr says why.

use std::sync::Arc;

use object_store::{ObjectStore, ObjectStoreExt, WriteMultipart, aws::AmazonS3Builder, path::Path};
use spinikus_core::id::BackupId;
use spinikus_proto::agent::v1::{self as pb, FailureReason};
use tokio_stream::StreamExt;

use crate::{
    commands::{Progress, failure, parse_id},
    runtime::{ContainerRuntime, ExecOutput, ExecStream},
};

/// How many parts may be uploading at once; the dump waits for one to finish beyond that.
const UPLOADS_AT_ONCE: usize = 4;

/// How much of a command's stderr is kept for the failure message.
pub(crate) const MAX_STDERR: usize = 4096;

/// Where backups go: each command's destination, or one store for every backup in tests.
#[derive(Clone, Default)]
pub enum BackupStores {
    #[default]
    Destination,
    Fixed(Arc<dyn ObjectStore>),
}

impl std::fmt::Debug for BackupStores {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Destination => f.write_str("Destination"),
            Self::Fixed(store) => write!(f, "Fixed({store})"),
        }
    }
}

impl BackupStores {
    pub(crate) fn store(&self, destination: &pb::StorageDestination) -> Result<Arc<dyn ObjectStore>, pb::CommandFailed> {
        match self {
            Self::Destination => s3(destination).map_err(|error| backup_failed(format!("invalid storage destination: {error}"))),
            Self::Fixed(store) => Ok(store.clone()),
        }
    }
}

/// The destination's bucket, over the S3 API.
pub fn s3(destination: &pb::StorageDestination) -> Result<Arc<dyn ObjectStore>, object_store::Error> {
    // object_store uses rustls without choosing a provider; this is the one the agent uses.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut builder = AmazonS3Builder::new()
        .with_bucket_name(&destination.bucket)
        .with_region(&destination.region)
        .with_access_key_id(&destination.access_key_id)
        .with_secret_access_key(&destination.secret_access_key)
        .with_virtual_hosted_style_request(!destination.path_style);
    if !destination.endpoint.is_empty() {
        builder = builder.with_endpoint(&destination.endpoint).with_allow_http(destination.endpoint.starts_with("http://"));
    }
    Ok(Arc::new(builder.build()?))
}

pub(crate) async fn backup_database(
    runtime: &dyn ContainerRuntime,
    stores: &BackupStores,
    backup: pb::BackupDatabase,
    progress: &Progress,
) -> Result<pb::CommandSucceeded, pb::CommandFailed> {
    let backup_id: BackupId = parse_id("backup ID", &backup.backup_id)?;
    if backup.command.is_empty() {
        return Err(backup_failed("no dump command"));
    }
    let destination = backup.destination.as_ref().ok_or_else(|| backup_failed("no storage destination"))?;
    let path = Path::parse(&backup.object_key).map_err(|error| backup_failed(format!("invalid object key: {error}")))?;
    if runtime.inspect(&backup.container_id).await?.is_none() {
        return Err(backup_failed(format!("no container `{}` managed by Spinikus", backup.container_id)));
    }
    let store = stores.store(destination)?;

    progress.phase("backing up");
    tracing::info!(backup = %backup_id, container = backup.container_id, "backing up a database");
    let output = runtime.exec(&backup.container_id, backup.command, None);
    match upload(output, &*store, &path, "the dump").await {
        Ok(size) => {
            progress.line(format!("uploaded {size} bytes to {}", backup.object_key));
            tracing::info!(backup = %backup_id, size, "backed up a database");
            Ok(uploaded(size))
        }
        Err(message) => {
            tracing::warn!(backup = %backup_id, message, "a backup failed");
            Err(backup_failed(message))
        }
    }
}

pub(crate) fn uploaded(size_bytes: u64) -> pb::CommandSucceeded {
    pb::CommandSucceeded { prepared: None, backup: Some(pb::BackupUploaded { size_bytes }) }
}

/// Uploads what a command (`what`, in errors) writes to stdout to `path`, a part at a time,
/// and returns its size. A command that fails aborts the upload; its stderr says why.
pub(crate) async fn upload(mut output: ExecStream, store: &dyn ObjectStore, path: &Path, what: &str) -> Result<u64, String> {
    let upload = store.put_multipart(path).await.map_err(|error| format!("could not start the upload: {error}"))?;
    let mut upload = WriteMultipart::new(upload);
    let mut size: u64 = 0;
    let mut stderr = Vec::new();
    let outcome = loop {
        match output.next().await {
            Some(Ok(ExecOutput::Stdout(bytes))) => {
                if let Err(error) = upload.wait_for_capacity(UPLOADS_AT_ONCE).await {
                    break Err(format!("the upload failed: {error}"));
                }
                size += bytes.len() as u64;
                upload.put(bytes);
            }
            Some(Ok(ExecOutput::Stderr(bytes))) => {
                keep_stderr(&mut stderr, &bytes);
            }
            Some(Ok(ExecOutput::Exited(0))) => break Ok(()),
            Some(Ok(ExecOutput::Exited(code))) => break Err(exited(what, code, &stderr)),
            Some(Err(error)) => break Err(format!("{what} failed: {error}")),
            None => break Err(format!("{what} ended without an exit code")),
        }
    };
    // Stop reading before the upload is finished or aborted.
    drop(output);
    match outcome {
        Ok(()) => {
            upload.finish().await.map_err(|error| format!("the upload failed: {error}"))?;
            Ok(size)
        }
        Err(message) => {
            if let Err(error) = upload.abort().await {
                tracing::warn!(%error, "could not abort an upload");
            }
            Err(message)
        }
    }
}

/// Keeps the start of a command's stderr, up to [`MAX_STDERR`].
pub(crate) fn keep_stderr(kept: &mut Vec<u8>, bytes: &[u8]) {
    let room = MAX_STDERR.saturating_sub(kept.len());
    kept.extend_from_slice(&bytes[..bytes.len().min(room)]);
}

/// Why `what` failed: its exit code, and what it wrote to stderr.
pub(crate) fn exited(what: &str, code: i64, stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    match stderr.trim() {
        "" => format!("{what} exited with {code}"),
        stderr => format!("{what} exited with {code}: {stderr}"),
    }
}

pub(crate) fn backup_failed(message: impl Into<String>) -> pb::CommandFailed {
    failure(FailureReason::BackupFailed, message)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use object_store::memory::InMemory;
    use spinikus_core::{
        id::{DeploymentId, ServiceId},
        server::{InstanceHealth, InstanceStatus, ObservedInstance},
    };
    use tokio::sync::mpsc;

    use super::*;
    use crate::runtime::MemoryRuntime;

    fn database(runtime: &MemoryRuntime) -> String {
        runtime.put(ObservedInstance {
            container_id: "pg".to_string(),
            service_id: ServiceId::new(),
            deployment_id: DeploymentId::new(),
            status: InstanceStatus::Running,
            health: InstanceHealth::None,
            restart_count: 0,
            started_at: None,
        });
        "pg".to_string()
    }

    fn command(container_id: String, key: &str) -> pb::BackupDatabase {
        pb::BackupDatabase {
            backup_id: BackupId::new().to_string(),
            container_id,
            command: vec!["pg_dump".to_string()],
            destination: Some(pb::StorageDestination { bucket: "b".to_string(), ..Default::default() }),
            object_key: key.to_string(),
        }
    }

    fn progress() -> Progress {
        Progress::new("cmd".to_string(), mpsc::channel(16).0)
    }

    #[tokio::test]
    async fn uploads_what_the_dump_writes() {
        let runtime = MemoryRuntime::new();
        let container = database(&runtime);
        let store = Arc::new(InMemory::new());
        let big = Bytes::from(vec![7u8; 6 * 1024 * 1024]);
        runtime.set_exec_output(vec![
            ExecOutput::Stdout(big.clone()),
            ExecOutput::Stderr(Bytes::from_static(b"warning\n")),
            ExecOutput::Stdout(Bytes::from_static(b"tail")),
            ExecOutput::Exited(0),
        ]);

        let stores = BackupStores::Fixed(store.clone());
        let done = backup_database(&runtime, &stores, command(container.clone(), "x/svc/1.pgdump"), &progress()).await.unwrap();

        let uploaded = store.get(&Path::from("x/svc/1.pgdump")).await.unwrap().bytes().await.unwrap();
        assert_eq!(done.backup, Some(pb::BackupUploaded { size_bytes: big.len() as u64 + 4 }));
        assert_eq!(uploaded.len(), big.len() + 4);
        assert!(uploaded.ends_with(b"tail"));
        assert_eq!(runtime.execs(), [(container, vec!["pg_dump".to_string()])]);
    }

    #[tokio::test]
    async fn a_failed_dump_uploads_nothing_and_says_why() {
        let runtime = MemoryRuntime::new();
        let container = database(&runtime);
        let store = Arc::new(InMemory::new());
        runtime.set_exec_output(vec![
            ExecOutput::Stdout(Bytes::from_static(b"partial")),
            ExecOutput::Stderr(Bytes::from_static(b"pg_dump: error: connection refused\n")),
            ExecOutput::Exited(1),
        ]);
        let stores = BackupStores::Fixed(store.clone());

        let failed = backup_database(&runtime, &stores, command(container, "k.pgdump"), &progress()).await.unwrap_err();
        let missing =
            backup_database(&runtime, &stores, command("other".to_string(), "k.pgdump"), &progress()).await.unwrap_err();

        assert_eq!(failed.reason(), FailureReason::BackupFailed);
        assert_eq!(failed.message, "the dump exited with 1: pg_dump: error: connection refused");
        assert!(store.get(&Path::from("k.pgdump")).await.is_err());
        assert!(missing.message.contains("no container `other`"), "{}", missing.message);
    }
}
