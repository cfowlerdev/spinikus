//! Database restores: the dump is downloaded from the destination and fed, as it arrives, to
//! the restore command in the database's container on stdin. The command decides what a
//! failure leaves behind (Postgres restores in one transaction, so nothing); a failed
//! download ends stdin early and is reported in place of the command's own error. Engines
//! that load their data only on start (Redis) have the container restarted next, and the
//! restore succeeds once the database answers its ready command.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use object_store::{ObjectStore, ObjectStoreExt, path::Path};
use spinikus_core::id::RestoreId;
use spinikus_proto::agent::v1::{self as pb, FailureReason};
use tokio_stream::StreamExt;

use crate::{
    backup::{BackupStores, exited, keep_stderr},
    commands::{DEFAULT_GRACE, Progress, failure, parse_id},
    runtime::{ContainerRuntime, ExecInput, ExecOutput, RuntimeError},
};

/// How long a restarted database has to answer its ready command: loading a large dump
/// takes a while.
const READY_TIMEOUT: Duration = Duration::from_secs(300);
/// How often the ready command runs until it passes.
const READY_INTERVAL: Duration = Duration::from_millis(500);

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
    let download = download(&*store, &path, &restore.object_key).await.map_err(restore_failed)?;
    let size = download.size;
    let (restart, ready_command) = (restore.restart, restore.ready_command);
    progress.phase("restoring");
    tracing::info!(restore = %restore_id, container = restore.container_id, size, "restoring a database");
    let (input, download_error) = download.into_input();
    let outcome = run(runtime, &restore.container_id, "the restore", restore.command, Some(input)).await;
    let download_error = download_error();
    let outcome = match (download_error, outcome) {
        (Some(error), _) => Err(format!("the download failed: {error}")),
        (None, Err(error)) => Err(error),
        (None, Ok(())) if restart => restart_until_ready(runtime, &restore.container_id, ready_command, progress).await,
        (None, Ok(())) => Ok(()),
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

/// A backup being downloaded, to feed a command's stdin as it arrives.
pub(crate) struct Download {
    stream: std::pin::Pin<Box<dyn tokio_stream::Stream<Item = object_store::Result<bytes::Bytes>> + Send>>,
    pub(crate) size: u64,
}

impl Download {
    /// The download as a command's input, which ends early if the download fails, and a way
    /// to ask afterwards whether it did and why.
    pub(crate) fn into_input(self) -> (ExecInput, impl FnOnce() -> Option<String>) {
        let error = Arc::new(Mutex::new(None));
        let failed = error.clone();
        let input = self.stream.map_while(move |chunk| match chunk {
            Ok(bytes) => Some(bytes),
            Err(error) => {
                *failed.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error.to_string());
                None
            }
        });
        let error = move || error.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
        (Box::pin(input), error)
    }
}

/// Starts downloading `object_key`.
pub(crate) async fn download(store: &dyn ObjectStore, path: &Path, object_key: &str) -> Result<Download, String> {
    match store.get(path).await {
        Ok(download) => {
            let size = download.meta.size;
            Ok(Download { stream: download.into_stream(), size })
        }
        Err(object_store::Error::NotFound { .. }) => Err(format!("the backup `{object_key}` is not in the bucket")),
        Err(error) => Err(format!("could not download the backup: {error}")),
    }
}

/// Restarts the container, so the database loads the restored data, then runs the ready
/// command until it passes.
async fn restart_until_ready(
    runtime: &dyn ContainerRuntime,
    container_id: &str,
    ready_command: Vec<String>,
    progress: &Progress,
) -> Result<(), String> {
    progress.phase("restarting");
    match runtime.restart(container_id, DEFAULT_GRACE).await {
        Ok(()) => {}
        Err(RuntimeError::NotFound) => return Err(format!("the container `{container_id}` is gone; it was not restarted")),
        Err(error) => return Err(format!("the data is in place, but the container did not restart: {error}")),
    }
    if ready_command.is_empty() {
        return Ok(());
    }
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    loop {
        let Err(problem) = run(runtime, container_id, "the ready check", ready_command.clone(), None).await else {
            return Ok(());
        };
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "the data is in place, but the database did not answer within {}s of its restart: {problem}",
                READY_TIMEOUT.as_secs()
            ));
        }
        tokio::time::sleep(READY_INTERVAL).await;
    }
}

