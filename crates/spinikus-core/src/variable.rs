//! Environment variables, set on a project, an environment or a service. A service gets
//! all three layers, the most specific winning, resolved at deploy time.

use std::{collections::BTreeMap, fmt, str::FromStr};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    id::{EnvironmentId, ProjectId, ServiceId, VariableId},
    secret::Secret,
    text::impl_text,
};

/// Longest allowed variable name.
pub const MAX_NAME_LEN: usize = 256;

/// Names starting with this are reserved for variables Spinikus sets itself.
pub const RESERVED_PREFIX: &str = "SPINIKUS_";

/// A variable as users see it. The value of a secret is never included: secrets are
/// write-only, and reading one back takes a separate, audited reveal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Variable {
    pub id: VariableId,
    #[serde(flatten)]
    pub scope: VariableScope,
    pub name: VariableName,
    /// The value, for variables that are not secret.
    pub value: Option<String>,
    pub availability: VariableAvailability,
    pub secret: bool,
    pub revision: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// The fields a user sets when creating a variable, or replaces when updating one.
/// The value is passed separately, since an update can keep the current one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableFields {
    pub name: VariableName,
    pub availability: VariableAvailability,
    /// Once set, a variable stays secret: its value has been treated as one.
    pub secret: bool,
}

/// What a variable is set on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "scope", content = "scope_id", rename_all = "snake_case")]
pub enum VariableScope {
    /// Shared by every service in the project.
    Project(ProjectId),
    /// Shared by every service in the environment.
    Environment(EnvironmentId),
    Service(ServiceId),
}

impl VariableScope {
    pub fn level(self) -> ScopeLevel {
        match self {
            Self::Project(_) => ScopeLevel::Project,
            Self::Environment(_) => ScopeLevel::Environment,
            Self::Service(_) => ScopeLevel::Service,
        }
    }
}

/// Scope levels, ordered from least to most specific: a more specific level wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ScopeLevel {
    Project,
    Environment,
    Service,
}

impl ScopeLevel {
    pub const ALL: [ScopeLevel; 3] = [Self::Project, Self::Environment, Self::Service];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Environment => "environment",
            Self::Service => "service",
        }
    }
}

/// When a variable is passed to the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum VariableAvailability {
    /// Set in the running container's environment.
    Runtime,
    /// Passed to the build only. Plain build values (build arguments) can end up in image
    /// layers; secret ones are BuildKit secrets, which don't.
    Build,
    Both,
}

impl VariableAvailability {
    pub const ALL: [VariableAvailability; 3] = [Self::Runtime, Self::Build, Self::Both];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Build => "build",
            Self::Both => "both",
        }
    }

    pub fn at_build(self) -> bool {
        matches!(self, Self::Build | Self::Both)
    }

    pub fn at_runtime(self) -> bool {
        matches!(self, Self::Runtime | Self::Both)
    }
}

impl_text!(ScopeLevel, "scope level");
impl_text!(VariableAvailability, "variable availability");

/// An environment variable name: letters, digits and underscores, not starting with a
/// digit, for example `DATABASE_URL`. Names are case-sensitive, like on Linux.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VariableName(String);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("`{value}` is not a valid variable name: {problem}")]
pub struct VariableNameError {
    value: String,
    problem: &'static str,
}

impl VariableName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for VariableName {
    type Err = VariableNameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let problem = if value.is_empty() {
            Some("it is empty")
        } else if value.len() > MAX_NAME_LEN {
            Some("it is longer than 256 characters")
        } else if !value.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') {
            Some("only letters, digits and underscores are allowed")
        } else if value.as_bytes()[0].is_ascii_digit() {
            Some("it cannot start with a digit")
        } else if value.starts_with(RESERVED_PREFIX) {
            Some("names starting with SPINIKUS_ are reserved")
        } else {
            None
        };

        match problem {
            Some(problem) => Err(VariableNameError { value: value.to_string(), problem }),
            None => Ok(Self(value.to_string())),
        }
    }
}

