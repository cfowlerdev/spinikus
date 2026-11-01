//! A service's build settings: the `build` object of its config, in the same shape as
//! `build` in `spinikus.json`.
//!
//! ```json
//! { "build": { "dockerfile": "docker/Dockerfile", "context": ".", "target": "release", "args": { "NODE_ENV": "production" } } }
//! { "build": { "strategy": "railpack", "command": "npm run build:prod" } }
//! ```
//!
//! Without a strategy, a repository with a Dockerfile is built from it and one without is
//! built by Railpack, which detects the language and framework. Paths are relative to the
//! source's root directory. Unknown fields are rejected, and so are build strategies
//! Spinikus cannot run yet and settings that don't apply to the chosen strategy.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::{
    id::{DeploymentId, ServiceId},
    service::ServiceConfig,
    source::relative_path_problem,
};

/// The tag of the image built for a deployment, `spinikus/<service>:<deployment>`, in lower
/// case as image references require. Deployments run the image by ID; the tag keeps it
/// until its deployment is cleaned up.
pub fn image_tag(service: ServiceId, deployment: DeploymentId) -> String {
    format!("spinikus/{}:{}", service.to_string().to_lowercase(), deployment.to_string().to_lowercase())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildStrategy {
    Dockerfile,
    Compose,
    Railpack,
    Buildpacks,
}

impl BuildStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dockerfile => "dockerfile",
            Self::Compose => "compose",
            Self::Railpack => "railpack",
            Self::Buildpacks => "buildpacks",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildSettings {
    /// How to build: a Dockerfile, or Railpack. Chosen by whether there is a Dockerfile if
    /// left out (see [`BuildSettings::strategy`]).
    pub strategy: Option<BuildStrategy>,
    /// Railpack only: the build command, in place of the one Railpack detects.
    pub command: Option<String>,
    /// The Dockerfile, `Dockerfile` by default. It must be inside the context.
    pub dockerfile: Option<String>,
    /// The directory sent to the builder, the root directory by default.
    pub context: Option<String>,
    /// The stage of a multi-stage Dockerfile to build, the last one by default.
    pub target: Option<String>,
    /// Build arguments. Not for secrets: they can end up in the image's history.
    #[serde(default)]
    pub args: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct BuildSettingsError(String);

impl BuildSettings {
    /// The settings in `config`, defaults if it has no `build` object.
    pub fn from_config(config: &ServiceConfig) -> Result<Self, BuildSettingsError> {
        let Some(build) = config.get("build") else {
            return Ok(Self::default());
        };
        let settings: Self = serde_json::from_value(build.clone()).map_err(|error| BuildSettingsError(error.to_string()))?;
        settings.check()?;
        Ok(settings)
    }

    fn check(&self) -> Result<(), BuildSettingsError> {
        match self.strategy {
            None | Some(BuildStrategy::Dockerfile | BuildStrategy::Railpack) => {}
            Some(strategy) => {
                let problem =
                    format!("the `{}` build strategy is not supported yet; use `dockerfile` or `railpack`", strategy.as_str());
                return Err(BuildSettingsError(problem));
            }
        }
        if self.strategy == Some(BuildStrategy::Railpack) {
            let dockerfile_only = [
                ("dockerfile", self.dockerfile.is_some()),
                ("context", self.context.is_some()),
                ("target", self.target.is_some()),
                ("args", !self.args.is_empty()),
            ];
            if let Some((field, _)) = dockerfile_only.into_iter().find(|(_, set)| *set) {
                return Err(BuildSettingsError(format!("{field} is for Dockerfile builds, not `railpack`")));
            }
        }
        if let Some(command) = &self.command {
            if self.strategy == Some(BuildStrategy::Dockerfile) || self.dockerfile.is_some() {
                return Err(BuildSettingsError("command is for Railpack builds; a Dockerfile says how to build".to_string()));
            }
            if command.trim().is_empty() || command.contains('\0') {
                return Err(BuildSettingsError("command is empty".to_string()));
            }
        }
        for (field, path) in [("dockerfile", &self.dockerfile), ("context", &self.context)] {
            if let Some(path) = path
                && let Some(problem) = path.is_empty().then_some("it is empty").or_else(|| relative_path_problem(path))
            {
                return Err(BuildSettingsError(format!("{field} `{}`: {problem}", path.escape_debug())));
            }
        }
        if let Some(target) = &self.target
            && (target.is_empty() || !target.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte)))
        {
            return Err(BuildSettingsError(format!("target `{}` is not a stage name", target.escape_debug())));
        }
        for name in self.args.keys() {
            if name.is_empty() || name.contains('=') || name.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(BuildSettingsError(format!("build argument name `{}` is invalid", name.escape_debug())));
            }
        }
        Ok(())
    }

    /// How to build, given whether the Dockerfile ([`Self::dockerfile`]) exists: the strategy
    /// set, else a Dockerfile build if there is one (or one was named, so its absence is an
    /// error rather than a different build), else Railpack.
    pub fn strategy(&self, dockerfile_exists: bool) -> BuildStrategy {
        match self.strategy {
            Some(strategy) => strategy,
            None if dockerfile_exists || self.dockerfile.is_some() || self.target.is_some() => BuildStrategy::Dockerfile,
            None => BuildStrategy::Railpack,
        }
    }

    pub fn dockerfile(&self) -> &str {
        self.dockerfile.as_deref().unwrap_or("Dockerfile")
    }

    pub fn context(&self) -> &str {
        self.context.as_deref().unwrap_or(".")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn config(build: serde_json::Value) -> ServiceConfig {
        json!({ "build": build }).as_object().unwrap().clone()
    }

    #[test]
    fn reads_the_build_object() {
        let settings = BuildSettings::from_config(&config(json!({
            "strategy": "dockerfile",
            "dockerfile": "docker/Dockerfile",
            "context": "./app",
            "target": "release",
            "args": { "NODE_ENV": "production" },
        })))
        .unwrap();

        assert_eq!((settings.dockerfile(), settings.context()), ("docker/Dockerfile", "./app"));
        assert_eq!(settings.target.as_deref(), Some("release"));
        assert_eq!(settings.args["NODE_ENV"], "production");
        let defaults = BuildSettings::from_config(&ServiceConfig::new()).unwrap();
        assert_eq!((defaults.dockerfile(), defaults.context()), ("Dockerfile", "."));
    }

    #[test]
    fn a_dockerfile_decides_unless_a_strategy_is_set() {
        let strategy = |build, exists| BuildSettings::from_config(&config(build)).unwrap().strategy(exists);

        assert_eq!(strategy(json!({}), true), BuildStrategy::Dockerfile);
        assert_eq!(strategy(json!({}), false), BuildStrategy::Railpack);
        assert_eq!(strategy(json!({ "command": "npm run build" }), false), BuildStrategy::Railpack);
        assert_eq!(strategy(json!({ "strategy": "railpack" }), true), BuildStrategy::Railpack);
        // A Dockerfile that was named but is missing is an error, not a Railpack build.
        assert_eq!(strategy(json!({ "dockerfile": "docker/Dockerfile" }), false), BuildStrategy::Dockerfile);
        assert_eq!(strategy(json!({ "strategy": "dockerfile" }), false), BuildStrategy::Dockerfile);
    }

    #[test]
    fn rejects_what_it_cannot_build() {
        let problem = |build| BuildSettings::from_config(&config(build)).unwrap_err().to_string();

        assert!(problem(json!({ "strategy": "compose" })).contains("`compose` build strategy is not supported yet"));
        assert_eq!(
            problem(json!({ "strategy": "railpack", "target": "app" })),
            "target is for Dockerfile builds, not `railpack`"
        );
        assert!(problem(json!({ "strategy": "dockerfile", "command": "make" })).starts_with("command is for Railpack builds"));
        assert!(problem(json!({ "dockerfile": "Dockerfile.prod", "command": "make" })).starts_with("command is for Railpack"));
        assert_eq!(problem(json!({ "command": " " })), "command is empty");
        assert!(problem(json!({ "strategy": "magic" })).contains("unknown variant"));
        assert!(problem(json!({ "cache": true })).contains("unknown field `cache`"));
        assert!(problem(json!({ "dockerfile": "../Dockerfile" })).contains("dockerfile `../Dockerfile`"));
        assert!(problem(json!({ "context": "/srv" })).contains("context `/srv`"));
        assert!(problem(json!({ "target": "a b" })).contains("not a stage name"));
        assert!(problem(json!({ "args": { "A=B": "c" } })).contains("build argument name"));
    }
}
