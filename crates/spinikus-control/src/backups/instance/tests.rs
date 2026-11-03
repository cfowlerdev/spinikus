//! Instance backups against an in-memory bucket.

use object_store::memory::InMemory;
use spinikus_store::{SqliteStore, connect, migrate};

use super::*;
use crate::{NewStorageDestination, test_support::user};

struct Fixture {
    _dir: tempfile::TempDir,
    control: Control,
    bucket: Arc<InMemory>,
    key_id: String,
}

/// A control plane that can back itself up (`with_master_key`) or not.
async fn fixture(with_master_key: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let pool = connect(&dir.path().join("spinikus.db")).await.unwrap();
    migrate(&pool).await.unwrap();
    let key = MasterKey::generate();
    let store = SqliteStore::open(pool, &key).await.unwrap();
    let bucket = Arc::new(InMemory::new());
    let mut control = Control::new(store).with_backup_store(bucket.clone());
    let key_id = key.id().to_string();
    if with_master_key {
        control = control.with_instance_backups(key, dir.path().join("work"));
    }
    Fixture { _dir: dir, control, bucket, key_id }
}

async fn destination(control: &Control, admin: &Principal) -> StorageDestinationId {
    let new = NewStorageDestination {
        name: "Offsite".to_string(),
        endpoint: Some("https://s3.example.com/".to_string()),
        region: None,
        bucket: "backups".to_string(),
        path_prefix: Some("prod".to_string()),
        path_style: true,
        access_key_id: "AKIA".to_string(),
        secret_access_key: Secret::new("s3cret"),
    };
    control.create_storage_destination(admin, new).await.unwrap().id
}

fn schedule(destination_id: StorageDestinationId, keep_last: u32, passphrase: Option<&str>) -> InstanceBackupScheduleInput {
    InstanceBackupScheduleInput {
        destination_id,
        cron: "0 3 * * *".to_string(),
        keep_last: Some(keep_last),
        enabled: true,
        passphrase: passphrase.map(Secret::new),
    }
}

const PASSPHRASE: &str = "a long enough passphrase";

async fn objects(bucket: &InMemory) -> Vec<String> {
    use tokio_stream::StreamExt;
    let mut keys: Vec<String> = bucket.list(None).map(|meta| meta.unwrap().location.to_string()).collect().await;
    keys.sort();
    keys
}

#[tokio::test]
async fn instance_admins_set_the_schedule() {
    // `_dir` stays bound: dropping it deletes the database while the pool may still connect.
    let Fixture { _dir, control, .. } = fixture(true).await;
    let admin = user(&control, "ada@example.com", true).await;
    let other = user(&control, "bob@example.com", false).await;
    let destination = destination(&control, &admin).await;

    assert!(control.get_instance_backup_schedule(&admin).await.unwrap().is_none());
    let refused = control.back_up_instance(&admin).await;
    assert!(matches!(refused, Err(ControlError::Rejected { .. })), "nothing to back up to yet: {refused:?}");
    for denied in [
        control.set_instance_backup_schedule(&other, 0, schedule(destination, 7, Some(PASSPHRASE))).await.err(),
        control.get_instance_backup_schedule(&other).await.err(),
        control.back_up_instance(&other).await.err(),
        control.list_instance_backups(&other, None, 10).await.err(),
    ] {
        assert!(matches!(denied, Some(ControlError::Forbidden)), "{denied:?}");
    }

    let invalid = |result: Result<InstanceBackupSchedule, ControlError>| match result {
        Err(ControlError::Invalid { field, .. }) => field,
        other => panic!("{other:?}"),
    };
    assert_eq!(invalid(control.set_instance_backup_schedule(&admin, 0, schedule(destination, 7, None)).await), "passphrase");
    assert_eq!(
        invalid(control.set_instance_backup_schedule(&admin, 0, schedule(destination, 7, Some("short"))).await),
        "passphrase"
    );
    assert_eq!(
        invalid(control.set_instance_backup_schedule(&admin, 0, schedule(destination, 0, Some(PASSPHRASE))).await),
        "keep_last"
    );
    let bad_cron = InstanceBackupScheduleInput { cron: "every day".to_string(), ..schedule(destination, 7, Some(PASSPHRASE)) };
    assert_eq!(invalid(control.set_instance_backup_schedule(&admin, 0, bad_cron).await), "cron");
    let nowhere = schedule(StorageDestinationId::new(), 7, Some(PASSPHRASE));
    assert_eq!(invalid(control.set_instance_backup_schedule(&admin, 0, nowhere).await), "destination_id");

    let set = control.set_instance_backup_schedule(&admin, 0, schedule(destination, 7, Some(PASSPHRASE))).await.unwrap();
    assert!(set.next_run_at.is_some_and(|next| next > Timestamp::now()));
    // An empty passphrase keeps the current one; turning it off clears the next run.
    let off = InstanceBackupScheduleInput { enabled: false, ..schedule(destination, 3, Some("")) };
    let off = control.set_instance_backup_schedule(&admin, set.revision, off).await.unwrap();
    assert_eq!((off.fields.keep_last, off.next_run_at), (3, None));
    assert_eq!(control.store().instance_backup_passphrase().await.unwrap().expose(), PASSPHRASE);
    let stale = control.set_instance_backup_schedule(&admin, set.revision, schedule(destination, 3, None)).await;
    assert!(matches!(stale, Err(ControlError::RevisionMismatch { .. })), "{stale:?}");
}

