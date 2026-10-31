//! A service's runtime settings: the `runtime` object of its config, in the same shape as
//! `runtime` in `spinikus.json`, so the file can later set the same fields.
//!
//! ```json
//! { "runtime": { "port": 3000, "healthcheck": { "path": "/healthz", "interval": "10s" }, "shutdown_timeout": "30s" } }
//! ```
//!
//! Unknown fields are rejected, so a setting Spinikus cannot apply yet (such as `release`)
//! fails loudly instead of being ignored.

use std::time::Duration;

use serde::{Deserialize, Deserializer};

use crate::service::ServiceConfig;

/// The longest duration any setting accepts.
pub const MAX_SETTING_DURATION: Duration = Duration::from_secs(60 * 60);

/// How long a container gets to stop when `shutdown_timeout` is unset.
pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Health check defaults, for fields the settings leave unset.
pub const DEFAULT_HEALTHCHECK_INTERVAL: Duration = Duration::from_secs(2);
pub const DEFAULT_HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_HEALTHCHECK_RETRIES: u32 = 3;
/// Failures in this time do not count, and success ends it early, so it can be generous.
pub const DEFAULT_HEALTHCHECK_START_PERIOD: Duration = Duration::from_secs(60);
/// How long a container without a health check path has to keep running.
pub const DEFAULT_SETTLE_PERIOD: Duration = Duration::from_secs(5);

/// The health check to run, with defaults filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthCheck {
    pub path: Option<String>,
    pub interval: Duration,
    pub timeout: Duration,
    pub retries: u32,
    pub start_period: Duration,
}

