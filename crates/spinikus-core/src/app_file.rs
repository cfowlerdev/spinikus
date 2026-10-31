//! `spinikus.json`: the settings that change with the code, read from the source's root
//! directory at build time. It may have comments, and holds a `build` and a `runtime`
//! object in the same shape as those of the service config. For each field it sets, the
//! file wins over the service config ([`AppFile::apply`]).
//!
//! ```json
//! {
//!   "$schema": "https://spinikus.dev/schema/v1.json",
//!   // Settings that ship with the code.
//!   "build": { "dockerfile": "docker/Dockerfile" },
//!   "runtime": { "port": 3000, "healthcheck": { "path": "/healthz" } }
//! }
//! ```
//!
//! Fields Spinikus cannot apply yet (`env`, `cron`, `apps`, and `release` in `runtime`) are
//! rejected, so a deployment fails loudly instead of ignoring them.

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::{build::BuildSettings, runtime::RuntimeSettings, service::ServiceConfig};

/// The file's name, in the source's root directory.
pub const APP_FILE: &str = "spinikus.json";

/// The largest file accepted.
pub const MAX_APP_FILE_SIZE: usize = 64 * 1024;

/// The sections of the file, each merged into the service config's object of the same name.
const SECTIONS: [&str; 2] = ["build", "runtime"];

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppFile {
    #[serde(rename = "$schema")]
    pub schema: Option<String>,
    pub build: Option<Map<String, Value>>,
    pub runtime: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{APP_FILE}: {0}")]
pub struct AppFileError(String);

impl AppFile {
    /// Reads the file's text, comments allowed, and checks its sections.
    pub fn parse(text: &str) -> Result<Self, AppFileError> {
        if text.len() > MAX_APP_FILE_SIZE {
            return Err(AppFileError(format!("it is larger than {} KiB", MAX_APP_FILE_SIZE / 1024)));
        }
        let json = strip_comments(text).map_err(|problem| AppFileError(problem.to_string()))?;
        let file: Self = serde_json::from_str(&json).map_err(|error| AppFileError(error.to_string()))?;
        // Each section on its own must be valid, whatever the service config adds.
        let mut config = ServiceConfig::new();
        for (name, section) in file.sections() {
            config.insert(name.to_string(), Value::Object(section.clone()));
        }
        check(&config)?;
        Ok(file)
    }

    fn sections(&self) -> impl Iterator<Item = (&'static str, &Map<String, Value>)> {
        SECTIONS.into_iter().zip([&self.build, &self.runtime]).filter_map(|(name, section)| Some((name, section.as_ref()?)))
    }

    /// The service config with the file's fields in place of the config's, and the fields
    /// the file set, such as `runtime.port`. A field holding an object, such as
    /// `runtime.healthcheck`, is replaced whole.
    pub fn apply(&self, config: &ServiceConfig) -> Result<(ServiceConfig, Vec<String>), AppFileError> {
        let mut merged = config.clone();
        let mut from_file = Vec::new();
        for (name, section) in self.sections() {
            let target = merged.entry(name).or_insert_with(|| Value::Object(Map::new()));
            if !target.is_object() {
                *target = Value::Object(Map::new());
            }
            let target = target.as_object_mut().expect("made an object above");
            for (field, value) in section {
                target.insert(field.clone(), value.clone());
                from_file.push(format!("{name}.{field}"));
            }
        }
        check(&merged)?;
        Ok((merged, from_file))
    }
}

fn check(config: &ServiceConfig) -> Result<(), AppFileError> {
    BuildSettings::from_config(config).map_err(|error| AppFileError(format!("build: {error}")))?;
    RuntimeSettings::from_config(config).map_err(|error| AppFileError(format!("runtime: {error}")))?;
    Ok(())
}

/// JSON without `//` and `/* */` comments, which become spaces so error positions stay right.
fn strip_comments(text: &str) -> Result<String, &'static str> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => out.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                    out.push(' ');
                }
            }
            ('/', Some('*')) => {
                chars.next();
                out.push_str("  ");
                let mut closed = false;
                while let Some(c) = chars.next() {
                    if c == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        out.push_str("  ");
                        closed = true;
                        break;
                    }
                    out.push(if c == '\n' { '\n' } else { ' ' });
                }
                if !closed {
                    return Err("a /* comment is not closed");
                }
            }
            _ => out.push(c),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reads_a_file_with_comments() {
        let file = AppFile::parse(
            r#"{
              "$schema": "https://spinikus.dev/schema/v1.json", // editors validate with this
              /* the build */
              "build": { "dockerfile": "docker/Dockerfile" },
              "runtime": { "port": 3000, "healthcheck": { "path": "/a//b" } }
            }"#,
        )
        .unwrap();

        assert_eq!(file.build.unwrap()["dockerfile"], "docker/Dockerfile");
        assert_eq!(file.runtime.unwrap()["healthcheck"]["path"], "/a//b");
    }

    #[test]
    fn rejects_what_it_cannot_apply() {
        let problem = |text: &str| AppFile::parse(text).unwrap_err().to_string();

        assert!(problem(r#"{ "cron": [] }"#).contains("unknown field `cron`"), "{}", problem(r#"{ "cron": [] }"#));
        assert!(problem(r#"{ "runtime": { "release": "x" } }"#).contains("runtime: unknown field `release`"));
        assert!(problem(r#"{ "build": { "strategy": "compose" } }"#).contains("build: the `compose` build strategy"));
        assert!(problem(r#"{ "build": 3 }"#).starts_with("spinikus.json: invalid type"));
        assert!(problem("{ /* open").contains("not closed"));
        assert!(problem(&format!(r#"{{ "$schema": "{}" }}"#, "x".repeat(MAX_APP_FILE_SIZE))).contains("larger than 64 KiB"));
    }

    #[test]
    fn the_file_wins_field_by_field() {
        let config = json!({
            "build": { "dockerfile": "Dockerfile.prod", "target": "app" },
            "runtime": { "port": 8080, "healthcheck": { "path": "/up", "retries": 5 }, "shutdown_timeout": "20s" },
            "other": true,
        });
        let file = AppFile::parse(r#"{ "build": { "dockerfile": "web/Dockerfile" }, "runtime": { "port": 3000, "healthcheck": { "path": "/healthz" } } }"#)
            .unwrap();

        let (merged, from_file) = file.apply(config.as_object().unwrap()).unwrap();

        assert_eq!(
            Value::Object(merged),
            json!({
                "build": { "dockerfile": "web/Dockerfile", "target": "app" },
                "runtime": { "port": 3000, "healthcheck": { "path": "/healthz" }, "shutdown_timeout": "20s" },
                "other": true,
            })
        );
        assert_eq!(from_file, ["build.dockerfile", "runtime.healthcheck", "runtime.port"]);
    }

    #[test]
    fn the_merged_config_must_be_valid() {
        // A health check path from the file needs a port from somewhere.
        let file = AppFile::parse(r#"{ "runtime": { "port": 3000, "healthcheck": { "path": "/up" } } }"#).unwrap();
        let without_port =
            AppFile { runtime: Some(json!({ "healthcheck": { "path": "/up" } }).as_object().unwrap().clone()), ..file.clone() };

        assert!(file.apply(&ServiceConfig::new()).is_ok());
        assert!(without_port.apply(&ServiceConfig::new()).unwrap_err().to_string().contains("needs a port"));
        let with_port = json!({ "runtime": { "port": 80 } });
        assert!(without_port.apply(with_port.as_object().unwrap()).is_ok());
    }
}
