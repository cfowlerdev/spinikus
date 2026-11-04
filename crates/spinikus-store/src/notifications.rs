//! Notification channels, their sealed secrets, and the notifications queued for them.

use async_trait::async_trait;
use jiff::Timestamp;
use spinikus_core::{
    id::{NotificationChannelId, NotificationId},
    notification::{ChannelFields, ChannelSettings, Message, Notification, NotificationChannel, NotificationEvent},
    secret::Secret,
};

use crate::{SqliteStore, StoreError, certificates::open_sealed, db::format_timestamp, error::parse_column};

const CHANNEL: &str = "notification channel";
const NOTIFICATION: &str = "notification";

macro_rules! channel_columns {
    () => {
        "id, name, settings, events, enabled, secret IS NOT NULL AS has_secret, revision, created_at, updated_at"
    };
}

macro_rules! notification_columns {
    () => {
        "id, channel_id, event, title, text, url, details, status, attempts, error, created_at, next_attempt_at, sent_at"
    };
}

/// What happens to a channel's secret on an update.
#[derive(Debug, Clone, Copy)]
pub enum SecretChange<'a> {
    Keep,
    Set(&'a Secret),
    Clear,
}

/// How one attempt to send a notification went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attempt {
    Sent,
    /// Failed; tried again at the time given, or never.
    Failed {
        error: String,
        retry_at: Option<Timestamp>,
    },
}

#[async_trait]
pub trait NotificationRepository: Send + Sync {
    /// `Conflict` on `name` if another channel has it.
    async fn create_notification_channel(
        &self,
        fields: ChannelFields,
        secret: Option<&Secret>,
    ) -> Result<NotificationChannel, StoreError>;

    async fn get_notification_channel(&self, id: NotificationChannelId) -> Result<NotificationChannel, StoreError>;

    /// Every channel, by name.
    async fn list_notification_channels(&self) -> Result<Vec<NotificationChannel>, StoreError>;

    /// Replaces the fields if the channel is still at `revision`.
    async fn update_notification_channel(
        &self,
        id: NotificationChannelId,
        revision: i64,
        fields: ChannelFields,
        secret: SecretChange<'_>,
    ) -> Result<NotificationChannel, StoreError>;

    /// Deletes the channel and its notifications.
    async fn delete_notification_channel(&self, id: NotificationChannelId) -> Result<(), StoreError>;

    async fn notification_channel_secret(&self, id: NotificationChannelId) -> Result<Option<Secret>, StoreError>;

    /// Queues `message` for each channel, due now.
    async fn queue_notifications(
        &self,
        message: &Message,
        url: Option<&str>,
        channels: &[NotificationChannelId],
    ) -> Result<Vec<Notification>, StoreError>;

    /// Queued notifications due at `now`, the longest due first.
    async fn due_notifications(&self, now: Timestamp, limit: u32) -> Result<Vec<Notification>, StoreError>;

    /// Records an attempt at a queued notification.
    async fn record_notification_attempt(&self, id: NotificationId, attempt: Attempt) -> Result<Notification, StoreError>;

    /// The channel's notifications, newest first, starting after `after`.
    async fn list_notifications(
        &self,
        channel_id: NotificationChannelId,
        after: Option<NotificationId>,
        limit: u32,
    ) -> Result<Vec<Notification>, StoreError>;

    /// Deletes sent and failed notifications created before `before`, and says how many.
    async fn prune_notifications(&self, before: Timestamp) -> Result<u64, StoreError>;
}

#[derive(sqlx::FromRow)]
struct ChannelRow {
    id: String,
    name: String,
    settings: String,
    events: String,
    enabled: bool,
    has_secret: bool,
    revision: i64,
    created_at: String,
    updated_at: String,
}

fn invalid(entity: &'static str, column: &'static str) -> impl FnOnce(serde_json::Error) -> StoreError {
    move |error| StoreError::InvalidRow { entity, column, problem: error.to_string() }
}

impl TryFrom<ChannelRow> for NotificationChannel {
    type Error = StoreError;

