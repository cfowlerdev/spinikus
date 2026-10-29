use std::path::PathBuf;

use serde::Deserialize;

/// Logging settings from the `[logging]` table of `spinikus.toml`.
#[derive(Deserialize, Default)]
pub struct LoggingConfig {
    pub level: String,
    pub json: bool
}

/// Database settings from the `[database]` table of `spinikus.toml`.
#[derive(Deserialize, Default)]
pub struct DatabaseConfig {
    /// SQLite database file. Defaults to `spinikus.db` inside the data directory;
    /// a relative path is resolved against the data directory.
    pub path: Option<PathBuf>
}

/// Instance configuration, the schema of `spinikus.toml`.
#[derive(Deserialize)]
pub struct SpinikusConfig {
    /// Directory for everything Spinikus stores on disk.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    pub logging: LoggingConfig,
    #[serde(default)]
    pub database: DatabaseConfig
}

impl SpinikusConfig {
    /// The SQLite database file, resolved against the data directory.
    pub fn database_path(&self) -> PathBuf {
        match &self.database.path {
            Some(path) => self.data_dir.join(path),
            None => self.data_dir.join("spinikus.db")
        }
    }
}

impl Default for SpinikusConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            logging: LoggingConfig::default(),
            database: DatabaseConfig::default()
        }
    }
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("data")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_path_defaults_to_data_dir() {
        let config = SpinikusConfig { data_dir: "/var/lib/spinikus".into(), ..Default::default() };
        assert_eq!(config.database_path(), PathBuf::from("/var/lib/spinikus/spinikus.db"));
    }

    #[test]
    fn relative_database_path_is_resolved_against_data_dir() {
        let config = SpinikusConfig {
            data_dir: "/var/lib/spinikus".into(),
            database: DatabaseConfig { path: Some("db/main.db".into()) },
            ..Default::default()
        };
        assert_eq!(config.database_path(), PathBuf::from("/var/lib/spinikus/db/main.db"));
    }

    #[test]
    fn absolute_database_path_is_kept() {
        let config = SpinikusConfig {
            data_dir: "/var/lib/spinikus".into(),
            database: DatabaseConfig { path: Some("/srv/spinikus.db".into()) },
            ..Default::default()
        };
        assert_eq!(config.database_path(), PathBuf::from("/srv/spinikus.db"));
    }
}
