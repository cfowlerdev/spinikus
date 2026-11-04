//! Notification channels and notifications. Instance admins manage the channels (a webhook,
//! Slack, Discord, Telegram or email, each with the events it gets) and send a test to one.
//! What happens elsewhere in the control plane calls [`Control::notify`] with a [`Message`],
//! which is queued once for each channel that wants its event; the runner
//! (`runner.rs`) sends them, retrying on failure, and also notices servers that stay
//! disconnected. Delivery (`delivery.rs`) never puts a channel's secret in an error: secret
//! URLs are left out of request errors.

use std::collections::BTreeMap;

use serde_json::json;
use spinikus_core::{
    id::{NotificationChannelId, NotificationId, ServiceId},
    notification::{
        ChannelFields, ChannelKind, ChannelSettings, Message, Notification, NotificationChannel, NotificationEvent,
        NotificationStatus, SecretUse, SmtpSecurity,
    },
    secret::Secret,
};
use spinikus_store::{EnvironmentRepository, NotificationRepository, ProjectRepository, SecretChange, ServiceRepository};

use crate::{
    Control, ControlError, Principal,
    backups::require_instance_admin,
    input,
    page::{self, Page},
};

pub mod delivery;
pub mod runner;

pub use runner::NotificationTimings;

/// The most addresses an email channel sends to.
pub const MAX_RECIPIENTS: usize = 10;

/// The longest URL, chat ID, host or address a channel takes.
const MAX_SETTING_LEN: usize = 2048;

/// A new channel.
#[derive(Debug, Clone)]
pub struct NewChannel {
    pub name: String,
    pub settings: ChannelSettings,
    pub events: Vec<NotificationEvent>,
    pub enabled: bool,
    pub secret: Option<Secret>,
}

/// Changes to a channel; `None` keeps a field. `secret`: `Some(Some(_))` replaces it,
/// `Some(None)` removes it. Changing the kind removes the secret unless a new one is given.
#[derive(Debug, Clone, Default)]
pub struct ChannelChanges {
    pub name: Option<String>,
    pub settings: Option<ChannelSettings>,
    pub events: Option<Vec<NotificationEvent>>,
    pub enabled: Option<bool>,
    pub secret: Option<Option<Secret>>,
}

impl Control {
    pub async fn create_notification_channel(
        &self,
        principal: &Principal,
        new: NewChannel,
    ) -> Result<NotificationChannel, ControlError> {
        require_instance_admin(principal)?;
        let settings = check_settings(new.settings)?;
        let secret = new.secret.filter(|secret| !secret.expose().is_empty());
        let secret = check_secret(settings.kind(), secret.as_ref())?;
        let fields = ChannelFields { name: input::name(&new.name)?, settings, events: events(new.events), enabled: new.enabled };
        let channel = self.store().create_notification_channel(fields, secret).await?;
        let details = json!({ "name": channel.fields.name, "kind": channel.fields.settings.kind().as_str() });
        self.audit_by(principal, "notification_channel.create", channel.id, None, details).await?;
        Ok(channel)
    }

    pub async fn list_notification_channels(&self, principal: &Principal) -> Result<Vec<NotificationChannel>, ControlError> {
        require_instance_admin(principal)?;
        Ok(self.store().list_notification_channels().await?)
    }

    pub async fn get_notification_channel(
        &self,
        principal: &Principal,
        id: NotificationChannelId,
    ) -> Result<NotificationChannel, ControlError> {
        require_instance_admin(principal)?;
        Ok(self.store().get_notification_channel(id).await?)
    }