    fn try_from(row: ChannelRow) -> Result<Self, Self::Error> {
        Ok(NotificationChannel {
            id: parse_column(CHANNEL, "id", &row.id)?,
            fields: ChannelFields {
                name: row.name,
                settings: serde_json::from_str::<ChannelSettings>(&row.settings).map_err(invalid(CHANNEL, "settings"))?,
                events: serde_json::from_str::<Vec<NotificationEvent>>(&row.events).map_err(invalid(CHANNEL, "events"))?,
                enabled: row.enabled,
            },
            has_secret: row.has_secret,
            revision: row.revision,
            created_at: parse_column(CHANNEL, "created_at", &row.created_at)?,
            updated_at: parse_column(CHANNEL, "updated_at", &row.updated_at)?,
        })
    }
}

#[derive(sqlx::FromRow)]
struct NotificationRow {
    id: String,
    channel_id: String,
    event: String,
    title: String,
    text: String,
    url: Option<String>,
    details: String,
    status: String,
    attempts: i64,
    error: Option<String>,
    created_at: String,
    next_attempt_at: Option<String>,
    sent_at: Option<String>,
}

impl TryFrom<NotificationRow> for Notification {
    type Error = StoreError;

    fn try_from(row: NotificationRow) -> Result<Self, Self::Error> {
        let optional = |column, value: Option<String>| value.map(|value| parse_column(NOTIFICATION, column, &value)).transpose();
        Ok(Notification {
            id: parse_column(NOTIFICATION, "id", &row.id)?,
            channel_id: parse_column(NOTIFICATION, "channel_id", &row.channel_id)?,
            event: parse_column(NOTIFICATION, "event", &row.event)?,
            title: row.title,
            text: row.text,
            url: row.url,
            details: serde_json::from_str(&row.details).map_err(invalid(NOTIFICATION, "details"))?,
            status: parse_column(NOTIFICATION, "status", &row.status)?,
            attempts: u32::try_from(row.attempts).unwrap_or(u32::MAX),
            error: row.error,
            created_at: parse_column(NOTIFICATION, "created_at", &row.created_at)?,
            next_attempt_at: optional("next_attempt_at", row.next_attempt_at)?,
            sent_at: optional("sent_at", row.sent_at)?,
        })
    }
}

fn name_taken(name: &str) -> impl FnOnce(&str) -> StoreError {
    let name = name.to_string();
    move |_| StoreError::Conflict { entity: CHANNEL, field: "name", value: name }
}

fn encode(fields: &ChannelFields) -> (String, String) {
    let settings = serde_json::to_string(&fields.settings).expect("settings serialize");
    let events = serde_json::to_string(&fields.events).expect("events serialize");
    (settings, events)
}

#[async_trait]
impl NotificationRepository for SqliteStore {
    async fn create_notification_channel(
        &self,
        fields: ChannelFields,
        secret: Option<&Secret>,
    ) -> Result<NotificationChannel, StoreError> {
        let id = NotificationChannelId::new();
        let sealed = secret.map(|secret| self.keyring().seal(secret.expose().as_bytes(), id.to_string().as_bytes()));
        let (settings, events) = encode(&fields);
        let row: ChannelRow = sqlx::query_as(concat!(
            "INSERT INTO notification_channels (id, name, kind, settings, events, enabled, secret, data_key_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING ",
            channel_columns!()
        ))
        .bind(id.to_string())
        .bind(&fields.name)
        .bind(fields.settings.kind().as_str())
        .bind(settings)
        .bind(events)
        .bind(fields.enabled)
        .bind(sealed.as_ref().map(|sealed| sealed.ciphertext.clone()))
        .bind(sealed.as_ref().map(|sealed| sealed.key_id.to_string()))
        .fetch_one(self.pool())
        .await
        .map_err(|error| StoreError::on_unique_violation(error, name_taken(&fields.name)))?;
        row.try_into()
    }

    async fn get_notification_channel(&self, id: NotificationChannelId) -> Result<NotificationChannel, StoreError> {
        let row: Option<ChannelRow> =
            sqlx::query_as(concat!("SELECT ", channel_columns!(), " FROM notification_channels WHERE id = ?"))
                .bind(id.to_string())
                .fetch_optional(self.pool())
                .await?;
        row.ok_or_else(|| StoreError::NotFound { entity: CHANNEL, id: id.to_string() })?.try_into()
    }

