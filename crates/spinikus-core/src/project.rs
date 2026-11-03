use jiff::Timestamp;
use serde::Serialize;

use crate::{id::ProjectId, slug::Slug};

/// The top of the hierarchy: a project has environments, which have services.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    /// Unique among projects that are not deleted.
    pub slug: Slug,
    /// Incremented on every update, for optimistic concurrency (`If-Match`).
    pub revision: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// The fields a user sets when creating a project, or replaces when updating one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectFields {
    pub name: String,
    pub slug: Slug,
}