impl fmt::Display for VariableName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for VariableName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for VariableName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// A variable with its decrypted value, as a deployment gets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedVariable {
    pub name: VariableName,
    pub value: Secret,
    pub availability: VariableAvailability,
    pub secret: bool,
    /// Where the winning value was set, so the UI can show inherited variables.
    pub level: ScopeLevel,
    pub variable_id: VariableId,
}

/// Applies precedence: for each name, the variable from the most specific scope wins.
/// Returns the winners sorted by name.
pub fn resolve(variables: impl IntoIterator<Item = ResolvedVariable>) -> Vec<ResolvedVariable> {
    let mut winners: BTreeMap<VariableName, ResolvedVariable> = BTreeMap::new();
    for variable in variables {
        match winners.get(&variable.name) {
            Some(current) if current.level >= variable.level => {}
            _ => {
                winners.insert(variable.name.clone(), variable);
            }
        }
    }
    winners.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(name: &str, value: &str, level: ScopeLevel) -> ResolvedVariable {
        ResolvedVariable {
            name: name.parse().unwrap(),
            value: Secret::new(value),
            availability: VariableAvailability::Runtime,
            secret: false,
            level,
            variable_id: VariableId::new(),
        }
    }

    #[test]
    fn accepts_valid_names() {
        for name in ["PORT", "DATABASE_URL", "_private", "node_env", "A1", &"A".repeat(MAX_NAME_LEN)] {
            assert_eq!(name.parse::<VariableName>().map(|name| name.to_string()), Ok(name.to_string()));
        }
    }

    #[test]
    fn rejects_invalid_names() {
        for name in ["", "1PORT", "MY-VAR", "MY VAR", "PÖRT", "SPINIKUS_URL", &"A".repeat(MAX_NAME_LEN + 1)] {
            assert!(name.parse::<VariableName>().is_err(), "{name:?} should be rejected");
        }
    }

    #[test]
    fn the_most_specific_scope_wins_whatever_the_order() {
        let variables = vec![
            resolved("PORT", "service", ScopeLevel::Service),
            resolved("PORT", "project", ScopeLevel::Project),
            resolved("LOG", "project", ScopeLevel::Project),
            resolved("PORT", "environment", ScopeLevel::Environment),
            resolved("API", "environment", ScopeLevel::Environment),
            resolved("API", "project", ScopeLevel::Project),
        ];

        let winners: Vec<_> = resolve(variables)
            .into_iter()
            .map(|variable| (variable.name.to_string(), variable.value.expose().to_string(), variable.level))
            .collect();

        assert_eq!(
            winners,
            vec![
                ("API".to_string(), "environment".to_string(), ScopeLevel::Environment),
                ("LOG".to_string(), "project".to_string(), ScopeLevel::Project),
                ("PORT".to_string(), "service".to_string(), ScopeLevel::Service)
            ]
        );
    }

    #[test]
    fn availability_flags() {
        assert!(VariableAvailability::Both.at_build() && VariableAvailability::Both.at_runtime());
        assert!(!VariableAvailability::Runtime.at_build());
        assert!(!VariableAvailability::Build.at_runtime());
    }

    #[test]
    fn serializes_the_scope_flat() {
        let service_id = ServiceId::new();
        let variable = Variable {
            id: VariableId::new(),
            scope: VariableScope::Service(service_id),
            name: "API_KEY".parse().unwrap(),
            value: None,
            availability: VariableAvailability::Both,
            secret: true,
            revision: 1,
            created_at: Timestamp::UNIX_EPOCH,
            updated_at: Timestamp::UNIX_EPOCH,
        };

        let json = serde_json::to_value(&variable).unwrap();

        assert_eq!(json["scope"], "service");
        assert_eq!(json["scope_id"], service_id.to_string());
        assert_eq!(json["value"], serde_json::Value::Null);
        assert_eq!(json["availability"], "both");
    }

    #[test]
    fn text_round_trips() {
        for level in ScopeLevel::ALL {
            assert_eq!(level.to_string().parse(), Ok(level));
        }
        for availability in VariableAvailability::ALL {
            assert_eq!(availability.to_string().parse(), Ok(availability));
        }
    }
}
