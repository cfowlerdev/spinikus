//! Volume backups and restores against the in-memory runtime and bucket.

use std::sync::Arc;

use bytes::Bytes;
use object_store::{ObjectStoreExt, PutPayload, memory::InMemory, path::Path};
use spinikus_core::{
    id::DeploymentId,
    server::{InstanceHealth, InstanceStatus, ObservedInstance},
};
use spinikus_proto::agent::v1::FailureReason;
use tokio::sync::mpsc;

use super::*;
use crate::runtime::{ExecOutput, MemoryRuntime};

fn progress() -> Progress {
    Progress::new("cmd".to_string(), mpsc::channel(16).0)
}

fn destination() -> Option<pb::StorageDestination> {
    Some(pb::StorageDestination { bucket: "b".to_string(), ..Default::default() })
}

fn backup(service: ServiceId, volume: VolumeId, key: &str) -> pb::BackupVolume {
    pb::BackupVolume {
        backup_id: BackupId::new().to_string(),
        service_id: service.to_string(),
        volume_id: volume.to_string(),
        image: "busybox:1.37.0".to_string(),
        command: vec!["tar".to_string(), "-czf".to_string(), "-".to_string()],
        destination: destination(),
        object_key: key.to_string(),
    }
}

fn restore(service: ServiceId, volume: VolumeId, key: &str, container_id: &str) -> pb::RestoreVolume {
    pb::RestoreVolume {
        restore_id: RestoreId::new().to_string(),
        service_id: service.to_string(),
        volume_id: volume.to_string(),
        image: "busybox:1.37.0".to_string(),
        command: vec!["/bin/sh".to_string(), "-c".to_string(), "unpack".to_string()],
        destination: destination(),
        object_key: key.to_string(),
        container_id: container_id.to_string(),
        grace_period: None,
    }
}

/// The service's running container.
fn app(runtime: &MemoryRuntime, service: ServiceId) -> String {
    runtime.put(ObservedInstance {
        container_id: "web".to_string(),
        service_id: service,
        deployment_id: DeploymentId::new(),
        status: InstanceStatus::Running,
        health: InstanceHealth::None,
        restart_count: 0,
        started_at: None,
    });
    "web".to_string()
}

#[tokio::test]
async fn archives_the_volume_in_a_read_only_helper_and_removes_it() {
    let runtime = MemoryRuntime::new();
    let store = Arc::new(InMemory::new());
    let stores = BackupStores::Fixed(store.clone());
    let (service, volume) = (ServiceId::new(), VolumeId::new());
    runtime.set_exec_output(vec![ExecOutput::Stdout(Bytes::from_static(b"archive")), ExecOutput::Exited(0)]);

    let command = backup(service, volume, "svc/1.tar.gz");
    let name = format!("spinikus-{}-helper", command.backup_id);
    let done = backup_volume(&runtime, &stores, command, &progress()).await.unwrap();

    let uploaded = store.get(&Path::from("svc/1.tar.gz")).await.unwrap().bytes().await.unwrap();
    let helpers = runtime.helpers();
    assert_eq!(done.backup, Some(pb::BackupUploaded { size_bytes: 7 }));
    assert_eq!(&uploaded[..], b"archive");
    assert_eq!(helpers.len(), 1);
    assert_eq!(
        (helpers[0].volume.clone(), helpers[0].mount.as_str(), helpers[0].read_only),
        (volume.to_string(), "/volume", true)
    );
    assert_eq!((helpers[0].service_id, helpers[0].image.as_str()), (service, "busybox:1.37.0"));
    assert_eq!(runtime.removed_one_offs(), [name]);
    assert_eq!(runtime.execs()[0].1, ["tar", "-czf", "-"]);
}

#[tokio::test]
async fn a_failed_archive_uploads_nothing_and_a_foreign_volume_is_refused() {
    let runtime = MemoryRuntime::new();
    let store = Arc::new(InMemory::new());
    let stores = BackupStores::Fixed(store.clone());
    let service = ServiceId::new();
    runtime.set_exec_output(vec![
        ExecOutput::Stdout(Bytes::from_static(b"part")),
        ExecOutput::Stderr(Bytes::from_static(b"tar: read error\n")),
        ExecOutput::Exited(1),
    ]);

    let failed = backup_volume(&runtime, &stores, backup(service, VolumeId::new(), "k.tar.gz"), &progress()).await.unwrap_err();
    let mut foreign = backup(service, VolumeId::new(), "k.tar.gz");
    foreign.volume_id = "postgres-data".to_string();
    let refused = backup_volume(&runtime, &stores, foreign, &progress()).await.unwrap_err();

    assert_eq!(failed.reason(), FailureReason::BackupFailed);
    assert_eq!(failed.message, "the archive exited with 1: tar: read error");
    assert!(store.get(&Path::from("k.tar.gz")).await.is_err(), "the upload was aborted");
    assert_eq!(runtime.removed_one_offs().len(), 1, "removed after failing too");
    assert!(refused.message.contains("invalid volume ID `postgres-data`"), "{}", refused.message);
}

#[tokio::test]
async fn restores_with_the_container_stopped_and_starts_it_again() {
    let runtime = MemoryRuntime::new();
    let store = Arc::new(InMemory::new());
    store.put(&Path::from("svc/1.tar.gz"), PutPayload::from_static(b"the archive")).await.unwrap();
    let stores = BackupStores::Fixed(store.clone());
    let (service, volume) = (ServiceId::new(), VolumeId::new());
    let container = app(&runtime, service);

    let done = restore_volume(&runtime, &stores, restore(service, volume, "svc/1.tar.gz", &container), &progress()).await;
    let after_success = runtime.get(&container).unwrap().status;
    runtime.set_exec_output(vec![ExecOutput::Stderr(Bytes::from_static(b"tar: invalid magic\n")), ExecOutput::Exited(1)]);
    let failed = restore_volume(&runtime, &stores, restore(service, volume, "svc/1.tar.gz", &container), &progress()).await;
    let after_failure = runtime.get(&container).unwrap().status;
    let missing = restore_volume(&runtime, &stores, restore(service, volume, "gone.tar.gz", ""), &progress()).await;
    let other_service =
        restore_volume(&runtime, &stores, restore(ServiceId::new(), volume, "svc/1.tar.gz", &container), &progress()).await;

    done.unwrap();
    assert_eq!(runtime.exec_inputs()[0], b"the archive");
    let helpers = runtime.helpers();
    assert!(!helpers[0].read_only, "a restore writes");
    assert_eq!(after_success, InstanceStatus::Running);
    assert_eq!(failed.unwrap_err().message, "the restore exited with 1: tar: invalid magic");
    assert_eq!(after_failure, InstanceStatus::Running, "started again after a failure");
    assert_eq!(missing.unwrap_err().message, "the backup `gone.tar.gz` is not in the bucket");
    assert!(other_service.unwrap_err().message.contains("no container `web` of the service"));
    assert_eq!(runtime.helpers().len(), 2, "no helper without a download");
}