impl HealthCheck {
    /// The longest a failing check can take: the start period, then every retry timing out.
    pub fn longest_run(&self) -> Duration {
        self.start_period + (self.interval + self.timeout) * (self.retries + 1)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSettings {
    /// The port the app listens on inside its container. Without one, the app receives no
    /// traffic and its health check cannot have a path.
    pub port: Option<u16>,
    pub healthcheck: Option<HealthCheckSettings>,
    /// How long a replaced container gets to finish in-flight requests before it is killed.
    #[serde(default, deserialize_with = "duration")]
    pub shutdown_timeout: Option<Duration>,
}

/// Unset fields use the agent's defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthCheckSettings {
    /// HTTP path to probe; without one, the container only has to keep running.
    pub path: Option<String>,
    #[serde(default, deserialize_with = "duration")]
    pub interval: Option<Duration>,
    #[serde(default, deserialize_with = "duration")]
    pub timeout: Option<Duration>,
    pub retries: Option<u32>,
    #[serde(default, deserialize_with = "duration")]
    pub start_period: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct RuntimeSettingsError(String);

impl RuntimeSettings {
    /// The settings in `config`, defaults if it has no `runtime` object.
    pub fn from_config(config: &ServiceConfig) -> Result<Self, RuntimeSettingsError> {
        let Some(runtime) = config.get("runtime") else {
            return Ok(Self::default());
        };
        let settings: Self = serde_json::from_value(runtime.clone()).map_err(|error| RuntimeSettingsError(error.to_string()))?;
        settings.check()?;
        Ok(settings)
    }

    fn check(&self) -> Result<(), RuntimeSettingsError> {
        if self.port == Some(0) {
            return Err(RuntimeSettingsError("port must be between 1 and 65535".to_string()));
        }
        let Some(check) = &self.healthcheck else {
            return Ok(());
        };
        if let Some(path) = &check.path {
            if !path.starts_with('/') || !path.bytes().all(|byte| byte.is_ascii_graphic()) {
                return Err(RuntimeSettingsError(format!(
                    "healthcheck path `{}` must start with `/` and contain no spaces or control characters",
                    path.escape_debug()
                )));
            }
            if self.port.is_none() {
                return Err(RuntimeSettingsError("a healthcheck path needs a port".to_string()));
            }
        }
        Ok(())
    }

    /// The health check, with defaults for what the settings leave unset.
    pub fn health_check(&self) -> HealthCheck {
        let check = self.healthcheck.clone().unwrap_or_default();
        let default_start = if check.path.is_some() { DEFAULT_HEALTHCHECK_START_PERIOD } else { DEFAULT_SETTLE_PERIOD };
        HealthCheck {
            interval: check.interval.filter(|interval| !interval.is_zero()).unwrap_or(DEFAULT_HEALTHCHECK_INTERVAL),
            timeout: check.timeout.filter(|timeout| !timeout.is_zero()).unwrap_or(DEFAULT_HEALTHCHECK_TIMEOUT),
            retries: check.retries.filter(|retries| *retries > 0).unwrap_or(DEFAULT_HEALTHCHECK_RETRIES),
            start_period: check.start_period.unwrap_or(default_start),
            path: check.path,
        }
    }

    pub fn shutdown_timeout(&self) -> Duration {
        self.shutdown_timeout.unwrap_or(DEFAULT_SHUTDOWN_TIMEOUT)
    }
}

/// A duration written like `10s`, `1m30s` or `2m`, at most [`MAX_SETTING_DURATION`].
fn duration<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Duration>, D::Error> {
    let Some(text) = Option::<String>::deserialize(deserializer)? else {
        return Ok(None);
    };
    let invalid = || serde::de::Error::custom(format!("invalid duration `{text}`: use a value like `10s` or `1m30s`"));
    let duration: jiff::SignedDuration = text.parse().map_err(|_| invalid())?;
    let duration = Duration::try_from(duration).map_err(|_| invalid())?;
    if duration > MAX_SETTING_DURATION {
        return Err(serde::de::Error::custom(format!("duration `{text}` is longer than an hour")));
    }
    Ok(Some(duration))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn config(runtime: serde_json::Value) -> ServiceConfig {
        json!({ "runtime": runtime }).as_object().unwrap().clone()
    }

    #[test]
    fn reads_the_runtime_object() {
        let settings = RuntimeSettings::from_config(&config(json!({
            "port": 3000,
            "healthcheck": { "path": "/healthz", "interval": "10s", "start_period": "1m30s", "retries": 5 },
            "shutdown_timeout": "30s",
        })))
        .unwrap();

        assert_eq!(settings.port, Some(3000));
        let check = settings.healthcheck.clone().unwrap();
        assert_eq!(check.path.as_deref(), Some("/healthz"));
        assert_eq!(check.interval, Some(Duration::from_secs(10)));
        assert_eq!(check.start_period, Some(Duration::from_secs(90)));
        assert_eq!((check.retries, check.timeout), (Some(5), None));
        assert_eq!(settings.shutdown_timeout, Some(Duration::from_secs(30)));
        let resolved = settings.health_check();
        assert_eq!(
            (resolved.interval, resolved.timeout, resolved.retries),
            (Duration::from_secs(10), DEFAULT_HEALTHCHECK_TIMEOUT, 5)
        );
        assert_eq!(resolved.start_period, Duration::from_secs(90));
    }

    #[test]
    fn defaults_without_a_runtime_object() {
        let settings = RuntimeSettings::from_config(&ServiceConfig::new()).unwrap();

        assert_eq!(settings, RuntimeSettings::default());
        assert_eq!(settings.shutdown_timeout(), DEFAULT_SHUTDOWN_TIMEOUT);
        let check = settings.health_check();
        assert_eq!((check.path.as_deref(), check.start_period), (None, DEFAULT_SETTLE_PERIOD));
        assert_eq!(check.longest_run(), DEFAULT_SETTLE_PERIOD + (DEFAULT_HEALTHCHECK_INTERVAL + DEFAULT_HEALTHCHECK_TIMEOUT) * 4);
    }

    #[test]
    fn rejects_what_it_cannot_apply() {
        let problem = |runtime| RuntimeSettings::from_config(&config(runtime)).unwrap_err().to_string();

        assert!(problem(json!({ "release": "npm run migrate" })).contains("unknown field `release`"));
        assert!(problem(json!({ "port": 70000 })).contains("invalid value"));
        assert!(problem(json!({ "port": 0 })).contains("between 1 and 65535"));
        assert!(problem(json!({ "port": 80, "shutdown_timeout": "soon" })).contains("invalid duration `soon`"));
        assert!(problem(json!({ "port": 80, "shutdown_timeout": "2h" })).contains("longer than an hour"));
        assert!(problem(json!({ "port": 80, "healthcheck": { "path": "up" } })).contains("must start with `/`"));
        assert!(problem(json!({ "port": 80, "healthcheck": { "path": "/a b" } })).contains("must start with `/`"));
        assert!(problem(json!({ "healthcheck": { "path": "/up" } })).contains("needs a port"));
        assert!(problem(json!("3000")).contains("invalid type"));
    }
}
