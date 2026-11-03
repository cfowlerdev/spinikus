//! `spinikus admin backup` and `admin restore`: the whole instance, its database and master
//! key, in one file encrypted with a passphrase (`spinikus_store::instance_backup`).
//!
//! A backup is taken while the server runs. A restore needs it stopped (it takes the lock
//! `server run` holds), keeps the current database and key beside the restored ones as
//! `*.before-restore-<time>`, and applies newer migrations, so a backup restores into the
//! version that made it or a newer one. Both may run as root: files they leave in the data
//! directory are given to its owner.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use jiff::Timestamp;
use spinikus_core::{config::SpinikusConfig, secret::Secret};
use spinikus_store::{
    MasterKey, SqliteStore,
    instance_backup::{self, BackupManifest},
};

use crate::{
    cli::{BackupArgs, RestoreArgs},
    secrets, server,
};

/// Environment variable to pass the passphrase non-interactively, such as from a cron job.
pub const PASSPHRASE_VAR: &str = "SPINIKUS_BACKUP_PASSPHRASE";

/// The shortest passphrase a backup takes.
const MIN_PASSPHRASE_LEN: usize = 12;

pub async fn backup(config: &SpinikusConfig, args: BackupArgs) -> anyhow::Result<()> {
    let passphrase = new_passphrase()?;
    let out = args.file.unwrap_or_else(|| PathBuf::from(format!("spinikus-backup-{}.tar.gz.age", stamp(Timestamp::now()))));
    let manifest = backup_to(config, &out, &passphrase).await?;
    println!(
        "Backed up Spinikus {} (database schema {}, master key {}) to {}",
        manifest.spinikus_version,
        manifest.schema_version,
        manifest.master_key_id,
        out.display()
    );
    println!("Keep the passphrase somewhere safe: the backup cannot be restored without it.");
    Ok(())
}

/// Writes a backup of the instance to `out`.
pub async fn backup_to(config: &SpinikusConfig, out: &Path, passphrase: &Secret) -> anyhow::Result<BackupManifest> {
    let path = config.database_path();
    if !path.is_file() {
        bail!("There is no database at {}, so nothing to back up", path.display());
    }
    if out.exists() {
        bail!("{} already exists", out.display());
    }
    let key = secrets::existing_master_key(config)?;
    // No migrations: the server may be running an older version than this binary.
    let pool = spinikus_store::connect(&path).await.with_context(|| format!("Failed to open {}", path.display()))?;
    let result = async {
        SqliteStore::open(pool.clone(), &key).await.context("The master key does not open this database's secrets")?;
        instance_backup::create_backup(&pool, &key, env!("CARGO_PKG_VERSION"), passphrase, out)
            .await
            .with_context(|| format!("Failed to write the backup to {}", out.display()))
    }
    .await;
    pool.close().await;
    give_to_owner(&config.data_dir, &database_files(&path));
    result
}

/// What a restore did.
#[derive(Debug)]
pub struct Restored {
    pub manifest: BackupManifest,
    /// The previous database's files, set aside.
    pub previous_database: Vec<PathBuf>,
    /// The previous master key file, set aside, if it was another key.
    pub previous_key: Option<PathBuf>,
}

pub async fn restore(config: &SpinikusConfig, args: RestoreArgs) -> anyhow::Result<()> {
    // Before asking for anything, so a running server is reported first.
    let lock = server::lock_instance(config)?;
    if !args.file.is_file() {
        bail!("There is no file {}", args.file.display());
    }
    let passphrase = read_passphrase("Passphrase: ")?;
    let restored = restore_locked(config, &args.file, &passphrase).await?;
    drop(lock);
    let manifest = &restored.manifest;
    println!(
        "Restored the backup made {} by Spinikus {} (master key {}).",
        manifest.created_at.strftime("%Y-%m-%d %H:%M UTC"),
        manifest.spinikus_version,
        manifest.master_key_id
    );
    for path in restored.previous_database.iter().chain(&restored.previous_key) {
        println!("Kept the previous {}", path.display());
    }
    println!(
        "Start Spinikus again (sudo systemctl start spinikus). Agents on other servers reconnect once it is reachable at the address they joined."
    );
    Ok(())
}

