use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{
    id::{EnvironmentId, ServiceId},
    slug::Slug,
    text::impl_text
};

/// Kind-specific settings, stored as a JSON object. Typed configs per kind come later.
pub type ServiceConfig = serde_json::Map<String, serde_json::Value>;

/// Something that runs as containers in an environment: an app, a database or a template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Service {
    pub id: ServiceId,
    pub environment_id: EnvironmentId,
    pub name: String,
    /// Unique among the environment's services that are not deleted.
    pub slug: Slug,
    /// Fixed when the service is created.
    pub kind: ServiceKind,
    pub config: ServiceConfig,
    pub revision: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceKind {
    App,
    Database,
    Template
}

impl ServiceKind {
    pub const ALL: [ServiceKind; 3] = [Self::App, Self::Database, Self::Template];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Database => "database",
            Self::Template => "template"
        }
    }
}

impl_text!(ServiceKind, "service kind");

/// The fields a user sets when creating a service, or replaces when updating one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceFields {
    pub name: String,
    pub slug: Slug,
    pub config: ServiceConfig
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_text_round_trips() {
        for kind in ServiceKind::ALL {
            assert_eq!(kind.to_string().parse(), Ok(kind));
            assert_eq!(serde_json::to_string(&kind).unwrap(), format!("\"{kind}\""));
        }
    }
}
