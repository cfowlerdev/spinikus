//! Notification channels, for instance admins, and what was sent to them. Each channel's
//! secret (a Slack or Discord webhook URL, a Telegram bot token, an SMTP password or a
//! webhook's signing secret) is write-only: responses say only `has_secret`.

use axum::{
    extract::State,
    http::StatusCode,
    response::{Json, Response},
};
use serde::Deserialize;
use spinikus_control::{
    Control,
    notifications::{ChannelChanges, NewChannel},
};
use spinikus_core::{
    id::{NotificationChannelId, NotificationId},
    notification::{ChannelSettings, Notification, NotificationChannel, NotificationEvent},
    secret::Secret,
};

use crate::{
    error::ApiError,
    extract::{Authenticated, Body, IfMatch, Path, Query},
    response::{PageBody, PageParams, created, tagged},
    routes::present,
};

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = NotificationsCreate)]
pub struct Create {
    name: String,
    settings: ChannelSettings,
    /// The events it gets; every event if omitted or empty.
    #[serde(default)]
    events: Vec<NotificationEvent>,
    /// On unless `false`.
    enabled: Option<bool>,
    /// What the kind needs: Slack's or Discord's webhook URL, Telegram's bot token, the SMTP
    /// password, or the signing secret for a webhook. Write-only.
    secret: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = NotificationsUpdate)]
pub struct Update {
    name: Option<String>,
    /// Replaces the settings; a different kind drops the secret unless one is given.
    settings: Option<ChannelSettings>,
    events: Option<Vec<NotificationEvent>>,
    enabled: Option<bool>,
    /// Replaces the secret; `null` removes it; omitted keeps it.
    #[serde(default, deserialize_with = "present")]
    secret: Option<Option<String>>,
}

/// Lists the notification channels, for instance admins.
#[utoipa::path(
    get,
    path = "/notification-channels",
    operation_id = "notifications_list",
    tag = "notifications",
    responses((status = 200, body = Vec<NotificationChannel>)),
)]
pub async fn list(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
) -> Result<Json<Vec<NotificationChannel>>, ApiError> {
    Ok(Json(control.list_notification_channels(&principal).await?))
}

/// Creates a notification channel.
#[utoipa::path(
    post,
    path = "/notification-channels",
    operation_id = "notifications_create",
    tag = "notifications",
    request_body = Create,
    responses((status = 201, body = NotificationChannel, headers(("Location" = String, description = "Where the new resource is."), ("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn create(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Body(body): Body<Create>,
) -> Result<Response, ApiError> {
    let new = NewChannel {
        name: body.name,
        settings: body.settings,
        events: body.events,
        enabled: body.enabled.unwrap_or(true),
        secret: body.secret.map(Secret::new),
    };
    let channel = control.create_notification_channel(&principal, new).await?;
    Ok(created(format!("/api/v1/notification-channels/{}", channel.id), channel.revision, channel))
}

/// Gets a notification channel.
#[utoipa::path(
    get,
    path = "/notification-channels/{id}",
    operation_id = "notifications_show",
    tag = "notifications",
    params(("id" = NotificationChannelId, Path)),
    responses((status = 200, body = NotificationChannel, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<NotificationChannelId>,
) -> Result<Response, ApiError> {
    let channel = control.get_notification_channel(&principal, id).await?;
    Ok(tagged(channel.revision, channel))
}

/// Updates a notification channel.
#[utoipa::path(
    patch,
    path = "/notification-channels/{id}",
    operation_id = "notifications_update",
    tag = "notifications",
    params(("id" = NotificationChannelId, Path), crate::extract::IfMatch),
    request_body = Update,
    responses((status = 200, body = NotificationChannel, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn update(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<NotificationChannelId>,
    IfMatch(revision): IfMatch,
    Body(body): Body<Update>,
) -> Result<Response, ApiError> {
    let changes = ChannelChanges {
        name: body.name,
        settings: body.settings,
        events: body.events,
        enabled: body.enabled,
        secret: body.secret.map(|secret| secret.map(Secret::new)),
    };
    let channel = control.update_notification_channel(&principal, id, revision, changes).await?;
    Ok(tagged(channel.revision, channel))
}

/// Deletes a notification channel and what was sent to it.
#[utoipa::path(
    delete,
    path = "/notification-channels/{id}",
    operation_id = "notifications_delete",
    tag = "notifications",
    params(("id" = NotificationChannelId, Path)),
    responses((status = 204, description = "Done.")),
)]
pub async fn delete(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<NotificationChannelId>,
) -> Result<StatusCode, ApiError> {
    control.delete_notification_channel(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Sends a test message to the channel now, enabled or not. `204` if it was delivered; a
/// `rejected` problem says what went wrong.
#[utoipa::path(
    post,
    path = "/notification-channels/{id}/test",
    operation_id = "notifications_test",
    tag = "notifications",
    params(("id" = NotificationChannelId, Path)),
    responses((status = 204, description = "Done.")),
)]
pub async fn test(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<NotificationChannelId>,
) -> Result<StatusCode, ApiError> {
    control.test_notification_channel(&principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// What was sent to the channel, or is waiting to be, newest first. Kept for 30 days.
#[utoipa::path(
    get,
    path = "/notification-channels/{id}/notifications",
    operation_id = "notifications_list_sent",
    tag = "notifications",
    params(("id" = NotificationChannelId, Path), crate::response::PageQuery),
    responses((status = 200, body = PageBody<Notification>)),
)]
pub async fn list_sent(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(id): Path<NotificationChannelId>,
    Query(params): Query<PageParams<NotificationId>>,
) -> Result<Json<PageBody<Notification>>, ApiError> {
    let page = control.list_notifications(&principal, id, params.after, params.limit()).await?;
    Ok(PageBody::new(page, |notification| notification.id.to_string()))
}