    async fn list_notification_channels(&self) -> Result<Vec<NotificationChannel>, StoreError> {
        let rows: Vec<ChannelRow> =
            sqlx::query_as(concat!("SELECT ", channel_columns!(), " FROM notification_channels ORDER BY name, id"))
                .fetch_all(self.pool())
                .await?;
        rows.into_iter().map(NotificationChannel::try_from).collect()
    }

    async fn update_notification_channel(
        &self,
        id: NotificationChannelId,
        revision: i64,
        fields: ChannelFields,
        secret: SecretChange<'_>,
    ) -> Result<NotificationChannel, StoreError> {
        let (settings, events) = encode(&fields);
        let (change, sealed) = match secret {
            SecretChange::Keep => (0, None),
            SecretChange::Set(secret) => (1, Some(self.keyring().seal(secret.expose().as_bytes(), id.to_string().as_bytes()))),
            SecretChange::Clear => (1, None),
        };
        let row: Option<ChannelRow> = sqlx::query_as(concat!(
            "UPDATE notification_channels
             SET name = ?, kind = ?, settings = ?, events = ?, enabled = ?,
                 secret = CASE WHEN ? THEN ? ELSE secret END,
                 data_key_id = CASE WHEN ? THEN ? ELSE data_key_id END,
                 revision = revision + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ? AND revision = ? RETURNING ",
            channel_columns!()
        ))
        .bind(&fields.name)
        .bind(fields.settings.kind().as_str())
        .bind(settings)
        .bind(events)
        .bind(fields.enabled)
        .bind(change)
        .bind(sealed.as_ref().map(|sealed| sealed.ciphertext.clone()))
        .bind(change)
        .bind(sealed.as_ref().map(|sealed| sealed.key_id.to_string()))
        .bind(id.to_string())
        .bind(revision)
        .fetch_optional(self.pool())
        .await
        .map_err(|error| StoreError::on_unique_violation(error, name_taken(&fields.name)))?;
        match row {
            Some(row) => row.try_into(),
            None => {
                let current = self.get_notification_channel(id).await?;
                Err(StoreError::RevisionMismatch {
                    entity: CHANNEL,
                    id: id.to_string(),
                    expected: revision,
                    actual: current.revision,
                })
            }
        }
    }