    /// Applies `changes` if the channel is still at `revision`.
    pub async fn update_notification_channel(
        &self,
        principal: &Principal,
        id: NotificationChannelId,
        revision: i64,
        changes: ChannelChanges,
    ) -> Result<NotificationChannel, ControlError> {
        require_instance_admin(principal)?;
        let current = self.store().get_notification_channel(id).await?;
        let mut changed: Vec<(&'static str, bool)> = Vec::new();
        let mut fields = current.fields.clone();
        if let Some(name) = changes.name {
            fields.name = input::name(&name)?;
            changed.push(("name", true));
        }
        if let Some(settings) = changes.settings {
            fields.settings = check_settings(settings)?;
            changed.push(("settings", true));
        }
        if let Some(events) = changes.events {
            fields.events = self::events(events);
            changed.push(("events", true));
        }
        if let Some(enabled) = changes.enabled {
            fields.enabled = enabled;
            changed.push(("enabled", true));
        }
        let kind = fields.settings.kind();
        let kind_changed = kind != current.fields.settings.kind();
        let new_secret = match &changes.secret {
            Some(Some(secret)) if !secret.expose().is_empty() => Some(secret),
            _ => None,
        };
        let secret = match (new_secret, &changes.secret, kind_changed) {
            (Some(secret), _, _) => SecretChange::Set(check_secret(kind, Some(secret))?.expect("given")),
            (None, Some(_), _) | (None, None, true) => {
                check_secret(kind, None)?;
                SecretChange::Clear
            }
            (None, None, false) => {
                if !current.has_secret {
                    check_secret(kind, None)?;
                }
                SecretChange::Keep
            }
        };
        if !matches!(secret, SecretChange::Keep) {
            changed.push(("secret", true));
        }
        let channel = self.store().update_notification_channel(id, revision, fields, secret).await?;
        self.audit_by(principal, "notification_channel.update", id, None, crate::audit::changed(&changed)).await?;
        Ok(channel)
    }

    pub async fn delete_notification_channel(
        &self,
        principal: &Principal,
        id: NotificationChannelId,
    ) -> Result<(), ControlError> {
        require_instance_admin(principal)?;
        let channel = self.store().get_notification_channel(id).await?;
        self.store().delete_notification_channel(id).await?;
        self.audit_by(principal, "notification_channel.delete", id, None, json!({ "name": channel.fields.name })).await?;
        Ok(())
    }

    /// Sends a test message to the channel now, enabled or not, and says how it went.
    pub async fn test_notification_channel(&self, principal: &Principal, id: NotificationChannelId) -> Result<(), ControlError> {
        require_instance_admin(principal)?;
        let channel = self.store().get_notification_channel(id).await?;
        let secret = self.store().notification_channel_secret(id).await?;
        let message = Message {
            event: NotificationEvent::ServiceRecovered,
            title: "A test from Spinikus".to_string(),
            text: format!("Notifications from Spinikus reach the channel `{}`.", channel.fields.name),
            path: Some("/settings/notifications".to_string()),
            details: BTreeMap::from([("test".to_string(), "true".to_string())]),
        };
        let url = self.dashboard_url(message.path.as_deref()).await;
        let test = Notification {
            id: NotificationId::new(),
            channel_id: id,
            event: message.event,
            title: message.title,
            text: message.text,
            url,
            details: message.details,
            status: NotificationStatus::Queued,
            attempts: 0,
            error: None,
            created_at: jiff::Timestamp::now(),
            next_attempt_at: None,
            sent_at: None,
        };
        let delivered = self.notifications.delivery.deliver(&channel, secret.as_ref(), &test).await;
        delivered.map_err(|problem| ControlError::Rejected {
            entity: "notification channel",
            id: id.to_string(),
            problem: format!("the test was not delivered: {problem}"),
        })
    }

    /// The channel's notifications, newest first.
    pub async fn list_notifications(
        &self,
        principal: &Principal,
        channel_id: NotificationChannelId,
        after: Option<NotificationId>,
        limit: u32,
    ) -> Result<Page<Notification>, ControlError> {
        require_instance_admin(principal)?;
        self.store().get_notification_channel(channel_id).await?;
        let limit = page::limit(limit);
        let items = self.store().list_notifications(channel_id, after, limit + 1).await?;
        Ok(Page::from_overfetch(items, limit))
    }

    /// Queues `message` for every channel that wants it. Best effort: a failure is logged,
    /// never the caller's.
    pub(crate) async fn notify(&self, message: Message) {
        if let Err(error) = self.queue_message(&message).await {
            tracing::error!(%error, event = message.event.as_str(), "failed to queue a notification");
        }
    }

    async fn queue_message(&self, message: &Message) -> Result<(), ControlError> {
        let channels: Vec<NotificationChannelId> = self
            .store()
            .list_notification_channels()
            .await?
            .into_iter()
            .filter(|channel| channel.fields.wants(message.event))
            .map(|channel| channel.id)
            .collect();
        if channels.is_empty() {
            return Ok(());
        }
        let url = self.dashboard_url(message.path.as_deref()).await;
        self.store().queue_notifications(message, url.as_deref(), &channels).await?;
        tracing::info!(event = message.event.as_str(), title = message.title, channels = channels.len(), "queued a notification");
        self.wake_notifications();
        Ok(())
    }

    /// The dashboard's address for `path`, when its domain is set.
    async fn dashboard_url(&self, path: Option<&str>) -> Option<String> {
        let path = path?;
        match self.dashboard_domain().await {
            Ok(Some(domain)) => Some(format!("https://{domain}{path}")),
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(%error, "could not read the dashboard's domain for a notification");
                None
            }
        }
    }

    /// The service's `project/environment/service` path and its IDs, for messages about it.
    pub(crate) async fn service_subject(&self, service_id: ServiceId) -> (String, BTreeMap<String, String>) {
        let mut details = BTreeMap::from([("service_id".to_string(), service_id.to_string())]);
        let Ok(service) = self.store().get_service(service_id).await else {
            return (service_id.to_string(), details);
        };
        let Ok(environment) = self.store().get_environment(service.environment_id).await else {
            return (service.slug.to_string(), details);
        };
        details.insert("environment_id".to_string(), environment.id.to_string());
        details.insert("project_id".to_string(), environment.project_id.to_string());
        let project = match self.store().get_project(environment.project_id).await {
            Ok(project) => project.slug.to_string(),
            Err(_) => environment.project_id.to_string(),
        };
        (format!("{project}/{}/{}", environment.slug, service.slug), details)
    }
}

