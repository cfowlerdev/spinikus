//! One-click databases: what a database service of each engine is made of. A database is an
//! ordinary service of kind `database`, with the engine's official image as its source, a
//! volume for its data, a runtime config, and generated variables that apps reference
//! (`${{ postgres.DATABASE_URL }}`). Everything here is data; the control plane creates it.
//!
//! The image reads its credentials only when it starts on an empty volume, so changing the
//! variables later does not change the database's password.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{service::ServiceConfig, slug::Slug, text::impl_text, volume::MountPath};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    Postgres,
    Redis,
}

impl Engine {
    pub const ALL: [Engine; 2] = [Self::Postgres, Self::Redis];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Redis => "redis",
        }
    }

    /// The major versions that can be created, oldest first.
    pub fn versions(self) -> &'static [&'static str] {
        match self {
            Self::Postgres => &["15", "16", "17", "18"],
            Self::Redis => &["7", "8"],
        }
    }

    /// The newest version.
    pub fn default_version(self) -> &'static str {
        self.versions().last().expect("every engine has a version")
    }

    pub fn port(self) -> u16 {
        match self {
            Self::Postgres => 5432,
            Self::Redis => 6379,
        }
    }
}

impl_text!(Engine, "database engine");

/// A database to create: an engine at one of its versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseSpec {
    engine: Engine,
    version: &'static str,
}

/// A variable a database service starts with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseVariable {
    pub name: &'static str,
    pub value: String,
    pub secret: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{engine} has no version `{version}`; use one of {}", engine.versions().join(", "))]
pub struct VersionError {
    engine: Engine,
    version: String,
}

impl DatabaseSpec {
    /// `engine` at `version`, its newest if `None`.
    pub fn new(engine: Engine, version: Option<&str>) -> Result<Self, VersionError> {
        let version = match version.map(str::trim) {
            None => engine.default_version(),
            Some(version) => engine
                .versions()
                .iter()
                .find(|known| **known == version)
                .ok_or_else(|| VersionError { engine, version: version.to_string() })?,
        };
        Ok(Self { engine, version })
    }

    pub fn engine(&self) -> Engine {
        self.engine
    }

    pub fn version(&self) -> &'static str {
        self.version
    }

    /// The official image, by major version.
    pub fn image(&self) -> String {
        format!("{}:{}", self.engine, self.version)
    }

    /// Where the data volume is mounted: where the image keeps its data. Postgres 18 moved it
    /// one level up, and an image's own anonymous volume there would otherwise hide ours.
    pub fn data_path(&self) -> MountPath {
        let path = match (self.engine, self.version) {
            (Engine::Postgres, "15" | "16" | "17") => "/var/lib/postgresql/data",
            (Engine::Postgres, _) => "/var/lib/postgresql",
            (Engine::Redis, _) => "/data",
        };
        path.parse().expect("a valid mount path")
    }

    /// The service config: the engine and version, and the runtime settings. Postgres gets
    /// time to shut down cleanly; Redis is started with its password and an append-only log,
    /// so writes survive a restart.
    pub fn config(&self) -> ServiceConfig {
        let mut runtime = json!({ "port": self.engine.port(), "shutdown_timeout": "30s" });
        if self.engine == Engine::Redis {
            runtime["start"] = Value::from(r#"exec redis-server --requirepass "$REDIS_PASSWORD" --appendonly yes"#);
        }
        let config = json!({ "database": { "engine": self.engine, "version": self.version }, "runtime": runtime });
        config.as_object().cloned().expect("an object")
    }

    /// The variables the service starts with, for a service reachable as `host` (its slug)
    /// with `password`. The URLs repeat the password, so they are secrets too.
    pub fn variables(&self, host: &Slug, password: &str) -> Vec<DatabaseVariable> {
        let plain = |name, value: &str| DatabaseVariable { name, value: value.to_string(), secret: false };
        let secret = |name, value: String| DatabaseVariable { name, value, secret: true };
        let port = self.engine.port();
        match self.engine {
            Engine::Postgres => vec![
                plain("POSTGRES_USER", "app"),
                secret("POSTGRES_PASSWORD", password.to_string()),
                plain("POSTGRES_DB", "app"),
                secret("DATABASE_URL", format!("postgres://app:{password}@{host}:{port}/app")),
            ],
            Engine::Redis => vec![
                secret("REDIS_PASSWORD", password.to_string()),
                secret("REDIS_URL", format!("redis://default:{password}@{host}:{port}")),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_a_known_version_or_the_newest() {
        let newest = DatabaseSpec::new(Engine::Postgres, None).unwrap();
        let old = DatabaseSpec::new(Engine::Postgres, Some("16")).unwrap();
        let unknown = DatabaseSpec::new(Engine::Redis, Some("6")).unwrap_err();

        assert_eq!((newest.image(), newest.data_path().as_str()), ("postgres:18".to_string(), "/var/lib/postgresql"));
        assert_eq!((old.image(), old.data_path().as_str()), ("postgres:16".to_string(), "/var/lib/postgresql/data"));
        assert_eq!(unknown.to_string(), "redis has no version `6`; use one of 7, 8");
    }

    #[test]
    fn configures_redis_with_its_password() {
        let redis = DatabaseSpec::new(Engine::Redis, None).unwrap();
        let config = redis.config();
        let settings = crate::runtime::RuntimeSettings::from_config(&config).unwrap();
        let variables = redis.variables(&"cache".parse().unwrap(), "pw");

        assert_eq!(config["database"], json!({ "engine": "redis", "version": "8" }));
        assert_eq!(settings.port, Some(6379));
        assert!(settings.start.unwrap().contains(r#"--requirepass "$REDIS_PASSWORD""#));
        assert_eq!(redis.data_path().as_str(), "/data");
        assert_eq!(variables[1].value, "redis://default:pw@cache:6379");
        assert!(variables.iter().all(|variable| variable.secret));
    }

    #[test]
    fn gives_postgres_a_url_for_apps() {
        let postgres = DatabaseSpec::new(Engine::Postgres, Some("17")).unwrap();
        let variables = postgres.variables(&"db".parse().unwrap(), "pw");
        let names: Vec<_> = variables.iter().map(|variable| (variable.name, variable.secret)).collect();

        assert_eq!(
            names,
            [("POSTGRES_USER", false), ("POSTGRES_PASSWORD", true), ("POSTGRES_DB", false), ("DATABASE_URL", true)]
        );
        assert_eq!(variables[3].value, "postgres://app:pw@db:5432/app");
        assert!(crate::runtime::RuntimeSettings::from_config(&postgres.config()).unwrap().start.is_none());
    }

    #[test]
    fn text_round_trips() {
        for engine in Engine::ALL {
            assert_eq!(engine.to_string().parse(), Ok(engine));
        }
    }
}
