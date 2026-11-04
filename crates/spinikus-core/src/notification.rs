//! Notifications: messages about things that need attention (a failed deployment, a crashing
//! service, a failed backup, …) sent to the instance's channels (a webhook, Slack, Discord,
//! Telegram or email). Each channel picks the events it gets. A message is queued once per
//! channel and sent, with retries, by the control plane.

use std::collections::BTreeMap;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{
    id::{NotificationChannelId, NotificationId},
    text::impl_text,
};

/// A place notifications go. Its secret (a webhook URL with a token in it, a bot token, a
/// password or a signing key) is write-only: only whether it has one is shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct NotificationChannel {
    pub id: NotificationChannelId,
    #[serde(flatten)]
    pub fields: ChannelFields,
    pub has_secret: bool,
    pub revision: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// What a user sets on a channel, besides its secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ChannelFields {
    pub name: String,
    pub settings: ChannelSettings,
    /// The events it gets; every event if empty.
    pub events: Vec<NotificationEvent>,
    pub enabled: bool,
}

impl ChannelFields {
    /// Whether the channel gets `event`.
    pub fn wants(&self, event: NotificationEvent) -> bool {
        self.enabled && (self.events.is_empty() || self.events.contains(&event))
    }
}

/// How to reach a channel, by kind. The secret each kind takes is in [`ChannelKind::secret`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChannelSettings {
    /// A JSON `POST` to `url`, signed with the secret if there is one.
    Webhook { url: String },
    /// A Slack incoming webhook (the secret is its URL).
    Slack {},
    /// A Discord webhook (the secret is its URL).
    Discord {},
    /// A Telegram bot's message to a chat (the secret is the bot's token).
    Telegram { chat_id: String },
    /// Email over SMTP (the secret is the password, if the server needs one).
    Email { host: String, port: u16, security: SmtpSecurity, username: Option<String>, from: String, to: Vec<String> },
}

impl ChannelSettings {
    pub fn kind(&self) -> ChannelKind {
        match self {
            Self::Webhook { .. } => ChannelKind::Webhook,
            Self::Slack {} => ChannelKind::Slack,
            Self::Discord {} => ChannelKind::Discord,
            Self::Telegram { .. } => ChannelKind::Telegram,
            Self::Email { .. } => ChannelKind::Email,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Webhook,
    Slack,
    Discord,
    Telegram,
    Email,
}

/// Whether a kind of channel needs a secret, and what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretUse {
    Required(&'static str),
    Optional(&'static str),
}

impl ChannelKind {
    pub const ALL: [ChannelKind; 5] = [Self::Webhook, Self::Slack, Self::Discord, Self::Telegram, Self::Email];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Webhook => "webhook",
            Self::Slack => "slack",
            Self::Discord => "discord",
            Self::Telegram => "telegram",
            Self::Email => "email",
        }
    }

    pub fn secret(self) -> SecretUse {
        match self {
            Self::Webhook => SecretUse::Optional("signing secret"),
            Self::Slack | Self::Discord => SecretUse::Required("webhook URL"),
            Self::Telegram => SecretUse::Required("bot token"),
            Self::Email => SecretUse::Optional("password"),
        }
    }
}

impl_text!(ChannelKind, "channel kind");

/// How email reaches the SMTP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum SmtpSecurity {
    /// TLS from the start, usually on port 465.
    Tls,
    /// Plain, then upgraded with STARTTLS (required), usually on port 587.
    Starttls,
    /// Plain throughout, for a relay on the same machine or network.
    None,
}

impl SmtpSecurity {
    pub const ALL: [SmtpSecurity; 3] = [Self::Tls, Self::Starttls, Self::None];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tls => "tls",
            Self::Starttls => "starttls",
            Self::None => "none",
        }
    }
}

impl_text!(SmtpSecurity, "SMTP security");

/// What a notification is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum NotificationEvent {
    #[serde(rename = "deployment.failed")]
    DeploymentFailed,
    #[serde(rename = "service.crashing")]
    ServiceCrashing,
    #[serde(rename = "service.recovered")]
    ServiceRecovered,
    #[serde(rename = "backup.failed")]
    BackupFailed,
    #[serde(rename = "instance_backup.failed")]
    InstanceBackupFailed,
    #[serde(rename = "certificate.failed")]
    CertificateFailed,
    #[serde(rename = "certificate.expiring")]
    CertificateExpiring,
    #[serde(rename = "server.disconnected")]
    ServerDisconnected,
    #[serde(rename = "server.reconnected")]
    ServerReconnected,
    #[serde(rename = "server.disk_low")]
    ServerDiskLow,
}

