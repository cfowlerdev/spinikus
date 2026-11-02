use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
};

use config::{Config, Environment, File, FileFormat};
use spinikus_core::config::{LoggingConfig, SpinikusConfig};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

pub fn init_logging(logging: &LoggingConfig) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&logging.level));

    let registry = tracing_subscriber::registry().with(filter);

    if logging.json {
        registry.with(fmt::layer().json()).init();
    } else {
        // Colors only on a terminal, not in the journal.
        registry.with(fmt::layer().with_ansi(std::io::stdout().is_terminal())).init();
    }
}

/// The config file in the current directory, for development.
const LOCAL_FILE: &str = "spinikus.toml";

/// The config file of an installed instance.
pub const SYSTEM_FILE: &str = "/etc/spinikus/spinikus.toml";

/// Which config file to read: `path` if given, else [`LOCAL_FILE`] if it exists, else
/// [`SYSTEM_FILE`].
pub fn path(path: Option<&Path>) -> PathBuf {
    match path {
        Some(path) => path.to_path_buf(),
        None if Path::new(LOCAL_FILE).exists() => PathBuf::from(LOCAL_FILE),
        None => PathBuf::from(SYSTEM_FILE),
    }
}

/// Reads the config file, overlaid by `SPINIKUS_…` environment variables.
pub fn load(path: &Path) -> anyhow::Result<SpinikusConfig> {
    if !path.exists() {
        anyhow::bail!(
            "{} does not exist; pass --config or SPINIKUS_CONFIG_FILE, or run from a directory with {LOCAL_FILE}",
            path.display()
        );
    }
    let config = Config::builder()
        .add_source(File::from(path).format(FileFormat::Toml))
        .add_source(Environment::with_prefix("SPINIKUS").prefix_separator("_").separator("__"))
        .build()?
        .try_deserialize()?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    #[test]
    fn reads_the_named_file_and_says_when_it_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("instance.toml");
        assert_eq!(super::path(Some(&file)), file);
        let missing = super::load(&file).err().expect("an error").to_string();
        assert!(missing.contains("instance.toml does not exist") && missing.contains("SPINIKUS_CONFIG_FILE"), "{missing}");

        std::fs::write(&file, "data_dir = \"/var/lib/spinikus\"\n[http]\nlisten = \"0.0.0.0:8080\"\n").unwrap();
        let config = super::load(&file).unwrap();
        assert_eq!(config.data_dir, PathBuf::from("/var/lib/spinikus"));
        assert_eq!(config.http.listen.to_string(), "0.0.0.0:8080");
        // Tests run in the crate's directory, which has no spinikus.toml.
        assert_eq!(super::path(None), Path::new(super::SYSTEM_FILE));
    }
}