/// Each event once, in a fixed order.
fn events(mut events: Vec<NotificationEvent>) -> Vec<NotificationEvent> {
    events.sort();
    events.dedup();
    events
}

fn invalid(field: &'static str, problem: impl Into<String>) -> ControlError {
    ControlError::Invalid { field, problem: problem.into() }
}

fn check_settings(settings: ChannelSettings) -> Result<ChannelSettings, ControlError> {
    Ok(match settings {
        ChannelSettings::Webhook { url } => ChannelSettings::Webhook { url: http_url("settings.url", &url, false)? },
        ChannelSettings::Slack {} => ChannelSettings::Slack {},
        ChannelSettings::Discord {} => ChannelSettings::Discord {},
        ChannelSettings::Telegram { chat_id } => {
            let chat_id = chat_id.trim().to_string();
            if chat_id.is_empty() || chat_id.len() > 64 || chat_id.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(invalid("settings.chat_id", "it must be a chat's ID, such as -1001234567890, or @channel"));
            }
            ChannelSettings::Telegram { chat_id }
        }
        ChannelSettings::Email { host, port, security, username, from, to } => {
            let host = host.trim().to_string();
            if host.is_empty() || host.len() > 253 || host.chars().any(|c| c.is_whitespace() || c.is_control() || c == '/') {
                return Err(invalid("settings.host", "it must be the SMTP server's host name"));
            }
            if port == 0 {
                return Err(invalid("settings.port", "it must be a port, such as 587"));
            }
            let username = username.map(|name| name.trim().to_string()).filter(|name| !name.is_empty());
            if username.as_ref().is_some_and(|name| name.len() > 256 || name.chars().any(char::is_control)) {
                return Err(invalid("settings.username", "it is too long or has control characters"));
            }
            let from = mailbox("settings.from", &from)?;
            let to = to.iter().map(|address| mailbox("settings.to", address)).collect::<Result<Vec<_>, _>>()?;
            if to.is_empty() || to.len() > MAX_RECIPIENTS {
                return Err(invalid("settings.to", format!("it must have 1 to {MAX_RECIPIENTS} addresses")));
            }
            let _: SmtpSecurity = security;
            ChannelSettings::Email { host, port, security, username, from, to }
        }
    })
}

/// An email address, optionally with a name (`Ops <ops@example.com>`), as written.
fn mailbox(field: &'static str, value: &str) -> Result<String, ControlError> {
    let value = value.trim();
    match value.parse::<lettre::message::Mailbox>() {
        Ok(_) if value.len() <= 320 => Ok(value.to_string()),
        _ => Err(invalid(field, format!("`{value}` is not an email address"))),
    }
}

/// An `http(s)` URL with a host (`https` only if `https_only`).
fn http_url(field: &'static str, value: &str, https_only: bool) -> Result<String, ControlError> {
    let value = value.trim();
    let parsed = reqwest::Url::parse(value).map_err(|_| invalid(field, "it must be a URL, such as https://example.com/hook"))?;
    let scheme_ok = parsed.scheme() == "https" || (!https_only && parsed.scheme() == "http");
    if !scheme_ok || parsed.host_str().is_none_or(str::is_empty) || value.len() > MAX_SETTING_LEN {
        let scheme = if https_only { "an https" } else { "an http or https" };
        return Err(invalid(field, format!("it must be {scheme} URL with a host")));
    }
    Ok(value.to_string())
}

/// The secret the kind takes, checked; `None` where it is optional and not given.
fn check_secret(kind: ChannelKind, secret: Option<&Secret>) -> Result<Option<&Secret>, ControlError> {
    let Some(secret) = secret else {
        return match kind.secret() {
            SecretUse::Required(what) => Err(invalid("secret", format!("a {} channel needs its {what}", kind.as_str()))),
            SecretUse::Optional(_) => Ok(None),
        };
    };
    let value = secret.expose();
    if value.len() > MAX_SETTING_LEN || value.chars().any(char::is_control) {
        return Err(invalid("secret", "it is too long or has control characters"));
    }
    match kind {
        ChannelKind::Slack | ChannelKind::Discord => {
            http_url("secret", value, true)?;
        }
        ChannelKind::Telegram if value.contains(['/', '?', '#', ' ']) => {
            return Err(invalid("secret", "it must be a bot token, such as 123456:ABC-DEF…"));
        }
        _ => {}
    }
    Ok(Some(secret))
}

#[cfg(test)]
mod tests;
