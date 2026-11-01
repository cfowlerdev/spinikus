//! Database restores: the dump is downloaded from the destination and fed, as it arrives, to
//! the restore command in the database's container on stdin. The command decides what a
//! failure leaves behind (Postgres restores in one transaction, so nothing); a failed
//! download ends stdin early and is reported in place of the command's own error.

use std::sync::{Arc, Mutex};

use object_store::{ObjectStoreExt, path::Path};
use spinikus_core::id::RestoreId;
use spinikus_proto::agent::v1::{self as pb, FailureReason};
use tokio_stream::StreamExt;

use crate::{
    backup::{BackupStores, exited, keep_stderr},
    commands::{Progress, failure, parse_id},
    runtime::{ContainerRuntime, ExecOutput},
};

pub(crate) async fn restore_database(
    runtime: &dyn ContainerRuntime,
    stores: &BackupStores,
    restore: pb::RestoreDatabase,
    progress: &Progress,
) -> Result<pb::CommandSucceeded, pb::CommandFailed> {
    let restore_id: RestoreId = parse_id("restore ID", &restore.restore_id)?;
    if restore.command.is_empty() {
        return Err(restore_failed("no restore command"));
    }
    let destination = restore.destination.as_ref().ok_or_else(|| restore_failed("no storage destination"))?;
    let path = Path::parse(&restore.object_key).map_err(|error| restore_failed(format!("invalid object key: {error}")))?;
    if runtime.inspect(&restore.container_id).await?.is_none() {
        return Err(restore_failed(format!("no container `{}` managed by Spinikus", restore.container_id)));
    }
    let store = stores.store(destination).map_err(|failed| restore_failed(failed.message))?;

    progress.phase("downloading");
    let download = match store.get(&path).await {
        Ok(download) => download,
        Err(object_store::Error::NotFound { .. }) => {
            return Err(restore_failed(format!("the backup `{}` is not in the bucket", restore.object_key)));
        }
        Err(error) => return Err(restore_failed(format!("could not download the backup: {error}"))),
    };
    let size = download.meta.size;
    progress.phase("restoring");
    tracing::info!(restore = %restore_id, container = restore.container_id, size, "restoring a database");
    // A failed download ends the input early and is remembered here.
    let download_error = Arc::new(Mutex::new(None));
    let failed_download = download_error.clone();
    let input = download.into_stream().map_while(move |chunk| match chunk {
        Ok(bytes) => Some(bytes),
        Err(error) => {
            *failed_download.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error.to_string());
            None
        }
    });
    let mut output = runtime.exec(&restore.container_id, restore.command, Some(Box::pin(input)));
    let mut stderr = Vec::new();
    let outcome = loop {
        match output.next().await {
            Some(Ok(ExecOutput::Stdout(_))) => {}
            Some(Ok(ExecOutput::Stderr(bytes))) => keep_stderr(&mut stderr, &bytes),
            Some(Ok(ExecOutput::Exited(0))) => break Ok(()),
            Some(Ok(ExecOutput::Exited(code))) => break Err(exited("the restore", code, &stderr)),
            Some(Err(error)) => break Err(format!("the restore failed: {error}")),
            None => break Err("the restore ended without an exit code".to_string()),
        }
    };
    let download_error = download_error.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
    let outcome = match download_error {
        Some(error) => Err(format!("the download failed: {error}")),
        None => outcome,
    };
    match outcome {
        Ok(()) => {
            progress.line(format!("restored {size} bytes from {}", restore.object_key));
            tracing::info!(restore = %restore_id, "restored a database");
            Ok(pb::CommandSucceeded::default())
        }
        Err(message) => {
            tracing::warn!(restore = %restore_id, message, "a restore failed");
            Err(restore_failed(message))
        }
    }
}

fn restore_failed(message: impl Into<String>) -> pb::CommandFailed {
    failure(FailureReason::RestoreFailed, message)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use object_store::{PutPayload, memory::InMemory};
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

    fn command(container_id: String, key: &str) -> pb::RestoreDatabase {
        pb::RestoreDatabase {
            restore_id: RestoreId::new().to_string(),
            container_id,
            command: vec!["pg_restore".to_string()],
            destination: Some(pb::StorageDestination { bucket: "b".to_string(), ..Default::default() }),
            object_key: key.to_string(),
        }
    }

    fn progress() -> Progress {
        Progress::new("cmd".to_string(), mpsc::channel(16).0)
    }

    #[tokio::test]
    async fn feeds_the_dump_to_the_restore_command() {
        let runtime = MemoryRuntime::new();
        let container = database(&runtime);
        let store = Arc::new(InMemory::new());
        let dump = vec![9u8; 3 * 1024 * 1024];
        store.put(&Path::from("svc/1.pgdump"), PutPayload::from(dump.clone())).await.unwrap();
        let stores = BackupStores::Fixed(store);

        restore_database(&runtime, &stores, command(container.clone(), "svc/1.pgdump"), &progress()).await.unwrap();

        assert_eq!(runtime.execs(), [(container, vec!["pg_restore".to_string()])]);
        assert_eq!(runtime.exec_inputs(), [dump]);
    }

    #[tokio::test]
    async fn reports_a_missing_dump_or_a_failed_restore() {
        let runtime = MemoryRuntime::new();
        let container = database(&runtime);
        let store = Arc::new(InMemory::new());
        store.put(&Path::from("k.pgdump"), PutPayload::from_static(b"PGDMP")).await.unwrap();
        let stores = BackupStores::Fixed(store);
        runtime.set_exec_output(vec![
            ExecOutput::Stderr(Bytes::from_static(b"pg_restore: error: relation \"notes\" is locked\n")),
            ExecOutput::Exited(1),
        ]);

        let missing = restore_database(&runtime, &stores, command(container.clone(), "gone.pgdump"), &progress()).await;
        let failed = restore_database(&runtime, &stores, command(container, "k.pgdump"), &progress()).await.unwrap_err();

        let missing = missing.unwrap_err();
        assert_eq!(missing.reason(), FailureReason::RestoreFailed);
        assert!(missing.message.contains("not in the bucket"), "{}", missing.message);
        assert_eq!(failed.reason(), FailureReason::RestoreFailed);
        assert_eq!(failed.message, "the restore exited with 1: pg_restore: error: relation \"notes\" is locked");
        assert_eq!(runtime.execs().len(), 1, "nothing runs without a dump");
    }
}
