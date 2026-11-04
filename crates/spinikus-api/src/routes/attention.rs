use axum::{extract::State, response::Json};
use serde::Serialize;
use spinikus_control::Control;
use spinikus_core::attention::AttentionItem;

use crate::{error::ApiError, extract::Authenticated};

#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = AttentionList)]
pub struct List {
    /// Servers first, then services, then Spinikus's own backups and notification channels.
    items: Vec<AttentionItem>,
}

/// What needs attention: servers that are away or low on disk, services that are crashing
/// or whose latest deployment failed, databases whose latest backup failed, HTTPS
/// certificates that couldn't be had or expire soon, a failed backup of Spinikus, and
/// notification channels that couldn't send. Each user gets only what they may see: a
/// service's problems need a role in its environment, the rest needs an instance admin.
#[utoipa::path(
    get,
    path = "/attention",
    operation_id = "attention_list",
    tag = "attention",
    responses((status = 200, body = List)),
)]
pub async fn list(State(control): State<Control>, Authenticated(principal): Authenticated) -> Result<Json<List>, ApiError> {
    Ok(Json(List { items: control.attention(&principal).await? }))
}