impl NotificationEvent {
    pub const ALL: [NotificationEvent; 10] = [
        Self::DeploymentFailed,
        Self::ServiceCrashing,
        Self::ServiceRecovered,
        Self::BackupFailed,
        Self::InstanceBackupFailed,
        Self::CertificateFailed,
        Self::CertificateExpiring,
        Self::ServerDisconnected,
        Self::ServerReconnected,
        Self::ServerDiskLow,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeploymentFailed => "deployment.failed",
            Self::ServiceCrashing => "service.crashing",
            Self::ServiceRecovered => "service.recovered",
            Self::BackupFailed => "backup.failed",
            Self::InstanceBackupFailed => "instance_backup.failed",
            Self::CertificateFailed => "certificate.failed",
            Self::CertificateExpiring => "certificate.expiring",
            Self::ServerDisconnected => "server.disconnected",
            Self::ServerReconnected => "server.reconnected",
            Self::ServerDiskLow => "server.disk_low",
        }
    }

    /// Words for it in lists and forms.
    pub fn label(self) -> &'static str {
        match self {
            Self::DeploymentFailed => "A deployment failed",
            Self::ServiceCrashing => "A service is crashing",
            Self::ServiceRecovered => "A crashing service recovered",
            Self::BackupFailed => "A database backup failed",
            Self::InstanceBackupFailed => "A backup of Spinikus failed",
            Self::CertificateFailed => "An HTTPS certificate couldn't be issued",
            Self::CertificateExpiring => "An HTTPS certificate expires soon",
            Self::ServerDisconnected => "A server is disconnected",
            Self::ServerReconnected => "A disconnected server is back",
            Self::ServerDiskLow => "A server is low on disk",
        }
    }

    /// Whether it is good news, for channels that color messages.
    pub fn is_good_news(self) -> bool {
        matches!(self, Self::ServiceRecovered | Self::ServerReconnected)
    }
}

impl_text!(NotificationEvent, "notification event");

/// What to say, once for every channel that gets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub event: NotificationEvent,
    /// One line, such as "shop/production/web is crashing".
    pub title: String,
    /// The rest, plain text, possibly several lines.
    pub text: String,
    /// Where to look in the dashboard, as a path such as `/services/svc_…`.
    pub path: Option<String>,
    /// IDs and names for webhooks, such as `service_id`.
    pub details: BTreeMap<String, String>,
}

/// A message queued for one channel, and how sending it went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Notification {
    pub id: NotificationId,
    pub channel_id: NotificationChannelId,
    pub event: NotificationEvent,
    pub title: String,
    pub text: String,
    /// A link to the dashboard, when its domain is set.
    pub url: Option<String>,
    #[cfg_attr(feature = "openapi", schema(value_type = Object))]
    pub details: BTreeMap<String, String>,
    pub status: NotificationStatus,
    pub attempts: u32,
    /// Why the last attempt failed.
    pub error: Option<String>,
    pub created_at: Timestamp,
    /// When it is tried next, while queued.
    pub next_attempt_at: Option<Timestamp>,
    pub sent_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum NotificationStatus {
    /// Waiting to be sent, or to be tried again.
    Queued,
    Sent,
    /// Given up on after its last attempt.
    Failed,
}

impl NotificationStatus {
    pub const ALL: [NotificationStatus; 3] = [Self::Queued, Self::Sent, Self::Failed];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Sent => "sent",
            Self::Failed => "failed",
        }
    }
}

impl_text!(NotificationStatus, "notification status");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_read_and_write_the_same_names() {
        for event in NotificationEvent::ALL {
            let json = serde_json::to_value(event).unwrap();
            assert_eq!(json, event.as_str());
            assert_eq!(event.as_str().parse::<NotificationEvent>().unwrap(), event);
        }
    }

    #[test]
    fn settings_are_tagged_by_kind() {
        let email = serde_json::json!({
            "kind": "email", "host": "smtp.example.com", "port": 587, "security": "starttls",
            "username": null, "from": "spinikus@example.com", "to": ["ops@example.com"]
        });
        let settings: ChannelSettings = serde_json::from_value(email.clone()).unwrap();
        let slack: ChannelSettings = serde_json::from_value(serde_json::json!({ "kind": "slack" })).unwrap();
        let unknown = serde_json::from_value::<ChannelSettings>(serde_json::json!({ "kind": "slack", "url": "x" }));

        assert_eq!(settings.kind(), ChannelKind::Email);
        assert_eq!(serde_json::to_value(&settings).unwrap(), email);
        assert_eq!(slack, ChannelSettings::Slack {});
        assert!(unknown.is_err(), "Slack's URL is its secret, never a setting");
    }

    #[test]
    fn a_channel_gets_its_events_or_all_while_enabled() {
        let fields = |events: Vec<NotificationEvent>, enabled| ChannelFields {
            name: "Ops".to_string(),
            settings: ChannelSettings::Slack {},
            events,
            enabled,
        };
        assert!(fields(Vec::new(), true).wants(NotificationEvent::BackupFailed));
        assert!(fields(vec![NotificationEvent::BackupFailed], true).wants(NotificationEvent::BackupFailed));
        assert!(!fields(vec![NotificationEvent::BackupFailed], true).wants(NotificationEvent::DeploymentFailed));
        assert!(!fields(Vec::new(), false).wants(NotificationEvent::BackupFailed));
    }
}