/// Restores the backup in `file`, with the server stopped.
pub async fn restore_from(config: &SpinikusConfig, file: &Path, passphrase: &Secret) -> anyhow::Result<Restored> {
    let _lock = server::lock_instance(config)?;
    restore_locked(config, file, passphrase).await
}

async fn restore_locked(config: &SpinikusConfig, file: &Path, passphrase: &Secret) -> anyhow::Result<Restored> {
    let database = config.database_path();
    let dir = database.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    // Next to the database, so it moves into place with a rename.
    let work = tempfile::Builder::new().prefix(".spinikus-restore-").tempdir_in(dir)?;
    let (from, into, phrase) = (file.to_path_buf(), work.path().to_path_buf(), passphrase.clone());
    let opened = tokio::task::spawn_blocking(move || instance_backup::open_backup(&from, &phrase, &into))
        .await?
        .with_context(|| format!("Failed to open the backup {}", file.display()))?;
    let manifest = opened.manifest.clone();
    let known = instance_backup::known_schema_version();
    if manifest.schema_version > known {
        bail!(
            "The backup was made by Spinikus {}, whose database is newer than this version ({}) knows; install that version or a newer one first",
            manifest.spinikus_version,
            env!("CARGO_PKG_VERSION")
        );
    }
    instance_backup::verify_database(&opened.database, &opened.master_key)
        .await
        .context("The backup's database can't be used")?;

    let at = stamp(Timestamp::now());
    let previous_key = place_master_key(config, &opened.master_key, &at)?;
    let mut previous_database = Vec::new();
    for path in database_files(&database) {
        let aside = before_restore(&path, &at);
        match fs::rename(&path, &aside) {
            Ok(()) => previous_database.push(aside),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("Failed to move {} aside", path.display())),
        }
    }
    fs::rename(&opened.database, &database)
        .with_context(|| format!("Failed to move the restored database to {}", database.display()))?;

    // Bring it up to this version's schema, and check it opens as the server will open it.
    let pool = server::open_database(config).await?;
    let opens = SqliteStore::open(pool.clone(), &opened.master_key).await;
    pool.close().await;
    opens.context("The restored database does not open with the restored master key")?;
    let mut owned = database_files(&database).to_vec();
    owned.push(config.master_key_path());
    owned.push(config.data_dir.join(server::LOCK_FILE));
    give_to_owner(&config.data_dir, &owned);
    Ok(Restored { manifest, previous_database, previous_key })
}

/// Makes the backup's key the instance's: written to the key file, the previous one (if it is
/// another key) kept beside it. Refused when the key comes from the configuration, which can't
/// be written, and is another key.
fn place_master_key(config: &SpinikusConfig, key: &MasterKey, at: &str) -> anyhow::Result<Option<PathBuf>> {
    if let Some(encoded) = &config.secrets.master_key {
        let configured = MasterKey::from_base64(encoded.trim()).context("Invalid secrets.master_key")?;
        if configured.id() == key.id() {
            return Ok(None);
        }
        bail!(
            "This instance takes its master key from secrets.master_key (or SPINIKUS_SECRETS__MASTER_KEY), which is not the backup's key ({}); remove that setting, so the backup's key is written to {}, and restore again",
            key.id(),
            config.master_key_path().display()
        );
    }
    let path = config.master_key_path();
    let previous = match fs::read_to_string(&path) {
        Ok(text) if MasterKey::from_base64(text.trim()).is_ok_and(|current| current.id() == key.id()) => return Ok(None),
        Ok(_) => {
            let aside = before_restore(&path, at);
            fs::rename(&path, &aside).with_context(|| format!("Failed to move {} aside", path.display()))?;
            Some(aside)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("Failed to read {}", path.display())),
    };
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&path).with_context(|| format!("Failed to create {}", path.display()))?;
    io::Write::write_all(&mut file, format!("{}\n", key.to_base64().as_str()).as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(previous)
}