#[tokio::test]
async fn backs_up_now_and_on_schedule_and_keeps_the_newest() {
    let Fixture { _dir, control, bucket, key_id } = fixture(true).await;
    let admin = user(&control, "ada@example.com", true).await;
    let destination = destination(&control, &admin).await;
    let schedule = control.set_instance_backup_schedule(&admin, 0, schedule(destination, 2, Some(PASSPHRASE))).await.unwrap();

    // By hand.
    let manual = control.back_up_instance(&admin).await.unwrap();
    assert_eq!((manual.status, manual.scheduled), (BackupStatus::Queued, false));
    assert_eq!(manual.object_key, format!("prod/instance/{}.tar.gz.age", manual.id));
    control.advance_instance_backups().await;
    let manual = control.get_instance_backup(&admin, manual.id).await.unwrap();
    assert_eq!(manual.status, BackupStatus::Succeeded, "{:?}", manual.error);
    let uploaded = bucket.get(&Path::parse(&manual.object_key).unwrap()).await.unwrap().bytes().await.unwrap();
    assert_eq!(manual.size_bytes, Some(uploaded.len() as u64));

    // The file opens with the passphrase, as `spinikus admin restore` opens it.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("backup.tar.gz.age");
    std::fs::write(&file, &uploaded).unwrap();
    let opened = instance_backup::open_backup(&file, &Secret::new(PASSPHRASE), dir.path()).unwrap();
    assert_eq!(opened.master_key.id(), key_id);
    instance_backup::verify_database(&opened.database, &opened.master_key).await.unwrap();

    // Not due yet: nothing happens. Due: one is made, and the next run moves on.
    control.advance_instance_backups().await;
    assert_eq!(control.list_instance_backups(&admin, None, 10).await.unwrap().items.len(), 1);
    let mut scheduled = Vec::new();
    for _ in 0..3 {
        control.store().set_instance_backup_next_run(Some(Timestamp::now() - jiff::SignedDuration::from_mins(1))).await.unwrap();
        control.advance_instance_backups().await;
        let newest = control.list_instance_backups(&admin, None, 1).await.unwrap().items.remove(0);
        assert!(newest.scheduled && newest.status == BackupStatus::Succeeded, "{newest:?}");
        scheduled.push(newest);
    }
    let next = control.get_instance_backup_schedule(&admin).await.unwrap().unwrap().next_run_at;
    assert!(next.is_some_and(|next| next > Timestamp::now()) && next == schedule.next_run_at);

    // It keeps two of its own; the one made by hand isn't its to delete.
    let listed: Vec<_> = control.list_instance_backups(&admin, None, 10).await.unwrap().items.into_iter().map(|b| b.id).collect();
    assert_eq!(listed, [scheduled[2].id, scheduled[1].id, manual.id]);
    let mut kept = vec![scheduled[1].object_key.clone(), scheduled[2].object_key.clone(), manual.object_key.clone()];
    kept.sort();
    assert_eq!(objects(&bucket).await, kept);

    // Deleting one deletes its file.
    control.delete_instance_backup(&admin, manual.id).await.unwrap();
    assert!(!objects(&bucket).await.contains(&manual.object_key));
    assert!(matches!(control.get_instance_backup(&admin, manual.id).await, Err(ControlError::NotFound { .. })));
}

#[tokio::test]
async fn fails_without_a_master_key_and_after_a_restart() {
    let Fixture { _dir, control, bucket, .. } = fixture(false).await;
    let admin = user(&control, "ada@example.com", true).await;
    let destination = destination(&control, &admin).await;
    control.set_instance_backup_schedule(&admin, 0, schedule(destination, 2, Some(PASSPHRASE))).await.unwrap();

    let backup = control.back_up_instance(&admin).await.unwrap();
    control.advance_instance_backups().await;
    let failed = control.get_instance_backup(&admin, backup.id).await.unwrap();
    assert_eq!(failed.status, BackupStatus::Failed);
    assert!(failed.error.as_deref().is_some_and(|error| error.contains("no master key")), "{:?}", failed.error);
    assert!(objects(&bucket).await.is_empty());

    // One left running by a control plane that stopped fails, and can't be deleted until then.
    let running = control.back_up_instance(&admin).await.unwrap();
    control.store().start_instance_backup(running.id).await.unwrap();
    let refused = control.delete_instance_backup(&admin, running.id).await;
    assert!(matches!(refused, Err(ControlError::Rejected { .. })), "{refused:?}");
    control.fail_interrupted_instance_backups().await;
    let interrupted = control.get_instance_backup(&admin, running.id).await.unwrap();
    assert_eq!((interrupted.status, interrupted.error.as_deref()), (BackupStatus::Failed, Some("Spinikus stopped while it ran")));
    control.delete_instance_backup(&admin, running.id).await.unwrap();
}
