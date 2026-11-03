//! The audit log: an append-only record of who did what.

use std::{fmt, str::FromStr};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    actor::Actor,
    id::{ApiTokenId, AuditEntryId, ProjectId},
    text::dotted_name_problem,
};

/// Extra facts about an action, as a JSON object. Never secret values.
pub type AuditDetails = serde_json::Map<String, serde_json::Value>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AuditEntry {
    pub id: AuditEntryId,
    pub actor: Actor,
    /// The API token the user acted through, if any.
    pub token_id: Option<ApiTokenId>,
    pub action: AuditAction,
    /// The ID of the entity acted on, such as `var_…`, if there is one.
    pub target_id: Option<String>,
    /// The project the action happened in, so a project's admins can read its part of the log.
    pub project_id: Option<ProjectId>,
    #[cfg_attr(feature = "openapi", schema(value_type = Object))]
    pub details: AuditDetails,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAuditEntry {
    pub actor: Actor,
    pub token_id: Option<ApiTokenId>,
    pub action: AuditAction,
    pub target_id: Option<String>,
    pub project_id: Option<ProjectId>,
    pub details: AuditDetails,
}

/// What was done, as `entity.verb`: lowercase letters, digits and underscores in dot-separated
/// parts, such as `variable.reveal` or `membership.create`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AuditAction(String);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("`{value}` is not a valid audit action: {problem}")]
pub struct AuditActionError {
    value: String,
    problem: &'static str,
}

impl AuditAction {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for AuditAction {
    type Err = AuditActionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match dotted_name_problem(value) {
            Some(problem) => Err(AuditActionError { value: value.to_string(), problem }),
            None => Ok(Self(value.to_string())),
        }
    }
}

impl fmt::Display for AuditAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for AuditAction {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AuditAction {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_actions() {
        assert_eq!("variable.reveal".parse::<AuditAction>().unwrap().as_str(), "variable.reveal");
        for value in ["", "Variable.reveal", "variable reveal", "variable..reveal", "variable."] {
            assert!(value.parse::<AuditAction>().is_err(), "{value:?} should be rejected");
        }
    }
}
