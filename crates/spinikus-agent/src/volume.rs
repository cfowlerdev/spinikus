//! Volume backups and restores, in a helper container that mounts the volume: the control
//! plane names the image and the command, which archives the volume to stdout or unpacks
//! the archive on stdin. The archive streams to and from the destination like a database
//! dump. The helper is always removed afterwards.
//!
//! A restore stops the service's container first, if the control plane names one, so the
//! app doesn't write to the volume while it changes, and starts it again afterwards,
//! whether the restore succeeded or not.

use spinikus_core::{
    backup::HELPER_MOUNT,
    id::{BackupId, RestoreId, ServiceId, VolumeId},
};
use spinikus_proto::agent::v1 as pb;

use crate::{
    backup::{BackupStores, backup_failed, upload, uploaded},
    commands::{Progress, grace, parse_id},
    restore::{download, restore_failed, run},
    runtime::{ContainerRuntime, HelperSpec, RuntimeError},
};

pub(crate) async fn backup_volume(
    runtime: &dyn ContainerRuntime,
    stores: &BackupStores,
    backup: pb::BackupVolume,
    progress: &Progress,
) -> Result<pb::CommandSucceeded, pb::CommandFailed> {
    let backup_id: BackupId = parse_id("backup ID", &backup.backup_id)?;
    let service_id: ServiceId = parse_id("service ID", &backup.service_id)?;
    // Only Spinikus's own volumes, which are named after their IDs.
    let volume: VolumeId = parse_id("volume ID", &backup.volume_id)?;
    if backup.command.is_empty() || backup.image.is_empty() {
        return Err(backup_failed("no image or command"));
    }
    let destination = backup.destination.as_ref().ok_or_else(|| backup_failed("no storage destination"))?;
    let path = object_store::path::Path::parse(&backup.object_key)
        .map_err(|error| backup_failed(format!("invalid object key: {error}")))?;
    let store = stores.store(destination)?;

    progress.phase("backing up");
    tracing::info!(backup = %backup_id, %volume, "backing up a volume");
    let spec = helper(&backup_id.to_string(), &backup.image, service_id, volume, true);
    let helper_id = runtime.start_helper(&spec).await.map_err(|error| backup_failed(helper_failed(error)))?;
    let output = runtime.exec(&helper_id, backup.command, None);
    let outcome = upload(output, &*store, &path, "the archive").await;
    remove(runtime, &spec.name).await;
    match outcome {
        Ok(size) => {
            progress.line(format!("uploaded {size} bytes to {}", backup.object_key));
            tracing::info!(backup = %backup_id, size, "backed up a volume");
            Ok(uploaded(size))
        }
        Err(message) => {
            tracing::warn!(backup = %backup_id, message, "a volume backup failed");
            Err(backup_failed(message))
        }
    }
}

pub(crate) async fn restore_volume(
    runtime: &dyn ContainerRuntime,
    stores: &BackupStores,
    restore: pb::RestoreVolume,
    progress: &Progress,
) -> Result<pb::CommandSucceeded, pb::CommandFailed> {
    let restore_id: RestoreId = parse_id("restore ID", &restore.restore_id)?;
    let service_id: ServiceId = parse_id("service ID", &restore.service_id)?;
    let volume: VolumeId = parse_id("volume ID", &restore.volume_id)?;
    if restore.command.is_empty() || restore.image.is_empty() {
        return Err(restore_failed("no image or command"));
    }
    let destination = restore.destination.as_ref().ok_or_else(|| restore_failed("no storage destination"))?;
    let path = object_store::path::Path::parse(&restore.object_key)
        .map_err(|error| restore_failed(format!("invalid object key: {error}")))?;
    let container = match restore.container_id.as_str() {
        "" => None,
        id => match runtime.inspect(id).await? {
            Some(instance) if instance.service_id == service_id => Some(id.to_string()),
            _ => return Err(restore_failed(format!("no container `{id}` of the service managed by Spinikus"))),
        },
    };
    let store = stores.store(destination).map_err(|failed| restore_failed(failed.message))?;

    progress.phase("downloading");
    let download = download(&*store, &path, &restore.object_key).await.map_err(restore_failed)?;
    let size = download.size;
    if let Some(container) = &container {
        progress.phase("stopping");
        runtime.stop(container, grace(restore.grace_period)).await?;
    }
    progress.phase("restoring");
    tracing::info!(restore = %restore_id, %volume, size, "restoring a volume");
    let spec = helper(&restore_id.to_string(), &restore.image, service_id, volume, false);
    let outcome = match runtime.start_helper(&spec).await {
        Ok(helper_id) => {
            let (input, download_error) = download.into_input();
            let outcome = run(runtime, &helper_id, "the restore", restore.command, Some(input)).await;
            match (download_error(), outcome) {
                (Some(error), _) => Err(format!("the download failed: {error}")),
                (None, outcome) => outcome,
            }
        }
        Err(error) => Err(helper_failed(error)),
    };
    remove(runtime, &spec.name).await;
    // Running again either way: a failed restore left the data as it was.
    let started = match &container {
        Some(container) => {
            progress.phase("starting");
            runtime.start(container).await.map_err(|error| format!("the container did not start again: {error}"))
        }
        None => Ok(()),
    };
    match outcome.and(started) {
        Ok(()) => {
            progress.line(format!("restored {size} bytes from {}", restore.object_key));
            tracing::info!(restore = %restore_id, "restored a volume");
            Ok(pb::CommandSucceeded::default())
        }
        Err(message) => {
            tracing::warn!(restore = %restore_id, message, "a volume restore failed");
            Err(restore_failed(message))
        }
    }
}

/// The helper of a backup or restore, `run` its ID.
fn helper(run: &str, image: &str, service_id: ServiceId, volume: VolumeId, read_only: bool) -> HelperSpec {
    HelperSpec {
        name: format!("spinikus-{run}-helper"),
        image: image.to_string(),
        service_id,
        volume: volume.to_string(),
        mount: HELPER_MOUNT.to_string(),
        read_only,
        label: run.to_string(),
    }
}

fn helper_failed(error: RuntimeError) -> String {
    format!("the helper container did not start: {error}")
}

async fn remove(runtime: &dyn ContainerRuntime, name: &str) {
    if let Err(error) = runtime.remove_one_off(name).await {
        tracing::warn!(helper = name, %error, "could not remove a helper container");
    }
}

#[cfg(test)]
mod tests;
