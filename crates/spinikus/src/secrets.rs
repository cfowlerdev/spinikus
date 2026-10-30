use std::{fs, io, path::Path};

use anyhow::Context;
use spinikus_core::config::SpinikusConfig;
use spinikus_store::MasterKey;

/// Loads the master key from `secrets.master_key`, or else from the key file.
///
/// The default key file (`master.key` in the data directory) is generated if it does not
/// exist yet. A key file configured explicitly is never generated: a missing one is an error,
/// since a fresh key could not decrypt anything already in the database.
pub fn load_master_key(config: &SpinikusConfig) -> anyhow::Result<MasterKey> {
    if let Some(encoded) = &config.secrets.master_key {
        return MasterKey::from_base64(encoded.trim()).context("Invalid secrets.master_key");
    }

    let path = config.master_key_path();
    match fs::read_to_string(&path) {
        Ok(encoded) => {
            warn_if_readable_by_others(&path);
            MasterKey::from_base64(encoded.trim()).with_context(|| format!("Invalid master key in {}", path.display()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound && config.secrets.master_key_file.is_none() => {
            generate_key_file(&path)
        }
        Err(error) => Err(error).with_context(|| format!("Failed to read master key from {}", path.display()))
    }
}

fn generate_key_file(path: &Path) -> anyhow::Result<MasterKey> {
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    }

    let key = MasterKey::generate();
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).with_context(|| format!("Failed to create master key file {}", path.display()))?;
    io::Write::write_all(&mut file, format!("{}\n", key.to_base64().as_str()).as_bytes())
        .with_context(|| format!("Failed to write master key file {}", path.display()))?;

    tracing::warn!(
        path = %path.display(),
        master_key = key.id(),
        "Generated a new master key. Back it up separately from the database: secrets in a backup cannot be decrypted without it"
    );
    Ok(key)
}

#[cfg(unix)]
fn warn_if_readable_by_others(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    if let Ok(metadata) = fs::metadata(path)
        && metadata.permissions().mode() & 0o077 != 0
    {
        tracing::warn!(path = %path.display(), "The master key file is accessible to other users; restrict it with chmod 600");
    }
}

#[cfg(not(unix))]
fn warn_if_readable_by_others(_path: &Path) {}

#[cfg(test)]
mod tests {
    use spinikus_core::config::SecretsConfig;

    use super::*;

    fn config(dir: &Path, secrets: SecretsConfig) -> SpinikusConfig {
        SpinikusConfig { data_dir: dir.to_path_buf(), secrets, ..Default::default() }
    }

    #[test]
    fn generates_the_default_key_file_once() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(&dir.path().join("data"), SecretsConfig::default());

        let generated = load_master_key(&config).unwrap();
        let reloaded = load_master_key(&config).unwrap();

        assert_eq!(reloaded.id(), generated.id());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(config.master_key_path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_configured_key_file_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = SecretsConfig { master_key: None, master_key_file: Some("missing.key".into()) };

        let result = load_master_key(&config(dir.path(), secrets));

        assert!(result.is_err());
        assert!(!dir.path().join("missing.key").exists());
    }

    #[test]
    fn the_key_setting_takes_precedence_over_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_key = MasterKey::generate();
        fs::write(dir.path().join("master.key"), file_key.to_base64().as_str()).unwrap();
        let setting_key = MasterKey::generate();
        let secrets = SecretsConfig { master_key: Some(setting_key.to_base64().to_string()), master_key_file: None };

        let loaded = load_master_key(&config(dir.path(), secrets)).unwrap();

        assert_eq!(loaded.id(), setting_key.id());
    }

    #[test]
    fn rejects_a_malformed_key_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("master.key"), "too short").unwrap();

        assert!(load_master_key(&config(dir.path(), SecretsConfig::default())).is_err());
    }
}