    async fn delete_notification_channel(&self, id: NotificationChannelId) -> Result<(), StoreError> {
        let result =
            sqlx::query("DELETE FROM notification_channels WHERE id = ?").bind(id.to_string()).execute(self.pool()).await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound { entity: CHANNEL, id: id.to_string() });
        }
        Ok(())
    }

    async fn notification_channel_secret(&self, id: NotificationChannelId) -> Result<Option<Secret>, StoreError> {
        if !self.get_notification_channel(id).await?.has_secret {
            return Ok(None);
        }
        let query = "SELECT secret, data_key_id FROM notification_channels WHERE id = ? AND secret IS NOT NULL";
        open_sealed(self, query, CHANNEL, id.to_string()).await.map(Some)
    }

    async fn queue_notifications(
        &self,
        message: &Message,
        url: Option<&str>,
        channels: &[NotificationChannelId],
    ) -> Result<Vec<Notification>, StoreError> {
        let details = serde_json::to_string(&message.details).expect("details serialize");
        let now = format_timestamp(Timestamp::now());
        let mut tx = self.begin_write().await?;
        let mut queued = Vec::with_capacity(channels.len());
        for channel in channels {
            let row: NotificationRow = sqlx::query_as(concat!(
                "INSERT INTO notifications (id, channel_id, event, title, text, url, details, status, next_attempt_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, 'queued', ?) RETURNING ",
                notification_columns!()
            ))
            .bind(NotificationId::new().to_string())
            .bind(channel.to_string())
            .bind(message.event.as_str())
            .bind(&message.title)
            .bind(&message.text)
            .bind(url)
            .bind(&details)
            .bind(&now)
            .fetch_one(&mut *tx)
            .await?;
            queued.push(row.try_into()?);
        }
        tx.commit().await?;
        Ok(queued)
    }

    async fn due_notifications(&self, now: Timestamp, limit: u32) -> Result<Vec<Notification>, StoreError> {
        let rows: Vec<NotificationRow> = sqlx::query_as(concat!(
            "SELECT ",
            notification_columns!(),
            " FROM notifications WHERE status = 'queued' AND next_attempt_at <= ? ORDER BY next_attempt_at, id LIMIT ?"
        ))
        .bind(format_timestamp(now))
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(Notification::try_from).collect()
    }

    async fn record_notification_attempt(&self, id: NotificationId, attempt: Attempt) -> Result<Notification, StoreError> {
        let (status, error, retry_at) = match &attempt {
            Attempt::Sent => ("sent", None, None),
            Attempt::Failed { error, retry_at: Some(at) } => ("queued", Some(error.as_str()), Some(format_timestamp(*at))),
            Attempt::Failed { error, retry_at: None } => ("failed", Some(error.as_str()), None),
        };
        let row: Option<NotificationRow> = sqlx::query_as(concat!(
            "UPDATE notifications
             SET status = ?, error = ?, next_attempt_at = ?, attempts = attempts + 1,
                 sent_at = CASE WHEN ? = 'sent' THEN strftime('%Y-%m-%dT%H:%M:%fZ', 'now') END
             WHERE id = ? AND status = 'queued' RETURNING ",
            notification_columns!()
        ))
        .bind(status)
        .bind(error)
        .bind(retry_at)
        .bind(status)
        .bind(id.to_string())
        .fetch_optional(self.pool())
        .await?;
        row.ok_or_else(|| StoreError::NotFound { entity: NOTIFICATION, id: id.to_string() })?.try_into()
    }

    async fn list_notifications(
        &self,
        channel_id: NotificationChannelId,
        after: Option<NotificationId>,
        limit: u32,
    ) -> Result<Vec<Notification>, StoreError> {
        let after = after.map(|id| id.to_string());
        let rows: Vec<NotificationRow> = sqlx::query_as(concat!(
            "SELECT ",
            notification_columns!(),
            " FROM notifications WHERE channel_id = ? AND (? IS NULL OR id < ?) ORDER BY id DESC LIMIT ?"
        ))
        .bind(channel_id.to_string())
        .bind(&after)
        .bind(&after)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(Notification::try_from).collect()
    }

    async fn prune_notifications(&self, before: Timestamp) -> Result<u64, StoreError> {
        let result = sqlx::query("DELETE FROM notifications WHERE status <> 'queued' AND created_at < ?")
            .bind(format_timestamp(before))
            .execute(self.pool())
            .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use spinikus_core::notification::{NotificationStatus, SmtpSecurity};

    use super::*;
    use crate::test_support::store;

    fn slack(name: &str) -> ChannelFields {
        ChannelFields { name: name.to_string(), settings: ChannelSettings::Slack {}, events: Vec::new(), enabled: true }
    }

    fn message() -> Message {
        Message {
            event: NotificationEvent::DeploymentFailed,
            title: "web failed".to_string(),
            text: "build_failed".to_string(),
            path: Some("/deployments/dep_x".to_string()),
            details: BTreeMap::from([("service_id".to_string(), "svc_x".to_string())]),
        }
    }

    #[tokio::test]
    async fn channels_keep_their_secret_sealed_until_changed() {
        let (_dir, store) = store().await;
        let url = Secret::new("https://hooks.slack.com/services/T/B/x");
        let channel = store.create_notification_channel(slack("Ops"), Some(&url)).await.unwrap();
        let taken = store.create_notification_channel(slack("Ops"), None).await;
        let email = ChannelFields {
            name: "Mail".to_string(),
            settings: ChannelSettings::Email {
                host: "smtp.example.com".to_string(),
                port: 587,
                security: SmtpSecurity::Starttls,
                username: None,
                from: "a@example.com".to_string(),
                to: vec!["b@example.com".to_string()],
            },
            events: vec![NotificationEvent::BackupFailed],
            enabled: false,
        };
        let without = store.create_notification_channel(email.clone(), None).await.unwrap();

        let renamed = ChannelFields { name: "Team".to_string(), ..slack("") };
        let kept = store.update_notification_channel(channel.id, 1, renamed.clone(), SecretChange::Keep).await.unwrap();
        let kept_secret = store.notification_channel_secret(channel.id).await.unwrap();
        let stale = store.update_notification_channel(channel.id, 1, renamed.clone(), SecretChange::Keep).await;
        let cleared = store.update_notification_channel(channel.id, 2, renamed, SecretChange::Clear).await.unwrap();

        assert!(channel.has_secret);
        assert!(matches!(taken, Err(StoreError::Conflict { field: "name", .. })), "{taken:?}");
        assert_eq!((without.fields.clone(), without.has_secret), (email, false));
        assert_eq!(store.notification_channel_secret(without.id).await.unwrap().map(|s| s.expose().to_string()), None);
        assert_eq!((kept.fields.name.as_str(), kept.has_secret, kept.revision), ("Team", true, 2));
        assert_eq!(kept_secret.unwrap().expose(), url.expose());
        assert!(matches!(stale, Err(StoreError::RevisionMismatch { .. })), "{stale:?}");
        assert!(!cleared.has_secret);
        let names: Vec<_> = store.list_notification_channels().await.unwrap().into_iter().map(|c| c.fields.name).collect();
        assert_eq!(names, ["Mail", "Team"]);
    }

    #[tokio::test]
    async fn notifications_are_retried_then_sent_or_given_up_on() {
        let (_dir, store) = store().await;
        let first = store.create_notification_channel(slack("One"), None).await.unwrap().id;
        let second = store.create_notification_channel(slack("Two"), None).await.unwrap().id;

        let queued = store.queue_notifications(&message(), Some("https://x/deployments/dep_x"), &[first, second]).await.unwrap();
        let now = Timestamp::now() + jiff::SignedDuration::from_secs(1);
        let due = store.due_notifications(now, 10).await.unwrap();
        let later = (now + jiff::SignedDuration::from_mins(5)).round(jiff::Unit::Millisecond).unwrap();
        let retried = store
            .record_notification_attempt(queued[0].id, Attempt::Failed { error: "503".to_string(), retry_at: Some(later) })
            .await
            .unwrap();
        let sent = store.record_notification_attempt(queued[1].id, Attempt::Sent).await.unwrap();
        let due_after = store.due_notifications(now, 10).await.unwrap();
        let given_up = store
            .record_notification_attempt(queued[0].id, Attempt::Failed { error: "timeout".to_string(), retry_at: None })
            .await
            .unwrap();
        let again = store.record_notification_attempt(queued[0].id, Attempt::Sent).await;

        assert_eq!(due.len(), 2);
        assert_eq!(due[0].details["service_id"], "svc_x");
        assert_eq!((retried.status, retried.attempts, retried.next_attempt_at), (NotificationStatus::Queued, 1, Some(later)));
        assert_eq!((sent.status, sent.error.as_deref()), (NotificationStatus::Sent, None));
        assert!(sent.sent_at.is_some() && sent.next_attempt_at.is_none());
        assert!(due_after.is_empty());
        assert_eq!(
            (given_up.status, given_up.attempts, given_up.error.as_deref()),
            (NotificationStatus::Failed, 2, Some("timeout"))
        );
        assert!(matches!(again, Err(StoreError::NotFound { .. })), "a finished one isn't attempted again");
        assert_eq!(store.list_notifications(first, None, 10).await.unwrap()[0].id, queued[0].id);

        assert_eq!(store.prune_notifications(Timestamp::now() + jiff::SignedDuration::from_secs(1)).await.unwrap(), 2);
        store.queue_notifications(&message(), None, &[first]).await.unwrap();
        store.delete_notification_channel(first).await.unwrap();
        assert!(store.list_notifications(first, None, 10).await.unwrap().is_empty(), "deleted with the channel");
    }
}
