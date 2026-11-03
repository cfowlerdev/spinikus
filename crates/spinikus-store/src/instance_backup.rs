//! Backups of a whole Spinikus instance: the database and the master key in one file,
//! encrypted with a passphrase.
//!
//! The file is an [age](https://age-encryption.org) file, encrypted to a passphrase (scrypt),
//! holding a gzipped tar of `manifest.json`, `spinikus.db` and `master.key`, so it can also be
//! opened by hand: `age -d backup.tar.gz.age | tar xz`. The database is a consistent snapshot
//! made with `VACUUM INTO`, so backing up never stops a running server.

use std::{
    fs,
    io::{self, Read, Write},
    iter,
    path::{Path, PathBuf},
};

use age::secrecy::SecretString;
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use spinikus_core::secret::Secret;
use sqlx::SqlitePool;
use zeroize::Zeroizing;

use crate::{StoreError, secrets::MasterKey};

/// The version of the file's layout. Readers refuse layouts newer than theirs.
pub const BACKUP_FORMAT: u32 = 1;

const MANIFEST: &str = "manifest.json";
const DATABASE: &str = "spinikus.db";
const KEY: &str = "master.key";

/// The most work a passphrase may take to try, as scrypt's log2 N: 2^22 takes a few seconds,
/// well above what encrypting picks (about one second's worth), so a backup made on a fast
/// machine still opens on a slow one.
const MAX_WORK_FACTOR: u8 = 22;

/// What a backup says about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupManifest {
    pub format: u32,
    /// The version of Spinikus that made it.
    pub spinikus_version: String,
    pub created_at: Timestamp,
    /// The newest migration applied to the database.
    pub schema_version: i64,
    /// The master key's ID ([`MasterKey::id`]), which identifies it without revealing it.
    pub master_key_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("the passphrase is wrong, or the file was not encrypted with a passphrase")]
    WrongPassphrase,
    #[error("this is not a Spinikus backup: {0}")]
    NotABackup(String),
    #[error("the backup's format ({0}) is newer than this version of Spinikus reads; upgrade Spinikus first")]
    NewerFormat(u32),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<sqlx::Error> for BackupError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(error.into())
    }
}

/// A backup opened into a directory: its manifest, where its database now is, and its master key.
pub struct OpenedBackup {
    pub manifest: BackupManifest,
    pub database: PathBuf,
    pub master_key: MasterKey,
}

/// The newest migration this build knows, which a database must not be past.
pub fn known_schema_version() -> i64 {
    sqlx::migrate!().migrations.iter().map(|migration| migration.version).max().unwrap_or(0)
}

/// The newest migration applied to the database, 0 for none.
pub async fn schema_version(pool: &SqlitePool) -> Result<i64, StoreError> {
    let version: Option<i64> =
        sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success = 1").fetch_one(pool).await?;
    Ok(version.unwrap_or(0))
}

/// Writes a backup of the database behind `pool`, with `master_key`, to `out`, which must not
/// exist yet. It is readable only by its owner. The work happens next to `out` (the database's
/// snapshot and the file being written, `<out>.partial`), and `out` appears only once complete.
///
/// `master_key` must be the database's: check that first (opening a [`crate::SqliteStore`]
/// with it does), since a backup with another key could never be read.
pub async fn create_backup(
    pool: &SqlitePool,
    master_key: &MasterKey,
    spinikus_version: &str,
    passphrase: &Secret,
    out: &Path,
) -> Result<BackupManifest, BackupError> {
    let dir = match out.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let work = tempfile::Builder::new().prefix(".spinikus-backup-").tempdir_in(&dir)?;
    let snapshot = work.path().join(DATABASE);
    sqlx::query("VACUUM INTO ?").bind(snapshot.to_string_lossy().into_owned()).execute(pool).await?;

    let manifest = BackupManifest {
        format: BACKUP_FORMAT,
        spinikus_version: spinikus_version.to_string(),
        created_at: Timestamp::now(),
        schema_version: schema_version(pool).await?,
        master_key_id: master_key.id().to_string(),
    };
    let key = master_key.to_base64();
    let (out, passphrase, written) = (out.to_path_buf(), passphrase.clone(), manifest.clone());
    tokio::task::spawn_blocking(move || write_archive(&written, &snapshot, &key, &passphrase, &out))
        .await
        .map_err(io::Error::other)??;
    drop(work);
    Ok(manifest)
}

