use jiff::Timestamp;
use serde::Serialize;
use thiserror::Error;

use crate::{
    id::{EnvironmentId, ProjectId},
    slug::Slug,
};

/// A stage of a project, such as production or the preview for one pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Environment {
    pub id: EnvironmentId,
    pub project_id: ProjectId,
    pub name: String,
    /// Unique among the project's environments that are not deleted.
    pub slug: Slug,
    /// Fixed when the environment is created.
    #[serde(flatten)]
    pub kind: EnvironmentKind,
    pub revision: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// What the environment is for. A project has at most one production environment
/// and one preview per pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EnvironmentKind {
    Production,
    Staging,
    Preview { pr_number: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EnvironmentKindError {
    #[error("unknown environment kind `{0}`")]
    Unknown(String),
    #[error("a preview environment needs a pull request number")]
    MissingPrNumber,
    #[error("only preview environments have a pull request number")]
    UnexpectedPrNumber,
}

impl EnvironmentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Production => "production",
            Self::Staging => "staging",
            Self::Preview { .. } => "preview",
        }
    }

    pub fn pr_number(self) -> Option<u32> {
        match self {
            Self::Preview { pr_number } => Some(pr_number),
            _ => None,
        }
    }

    /// Rebuilds a kind from its stored parts, the kind name and the optional pull request number.
    pub fn from_parts(kind: &str, pr_number: Option<u32>) -> Result<Self, EnvironmentKindError> {
        match (kind, pr_number) {
            ("preview", Some(pr_number)) => Ok(Self::Preview { pr_number }),
            ("preview", None) => Err(EnvironmentKindError::MissingPrNumber),
            ("production" | "staging", Some(_)) => Err(EnvironmentKindError::UnexpectedPrNumber),
            ("production", None) => Ok(Self::Production),
            ("staging", None) => Ok(Self::Staging),
            (other, _) => Err(EnvironmentKindError::Unknown(other.to_string())),
        }
    }
}

/// The fields a user sets when creating an environment, or replaces when updating one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentFields {
    pub name: String,
    pub slug: Slug,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_parts() {
        for kind in [EnvironmentKind::Production, EnvironmentKind::Staging, EnvironmentKind::Preview { pr_number: 42 }] {
            assert_eq!(EnvironmentKind::from_parts(kind.as_str(), kind.pr_number()), Ok(kind));
        }
    }

    #[test]
    fn rejects_inconsistent_parts() {
        assert_eq!(EnvironmentKind::from_parts("preview", None), Err(EnvironmentKindError::MissingPrNumber));
        assert_eq!(EnvironmentKind::from_parts("staging", Some(1)), Err(EnvironmentKindError::UnexpectedPrNumber));
        assert!(matches!(EnvironmentKind::from_parts("qa", None), Err(EnvironmentKindError::Unknown(_))));
    }

    #[test]
    fn serializes_kind_inline() {
        let preview = serde_json::to_value(EnvironmentKind::Preview { pr_number: 7 }).unwrap();
        let production = serde_json::to_value(EnvironmentKind::Production).unwrap();

        assert_eq!(preview, serde_json::json!({ "kind": "preview", "pr_number": 7 }));
        assert_eq!(production, serde_json::json!({ "kind": "production" }));
    }
}
