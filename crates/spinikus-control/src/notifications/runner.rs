//! Sending queued notifications, and noticing servers that stay disconnected. One runner per
//! control plane, woken when something is queued. A failed attempt is tried again after
//! each of `retry_delays` in turn, then given up on. A server counts as disconnected once
//! its agent has been away for `server_down_after`, counted from the runner's start at the
//! earliest (agents need time to reconnect to a control plane that just started); servers
//! that never connected don't count. Which servers were reported down is kept in memory, so
//! a control plane that restarts may report one again.

use std::{
    collections::{BTreeMap, HashSet},
    time::Duration,
};

use jiff::{SignedDuration, Timestamp};
use spinikus_core::{
    id::ServerId,
    notification::{Message, NotificationEvent},
};
use spinikus_store::{Attempt, NotificationRepository, ServerRepository};
use tokio::sync::{Mutex, Notify};

use super::delivery::{Delivery, TELEGRAM_API};
use crate::{Control, ControlError};

/// How many notifications one pass sends at most.
const BATCH: u32 = 50;

#[derive(Debug, Clone)]
pub struct NotificationTimings {
    /// How often to look for notifications to send and servers that went away.
    pub tick: Duration,
    /// The waits before each retry; after the last, it is given up on.
    pub retry_delays: Vec<Duration>,
    /// How long an agent may be away before its server is reported disconnected.
    pub server_down_after: Duration,
    /// How long sent and failed notifications are kept.
    pub keep_for: Duration,
}

impl Default for NotificationTimings {
    fn default() -> Self {
        let minutes = |minutes: u64| Duration::from_secs(minutes * 60);
        Self {
            tick: Duration::from_secs(5),
            retry_delays: vec![Duration::from_secs(30), minutes(2), minutes(10), minutes(30), minutes(60)],
            server_down_after: minutes(5),
            keep_for: minutes(30 * 24 * 60),
        }
    }
}

pub(crate) struct Notifications {
    pub(crate) timings: NotificationTimings,
    pub(crate) delivery: Delivery,
    wake: Notify,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    started: Option<Timestamp>,
    down: HashSet<ServerId>,
    pruned_at: Option<Timestamp>,
}

impl Notifications {
    pub(crate) fn new(timings: NotificationTimings, telegram_api: &str) -> Self {
        Self { timings, delivery: Delivery::new(telegram_api), wake: Notify::new(), state: Mutex::default() }
    }
}

impl Default for Notifications {
    fn default() -> Self {
        Self::new(NotificationTimings::default(), TELEGRAM_API)
    }
}

fn signed(duration: Duration) -> SignedDuration {
    SignedDuration::try_from(duration).unwrap_or(SignedDuration::MAX)
}

impl Control {
    /// Replaces the notification runner's timings, and Telegram's API (such as a test
    /// server's). Call it before the runner starts.
    pub fn with_notifications(mut self, timings: NotificationTimings, telegram_api: &str) -> Self {
        self.notifications = std::sync::Arc::new(Notifications::new(timings, telegram_api));
        self
    }

    /// Sends notifications until `shutdown` resolves.
    pub async fn run_notifications(&self, shutdown: impl Future<Output = ()>) {
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            self.advance_notifications().await;
            tokio::select! {
                () = shutdown.as_mut() => return,
                () = self.notifications.wake.notified() => {}
                () = tokio::time::sleep(self.notifications.timings.tick) => {}
            }
        }
    }

    pub(crate) fn wake_notifications(&self) {
        self.notifications.wake.notify_one();
    }

    /// One pass: notices servers that went away or came back, sends what is due, and
    /// deletes old notifications. Tests call it directly.
    pub async fn advance_notifications(&self) {
        let now = Timestamp::now();
        if let Err(error) = self.watch_servers(now).await {
            tracing::error!(%error, "failed to look at the servers' connections");
        }
        // Now, so what the server watch just queued goes out in this pass.
        if let Err(error) = self.send_due_notifications(Timestamp::now()).await {
            tracing::error!(%error, "failed to send notifications");
        }
        let mut state = self.notifications.state.lock().await;
        if state.pruned_at.is_none_or(|at| now.duration_since(at) >= SignedDuration::from_hours(1)) {
            state.pruned_at = Some(now);
            if let Err(error) = self.store().prune_notifications(now - signed(self.notifications.timings.keep_for)).await {
                tracing::error!(%error, "failed to delete old notifications");
            }
        }
    }

    async fn send_due_notifications(&self, now: Timestamp) -> Result<(), ControlError> {
        let timings = &self.notifications.timings;
        for notification in self.store().due_notifications(now, BATCH).await? {
            let channel = self.store().get_notification_channel(notification.channel_id).await?;
            let secret = self.store().notification_channel_secret(channel.id).await?;
            let attempt = match self.notifications.delivery.deliver(&channel, secret.as_ref(), &notification).await {
                Ok(()) => Attempt::Sent,
                Err(error) => {
                    let delay = timings.retry_delays.get(notification.attempts as usize);
                    tracing::warn!(notification = %notification.id, channel = channel.fields.name, %error, retry_in = ?delay, "a notification was not delivered");
                    Attempt::Failed { error, retry_at: delay.map(|delay| Timestamp::now() + signed(*delay)) }
                }
            };
            self.store().record_notification_attempt(notification.id, attempt).await?;
        }
        Ok(())
    }

    async fn watch_servers(&self, now: Timestamp) -> Result<(), ControlError> {
        let mut state = self.notifications.state.lock().await;
        let started = *state.started.get_or_insert(now);
        let down_after = signed(self.notifications.timings.server_down_after);
        let mut servers = Vec::new();
        loop {
            let page =
                self.store().list_servers(servers.last().map(|server: &spinikus_core::server::Server| server.id), 100).await?;
            let done = page.len() < 100;
            servers.extend(page);
            if done {
                break;
            }
        }
        for server in servers {
            if server.revoked_at.is_some() {
                state.down.remove(&server.id);
                continue;
            }
            let connected = self.agents().is_connected(server.id);
            let details = BTreeMap::from([("server_id".to_string(), server.id.to_string())]);
            let path = Some(format!("/settings/servers/{}", server.id));
            if connected {
                if state.down.remove(&server.id) {
                    self.notify(Message {
                        event: NotificationEvent::ServerReconnected,
                        title: format!("Server {} is connected again", server.name),
                        text: "Its agent reconnected to the control plane.".to_string(),
                        path,
                        details,
                    })
                    .await;
                }
                continue;
            }
            let Some(last_seen) = server.last_seen_at else { continue };
            if state.down.contains(&server.id) || now.duration_since(last_seen.max(started)) < down_after {
                continue;
            }
            state.down.insert(server.id);
            self.notify(Message {
                event: NotificationEvent::ServerDisconnected,
                title: format!("Server {} is disconnected", server.name),
                text: format!(
                    "Its agent was last seen {}. What runs there keeps running, but nothing can be deployed to it, and its \
                     state isn't known.",
                    last_seen.round(jiff::Unit::Second).unwrap_or(last_seen)
                ),
                path,
                details,
            })
            .await;
        }
        Ok(())
    }
}