fn write_archive(manifest: &BackupManifest, snapshot: &Path, key: &str, passphrase: &Secret, out: &Path) -> io::Result<()> {
    let partial = out.with_file_name(format!(
        "{}.partial",
        out.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
    ));
    let result = (|| {
        let file = private_file(&partial)?;
        let encryptor = age::Encryptor::with_user_passphrase(SecretString::from(passphrase.expose().to_string()));
        let encrypted = encryptor.wrap_output(io::BufWriter::new(file))?;
        let mut archive = tar::Builder::new(GzEncoder::new(encrypted, Compression::default()));
        archive.mode(tar::HeaderMode::Deterministic);
        let manifest = serde_json::to_vec_pretty(manifest).map_err(io::Error::other)?;
        append(&mut archive, MANIFEST, &mut manifest.as_slice(), manifest.len() as u64)?;
        let mut database = fs::File::open(snapshot)?;
        let size = database.metadata()?.len();
        append(&mut archive, DATABASE, &mut database, size)?;
        let key = Zeroizing::new(format!("{key}\n"));
        append(&mut archive, KEY, &mut key.as_bytes(), key.len() as u64)?;
        let encrypted = archive.into_inner()?.finish()?;
        let mut file = encrypted.finish()?.into_inner().map_err(io::IntoInnerError::into_error)?;
        file.flush()?;
        file.sync_all()?;
        // Never replaces a file that appeared meanwhile.
        fs::hard_link(&partial, out)?;
        fs::remove_file(&partial)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

fn append<W: Write>(archive: &mut tar::Builder<W>, name: &str, data: &mut impl Read, size: u64) -> io::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(size);
    header.set_mode(0o600);
    archive.append_data(&mut header, name, data)
}

/// Creates a new file only its owner can read.
fn private_file(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

/// Decrypts the backup in `file` and writes its database into the directory `into`. Blocks:
/// decrypting with a passphrase takes a second or so on purpose.
pub fn open_backup(file: &Path, passphrase: &Secret, into: &Path) -> Result<OpenedBackup, BackupError> {
    let input = io::BufReader::new(fs::File::open(file)?);
    let decryptor = age::Decryptor::new_buffered(input).map_err(|error| match error {
        age::DecryptError::Io(error) => BackupError::Io(error),
        error => BackupError::NotABackup(error.to_string()),
    })?;
    if !decryptor.is_scrypt() {
        return Err(BackupError::WrongPassphrase);
    }
    let mut identity = age::scrypt::Identity::new(SecretString::from(passphrase.expose().to_string()));
    identity.set_max_work_factor(MAX_WORK_FACTOR);
    let decrypted = decryptor.decrypt(iter::once(&identity as &dyn age::Identity)).map_err(|error| match error {
        age::DecryptError::DecryptionFailed | age::DecryptError::KeyDecryptionFailed | age::DecryptError::NoMatchingKeys => {
            BackupError::WrongPassphrase
        }
        age::DecryptError::Io(error) => BackupError::Io(error),
        error => BackupError::NotABackup(error.to_string()),
    })?;

    let mut archive = tar::Archive::new(GzDecoder::new(decrypted));
    let (mut manifest, mut key, mut database) = (None, None, None);
    let entries = archive.entries().map_err(not_an_archive)?;
    for entry in entries {
        let mut entry = entry.map_err(not_an_archive)?;
        let name = entry.path().map_err(not_an_archive)?.to_string_lossy().into_owned();
        match name.as_str() {
            MANIFEST => {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                let read: BackupManifest =
                    serde_json::from_slice(&bytes).map_err(|error| BackupError::NotABackup(format!("its manifest: {error}")))?;
                if read.format > BACKUP_FORMAT {
                    return Err(BackupError::NewerFormat(read.format));
                }
                manifest = Some(read);
            }
            DATABASE => {
                let path = into.join(DATABASE);
                io::copy(&mut entry, &mut private_file(&path)?)?;
                database = Some(path);
            }
            KEY => {
                let mut text = Zeroizing::new(String::new());
                entry.read_to_string(&mut text)?;
                key = Some(
                    MasterKey::from_base64(text.trim())
                        .map_err(|_| BackupError::NotABackup("its master key is invalid".into()))?,
                );
            }
            // Later formats may add files; this one needs only these.
            _ => {}
        }
    }
    let missing = |what: &str| BackupError::NotABackup(format!("it has no {what}"));
    let manifest = manifest.ok_or_else(|| missing(MANIFEST))?;
    let master_key = key.ok_or_else(|| missing(KEY))?;
    let database = database.ok_or_else(|| missing(DATABASE))?;
    if master_key.id() != manifest.master_key_id {
        return Err(BackupError::NotABackup("its master key is not the one its manifest names".into()));
    }
    Ok(OpenedBackup { manifest, database, master_key })
}

fn not_an_archive(error: io::Error) -> BackupError {
    BackupError::NotABackup(format!("its contents: {error}"))
}

/// Checks a database file before it replaces another: SQLite finds it intact, and
/// `master_key` unwraps its data keys. Applies no migrations.
pub async fn verify_database(path: &Path, master_key: &MasterKey) -> Result<(), BackupError> {
    if !path.is_file() {
        return Err(BackupError::NotABackup(format!("{} is missing", path.display())));
    }
    let pool = crate::connect(path).await?;
    let result = async {
        let check: String = sqlx::query_scalar("PRAGMA integrity_check").fetch_one(&pool).await?;
        if check != "ok" {
            return Err(BackupError::NotABackup(format!("its database is damaged: {check}")));
        }
        crate::secrets::Keyring::load(&pool, master_key).await?;
        Ok(())
    }
    .await;
    pool.close().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SqliteStore, connect, migrate};

    async fn database(dir: &Path) -> (SqlitePool, MasterKey) {
        let pool = connect(&dir.join("spinikus.db")).await.unwrap();
        migrate(&pool).await.unwrap();
        let key = MasterKey::generate();
        SqliteStore::open(pool.clone(), &key).await.unwrap();
        sqlx::query("INSERT INTO settings (key, value) VALUES ('base_domain', '\"example.com\"')").execute(&pool).await.unwrap();
        (pool, key)
    }

    #[tokio::test]
    async fn backs_up_and_opens_with_the_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let (pool, key) = database(dir.path()).await;
        let out = dir.path().join("backup.tar.gz.age");
        let passphrase = Secret::new("a long enough passphrase");

        let manifest = create_backup(&pool, &key, "1.2.3", &passphrase, &out).await.unwrap();
        assert_eq!((manifest.format, manifest.spinikus_version.as_str()), (BACKUP_FORMAT, "1.2.3"));
        assert_eq!(manifest.schema_version, known_schema_version());
        assert_eq!(manifest.master_key_id, key.id());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&out).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let names: Vec<_> = fs::read_dir(dir.path()).unwrap().map(|entry| entry.unwrap().file_name()).collect();
        assert!(!names.iter().any(|name| name.to_string_lossy().contains("partial") || name.to_string_lossy().starts_with('.')));
        // A backup never replaces a file.
        assert!(create_backup(&pool, &key, "1.2.3", &passphrase, &out).await.is_err());

        let into = tempfile::tempdir().unwrap();
        let opened = open_backup(&out, &passphrase, into.path()).unwrap();
        assert_eq!(opened.manifest, manifest);
        assert_eq!(opened.master_key.id(), key.id());
        verify_database(&opened.database, &opened.master_key).await.unwrap();
        let restored = connect(&opened.database).await.unwrap();
        let value: String =
            sqlx::query_scalar("SELECT value FROM settings WHERE key = 'base_domain'").fetch_one(&restored).await.unwrap();
        assert_eq!(value, "\"example.com\"");
        // Another master key can't open its secrets.
        assert!(verify_database(&opened.database, &MasterKey::generate()).await.is_err());
    }

    #[tokio::test]
    async fn refuses_a_wrong_passphrase_and_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let (pool, key) = database(dir.path()).await;
        let out = dir.path().join("backup.tar.gz.age");
        create_backup(&pool, &key, "1.2.3", &Secret::new("the right passphrase"), &out).await.unwrap();
        let into = tempfile::tempdir().unwrap();

        let wrong = open_backup(&out, &Secret::new("the wrong passphrase"), into.path());
        assert!(matches!(wrong, Err(BackupError::WrongPassphrase)), "{:?}", wrong.err());
        let other = dir.path().join("other");
        fs::write(&other, "not an age file").unwrap();
        let refused = open_backup(&other, &Secret::new("the right passphrase"), into.path());
        assert!(matches!(refused, Err(BackupError::NotABackup(_))), "{:?}", refused.err());
    }
}
