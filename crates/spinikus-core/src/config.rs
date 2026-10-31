use std::{
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
};

use serde::Deserialize;

/// Logging settings from the `[logging]` table of `spinikus.toml`.
#[derive(Deserialize, Default)]
pub struct LoggingConfig {
    pub level: String,
    pub json: bool,
}

/// Database settings from the `[database]` table of `spinikus.toml`.
#[derive(Deserialize, Default)]
pub struct DatabaseConfig {
    /// SQLite database file. Defaults to `spinikus.db` inside the data directory;
    /// a relative path is resolved against the data directory.
    pub path: Option<PathBuf>,
}

/// Where the master encryption key comes from, from the `[secrets]` table of `spinikus.toml`.
/// The key never goes in the database, so encrypted secrets stay usable after a restore.
#[derive(Deserialize, Default)]
pub struct SecretsConfig {
    /// The base64 key itself, meant for `SPINIKUS_SECRETS__MASTER_KEY`. Takes precedence over the key file.
    pub master_key: Option<String>,
    /// File holding the base64 key. Defaults to `master.key` inside the data directory, which is
    /// generated on first start; a relative path is resolved against the data directory.
    pub master_key_file: Option<PathBuf>,
}

/// HTTP settings from the `[http]` table of `spinikus.toml`: where the API and web UI listen.
#[derive(Deserialize)]
pub struct HttpConfig {
    /// Defaults to `127.0.0.1:8080`, reachable from this machine only.
    #[serde(default = "default_http_listen")]
    pub listen: SocketAddr,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self { listen: default_http_listen() }
    }
}

fn default_http_listen() -> SocketAddr {
    (Ipv4Addr::LOCALHOST, 8080).into()
}

/// Agent listener settings from the `[agents]` table of `spinikus.toml`.
#[derive(Deserialize)]
pub struct AgentsConfig {
    /// Where agents connect, over mutual TLS. Defaults to `0.0.0.0:8443`, so agents on
    /// other servers can reach it: every call but enrollment needs a client certificate
    /// from the internal CA, and enrollment needs a join token.
    #[serde(default = "default_agents_listen")]
    pub listen: SocketAddr,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self { listen: default_agents_listen() }
    }
}

fn default_agents_listen() -> SocketAddr {
    (Ipv4Addr::UNSPECIFIED, 8443).into()
}

/// Instance configuration, the schema of `spinikus.toml`.
#[derive(Deserialize)]
pub struct SpinikusConfig {
    /// Directory for everything Spinikus stores on disk.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    pub logging: LoggingConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub secrets: SecretsConfig,
    #[serde(default)]
    pub http: HttpConfig,
    #[serde(default)]
    pub agents: AgentsConfig,
}

impl SpinikusConfig {
    /// The SQLite database file, resolved against the data directory.
    pub fn database_path(&self) -> PathBuf {
        match &self.database.path {
            Some(path) => self.data_dir.join(path),
            None => self.data_dir.join("spinikus.db"),
        }
    }

    /// The master key file, resolved against the data directory.
    pub fn master_key_path(&self) -> PathBuf {
        match &self.secrets.master_key_file {
            Some(path) => self.data_dir.join(path),
            None => self.data_dir.join("master.key"),
        }
    }
}

impl Default for SpinikusConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            logging: LoggingConfig::default(),
            database: DatabaseConfig::default(),
            secrets: SecretsConfig::default(),
            http: HttpConfig::default(),
            agents: AgentsConfig::default(),
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

    #[test]
    fn master_key_path_defaults_to_data_dir() {
        let default = SpinikusConfig { data_dir: "/var/lib/spinikus".into(), ..Default::default() };
        let configured = SpinikusConfig {
            data_dir: "/var/lib/spinikus".into(),
            secrets: SecretsConfig { master_key: None, master_key_file: Some("/etc/spinikus/master.key".into()) },
            ..Default::default()
        };

        assert_eq!(default.master_key_path(), PathBuf::from("/var/lib/spinikus/master.key"));
        assert_eq!(configured.master_key_path(), PathBuf::from("/etc/spinikus/master.key"));
    }
}