/// The database file and SQLite's write-ahead log and shared memory beside it, which belong
/// with it.
fn database_files(database: &Path) -> [PathBuf; 3] {
    let with = |suffix: &str| {
        let mut name = database.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    [database.to_path_buf(), with("-wal"), with("-shm")]
}

fn before_restore(path: &Path, at: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".before-restore-{at}"));
    PathBuf::from(name)
}

/// A time for file names, such as `20261103T091500Z`.
fn stamp(at: Timestamp) -> String {
    at.strftime("%Y%m%dT%H%M%SZ").to_string()
}

/// Gives files that exist to the owner of `dir`, the data directory, so a command run as root
/// leaves nothing the server's user can't open. Only warns when it can't.
#[cfg(unix)]
fn give_to_owner(dir: &Path, paths: &[PathBuf]) {
    use std::os::unix::fs::MetadataExt;

    let Ok(owner) = fs::metadata(dir) else { return };
    for path in paths {
        let Ok(metadata) = fs::metadata(path) else { continue };
        if (metadata.uid(), metadata.gid()) != (owner.uid(), owner.gid())
            && let Err(error) = std::os::unix::fs::chown(path, Some(owner.uid()), Some(owner.gid()))
        {
            tracing::warn!(path = %path.display(), %error, "Failed to give the file to the data directory's owner");
        }
    }
}

#[cfg(not(unix))]
fn give_to_owner(_dir: &Path, _paths: &[PathBuf]) {}

/// A passphrase for a new backup: from the environment, or typed twice.
fn new_passphrase() -> anyhow::Result<Secret> {
    let passphrase = match std::env::var(PASSPHRASE_VAR) {
        Ok(passphrase) => Secret::new(passphrase),
        Err(_) => {
            let passphrase = read_passphrase("Passphrase for the backup: ")?;
            let again = Secret::new(rpassword::prompt_password("Repeat it: ").context("Failed to read the passphrase")?);
            if passphrase != again {
                bail!("The passphrases do not match");
            }
            passphrase
        }
    };
    if passphrase.expose().chars().count() < MIN_PASSPHRASE_LEN {
        bail!("The passphrase must be at least {MIN_PASSPHRASE_LEN} characters long");
    }
    Ok(passphrase)
}

fn read_passphrase(prompt: &str) -> anyhow::Result<Secret> {
    match std::env::var(PASSPHRASE_VAR) {
        Ok(passphrase) => Ok(Secret::new(passphrase)),
        Err(_) => Ok(Secret::new(rpassword::prompt_password(prompt).context("Failed to read the passphrase")?)),
    }
}

#[cfg(test)]
mod tests {
    use spinikus_control::Control;

    use super::*;

    fn config(dir: &Path) -> SpinikusConfig {
        SpinikusConfig { data_dir: dir.to_path_buf(), ..Default::default() }
    }

    async fn instance(dir: &Path, admin: &str) -> Control {
        let control = server::open_control(&config(dir)).await.unwrap();
        control.create_first_admin(admin, "Ada", Secret::new("correct horse battery staple")).await.unwrap();
        control
    }

    async fn admin_email(dir: &Path) -> String {
        let control = server::open_control(&config(dir)).await.unwrap();
        let store = control.store();
        let pool = store.pool();
        sqlx::query_scalar("SELECT email FROM users").fetch_one(pool).await.unwrap()
    }

