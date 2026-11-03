//! The audit log: everything for instance admins, a project's part for its admins.

use axum::{extract::State, response::Json};
use serde::{Deserialize, Serialize};
use spinikus_control::Control;
use spinikus_core::{
    audit::AuditEntry,
    id::{AuditEntryId, ProjectId},
    user::User,
};

use crate::{
    error::ApiError,
    extract::{Authenticated, Query},
};

#[derive(Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct Params {
    project_id: Option<ProjectId>,
    /// The `next_cursor` of the previous page: entries older than it.
    before: Option<AuditEntryId>,
    limit: Option<u32>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = AuditList)]
pub struct List {
    items: Vec<AuditEntry>,
    /// Pass as `before` for older entries; `null` on the last page.
    next_cursor: Option<String>,
    /// The users who acted in these entries, to name them.
    users: Vec<User>,
}

/// Entries newest first.
#[utoipa::path(
    get,
    path = "/audit-log",
    operation_id = "audit_list",
    tag = "audit-log",
    params(Params),
    responses((status = 200, body = List)),
)]
pub async fn list(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Query(params): Query<Params>,
) -> Result<Json<List>, ApiError> {
    let page = control.list_audit_entries(&principal, params.project_id, params.before, params.limit.unwrap_or(50)).await?;
    let entries = page.entries;
    let next_cursor = if entries.has_more { entries.items.last().map(|entry| entry.id.to_string()) } else { None };
    Ok(Json(List { items: entries.items, next_cursor, users: page.users }))
}