/// Runs a command in the container to its end; `what` names it in errors.
pub(crate) async fn run(
    runtime: &dyn ContainerRuntime,
    container_id: &str,
    what: &str,
    command: Vec<String>,
    input: Option<ExecInput>,
) -> Result<(), String> {
    let mut output = runtime.exec(container_id, command, input);
    let mut stderr = Vec::new();
    loop {
        match output.next().await {
            Some(Ok(ExecOutput::Stdout(_))) => {}
            Some(Ok(ExecOutput::Stderr(bytes))) => keep_stderr(&mut stderr, &bytes),
            Some(Ok(ExecOutput::Exited(0))) => return Ok(()),
            Some(Ok(ExecOutput::Exited(code))) => return Err(exited(what, code, &stderr)),
            Some(Err(error)) => return Err(format!("{what} failed: {error}")),
            None => return Err(format!("{what} ended without an exit code")),
        }
    }
}

pub(crate) fn restore_failed(message: impl Into<String>) -> pb::CommandFailed {
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
            ..Default::default()
        }
    }

    /// A restore that restarts the container and checks it with `ready`.
    fn restarting(container_id: String, key: &str) -> pb::RestoreDatabase {
        pb::RestoreDatabase { restart: true, ready_command: vec!["ready".to_string()], ..command(container_id, key) }
    }

    fn not_ready() -> Vec<ExecOutput> {
        vec![ExecOutput::Stdout(Bytes::from_static(b"LOADING\n")), ExecOutput::Exited(1)]
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

    #[tokio::test(start_paused = true)]
    async fn restarts_the_database_and_waits_until_it_answers() {
        let runtime = MemoryRuntime::new();
        let container = database(&runtime);
        let store = Arc::new(InMemory::new());
        store.put(&Path::from("k.rdb"), PutPayload::from_static(b"REDIS0012")).await.unwrap();
        let stores = BackupStores::Fixed(store);
        runtime.queue_exec_output(vec![ExecOutput::Exited(0)]);
        runtime.queue_exec_output(not_ready());
        runtime.queue_exec_output(not_ready());

        restore_database(&runtime, &stores, restarting(container.clone(), "k.rdb"), &progress()).await.unwrap();

        let commands: Vec<_> = runtime.execs().into_iter().map(|(_, command)| command[0].clone()).collect();
        assert_eq!(commands, ["pg_restore", "ready", "ready", "ready"]);
        assert!(runtime.inspect(&container).await.unwrap().unwrap().started_at.is_some(), "restarted");
    }

    #[tokio::test(start_paused = true)]
    async fn fails_when_the_database_does_not_come_back() {
        let runtime = MemoryRuntime::new();
        let container = database(&runtime);
        let store = Arc::new(InMemory::new());
        store.put(&Path::from("k.rdb"), PutPayload::from_static(b"REDIS0012")).await.unwrap();
        let stores = BackupStores::Fixed(store);
        runtime.queue_exec_output(vec![ExecOutput::Exited(0)]);
        runtime.set_exec_output(not_ready());
        let failing = BackupStores::Fixed(Arc::new(InMemory::new()));

        let failed = restore_database(&runtime, &stores, restarting(container.clone(), "k.rdb"), &progress()).await.unwrap_err();
        let missing = restore_database(&runtime, &failing, restarting(container, "k.rdb"), &progress()).await.unwrap_err();

        assert_eq!(failed.reason(), FailureReason::RestoreFailed);
        assert!(failed.message.starts_with("the data is in place, but the database did not answer"), "{}", failed.message);
        assert!(failed.message.ends_with("the ready check exited with 1"), "{}", failed.message);
        assert!(missing.message.contains("not in the bucket"), "{}", missing.message);
        let restarts = runtime.execs().iter().filter(|(_, command)| command[0] == "pg_restore").count();
        assert_eq!(restarts, 1, "nothing runs, and nothing restarts, without a dump");
    }
}