    #[tokio::test]
    async fn restores_a_backup_over_another_instance_and_keeps_the_old_one() {
        let (one, two, files) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let passphrase = Secret::new("a long enough passphrase");
        instance(one.path(), "ada@example.com").await;
        instance(two.path(), "bob@example.com").await;
        let out = files.path().join("backup.tar.gz.age");

        let manifest = backup_to(&config(one.path()), &out, &passphrase).await.unwrap();
        // The server can be running: the lock doesn't stop a backup.
        let lock = server::lock_instance(&config(one.path())).unwrap();
        backup_to(&config(one.path()), &files.path().join("second"), &passphrase).await.unwrap();
        drop(lock);
        assert!(backup_to(&config(one.path()), &out, &passphrase).await.is_err(), "never overwrites");

        // Not while the server runs, nor with the wrong passphrase.
        let lock = server::lock_instance(&config(two.path())).unwrap();
        let refused = restore_from(&config(two.path()), &out, &passphrase).await.unwrap_err();
        assert!(format!("{refused:#}").contains("stop it first"), "{refused:#}");
        drop(lock);
        let refused = restore_from(&config(two.path()), &out, &Secret::new("not the passphrase")).await.unwrap_err();
        assert!(format!("{refused:#}").contains("passphrase is wrong"), "{refused:#}");
        assert_eq!(admin_email(two.path()).await, "bob@example.com", "nothing changed");

        let restored = restore_from(&config(two.path()), &out, &passphrase).await.unwrap();
        assert_eq!(restored.manifest, manifest);
        assert_eq!(admin_email(two.path()).await, "ada@example.com");
        let key = secrets::existing_master_key(&config(two.path())).unwrap();
        assert_eq!(key.id(), manifest.master_key_id);
        // The previous database and key are kept.
        let previous = |path: &&PathBuf| {
            path.file_name().is_some_and(|name| name.to_string_lossy().starts_with("spinikus.db.before-restore-"))
        };
        assert!(
            restored.previous_database.iter().find(previous).is_some_and(|path| path.is_file()),
            "{:?}",
            restored.previous_database
        );
        assert!(restored.previous_key.as_ref().is_some_and(|path| path.is_file()), "{:?}", restored.previous_key);
        assert!(
            !fs::read_dir(two.path()).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".spinikus-restore-")),
            "the work directory is gone"
        );

        // Restoring the same backup again keeps the key, which is already the backup's.
        let again = restore_from(&config(two.path()), &out, &passphrase).await.unwrap();
        assert!(again.previous_key.is_none());
    }

    #[tokio::test]
    async fn restores_into_an_empty_data_directory() {
        let (one, two) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let passphrase = Secret::new("a long enough passphrase");
        instance(one.path(), "ada@example.com").await;
        let out = one.path().join("backup.tar.gz.age");
        backup_to(&config(one.path()), &out, &passphrase).await.unwrap();
        let fresh = two.path().join("data");

        let restored = restore_from(&config(&fresh), &out, &passphrase).await.unwrap();
        assert!(restored.previous_database.is_empty() && restored.previous_key.is_none());
        assert_eq!(admin_email(&fresh).await, "ada@example.com");
        assert!(!server::open_control(&config(&fresh)).await.unwrap().needs_setup().await.unwrap());
    }

    #[tokio::test]
    async fn a_configured_master_key_must_be_the_backups() {
        let (one, two) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let passphrase = Secret::new("a long enough passphrase");
        instance(one.path(), "ada@example.com").await;
        let out = one.path().join("backup.tar.gz.age");
        backup_to(&config(one.path()), &out, &passphrase).await.unwrap();
        let mut other = config(two.path());
        other.secrets.master_key = Some(MasterKey::generate().to_base64().to_string());

        let refused = restore_from(&other, &out, &passphrase).await.unwrap_err();
        assert!(format!("{refused:#}").contains("remove that setting"), "{refused:#}");
        assert!(!two.path().join("spinikus.db").exists());
    }

    #[tokio::test]
    async fn backing_up_needs_a_database_and_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let passphrase = Secret::new("a long enough passphrase");
        let refused = backup_to(&config(dir.path()), &dir.path().join("out"), &passphrase).await.unwrap_err();
        assert!(format!("{refused:#}").contains("nothing to back up"), "{refused:#}");

        instance(dir.path(), "ada@example.com").await;
        fs::remove_file(config(dir.path()).master_key_path()).unwrap();
        assert!(backup_to(&config(dir.path()), &dir.path().join("out"), &passphrase).await.is_err());
        assert!(!config(dir.path()).master_key_path().exists(), "no key is generated");
    }
}
