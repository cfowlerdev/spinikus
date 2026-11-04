//! Notification channels, managed by instance admins in the instance settings: where messages
//! about failed deployments, crashing services, failed backups, certificates and servers go.
//! Each channel's secret (a webhook URL, a bot token, a password or a signing secret) is
//! write-only: never shown, kept when left empty.

use axum::{
    Form,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use spinikus_control::{
    Control, ControlError,
    notifications::{ChannelChanges, NewChannel},
};
use spinikus_core::{
    notification::{
        ChannelKind, ChannelSettings, Notification, NotificationChannel, NotificationEvent, NotificationStatus, SecretUse,
        SmtpSecurity,
    },
    secret::Secret,
};

use super::{
    parse_id, problem, problem_message, redirect,
    settings::{SettingsTab, require_admin, tabs},
};
use crate::{
    auth::Signed,
    error::WebError,
    forms::{self, Input, Notice},
    layout::{self, BADGE, BUTTON_SECONDARY, CARD, LABEL, LINK, csrf_field},
};

const URL: &str = "/settings/notifications";
/// How many of a channel's notifications its page lists.
const RECENT: u32 = 20;

/// The form as typed. Read from the posted pairs, since the event checkboxes repeat a name.
#[derive(Default)]
pub struct ChannelForm {
    revision: Option<i64>,
    name: String,
    kind: String,
    url: String,
    chat_id: String,
    host: String,
    port: String,
    security: String,
    username: String,
    from: String,
    to: String,
    /// The secret typed for each kind, so switching kinds before saving loses nothing.
    signing_secret: String,
    slack_url: String,
    discord_url: String,
    bot_token: String,
    password: String,
    remove_secret: bool,
    events: Vec<String>,
    enabled: bool,
}

impl ChannelForm {
    /// A new channel's form: a webhook, every event, on.
    fn blank() -> Self {
        Self {
            kind: ChannelKind::Webhook.as_str().to_string(),
            security: SmtpSecurity::Starttls.as_str().to_string(),
            port: "587".to_string(),
            events: NotificationEvent::ALL.iter().map(|event| event.as_str().to_string()).collect(),
            enabled: true,
            ..Self::default()
        }
    }

    fn parse(pairs: Vec<(String, String)>) -> Self {
        let mut form = Self::default();
        for (key, value) in pairs {
            match key.as_str() {
                "revision" => form.revision = value.trim().parse().ok(),
                "name" => form.name = value,
                "kind" => form.kind = value,
                "url" => form.url = value,
                "chat_id" => form.chat_id = value,
                "host" => form.host = value,
                "port" => form.port = value,
                "security" => form.security = value,
                "username" => form.username = value,
                "from" => form.from = value,
                "to" => form.to = forms::lines(&value),
                "signing_secret" => form.signing_secret = value,
                "slack_url" => form.slack_url = value,
                "discord_url" => form.discord_url = value,
                "bot_token" => form.bot_token = value,
                "password" => form.password = value,
                "remove_secret" => form.remove_secret = true,
                "events" => form.events.push(value),
                "enabled" => form.enabled = true,
                _ => {}
            }
        }
        form
    }

    fn of(channel: &NotificationChannel) -> Self {
        let fields = &channel.fields;
        let events = match fields.events.is_empty() {
            true => NotificationEvent::ALL.to_vec(),
            false => fields.events.clone(),
        };
        let mut form = Self {
            revision: Some(channel.revision),
            name: fields.name.clone(),
            kind: fields.settings.kind().as_str().to_string(),
            events: events.iter().map(|event| event.as_str().to_string()).collect(),
            enabled: fields.enabled,
            ..Self::blank()
        };
        match &fields.settings {
            ChannelSettings::Webhook { url } => form.url = url.clone(),
            ChannelSettings::Slack {} | ChannelSettings::Discord {} => {}
            ChannelSettings::Telegram { chat_id } => form.chat_id = chat_id.clone(),
            ChannelSettings::Email { host, port, security, username, from, to } => {
                form.host = host.clone();
                form.port = port.to_string();
                form.security = security.as_str().to_string();
                form.username = username.clone().unwrap_or_default();
                form.from = from.clone();
                form.to = to.join("\n");
            }
        }
        form
    }

    fn kind(&self) -> Result<ChannelKind, String> {
        self.kind.parse().map_err(|_| "Choose where notifications go.".to_string())
    }

    fn settings(&self) -> Result<ChannelSettings, String> {
        Ok(match self.kind()? {
            ChannelKind::Webhook => ChannelSettings::Webhook { url: self.url.trim().to_string() },
            ChannelKind::Slack => ChannelSettings::Slack {},
            ChannelKind::Discord => ChannelSettings::Discord {},
            ChannelKind::Telegram => ChannelSettings::Telegram { chat_id: self.chat_id.trim().to_string() },
            ChannelKind::Email => ChannelSettings::Email {
                host: self.host.trim().to_string(),
                port: self.port.trim().parse().map_err(|_| "The port must be a number from 1 to 65535.".to_string())?,
                security: self.security.parse().map_err(|_| "Choose how to connect to the SMTP server.".to_string())?,
                username: Some(self.username.trim().to_string()).filter(|name| !name.is_empty()),
                from: self.from.trim().to_string(),
                to: self.to.split([',', '\n']).map(str::trim).filter(|to| !to.is_empty()).map(str::to_string).collect(),
            },
        })
    }

    /// The events checked; all of them is every event, including ones added later.
    fn events(&self) -> Result<Vec<NotificationEvent>, String> {
        let events: Vec<NotificationEvent> = self.events.iter().filter_map(|event| event.parse().ok()).collect();
        if events.is_empty() {
            return Err("Choose at least one event, or turn the channel off.".to_string());
        }
        match NotificationEvent::ALL.iter().all(|event| events.contains(event)) {
            true => Ok(Vec::new()),
            false => Ok(events),
        }
    }

    /// The secret typed for the chosen kind, if any.
    fn secret(&self) -> Option<Secret> {
        let typed = match self.kind().ok()? {
            ChannelKind::Webhook => &self.signing_secret,
            ChannelKind::Slack => &self.slack_url,
            ChannelKind::Discord => &self.discord_url,
            ChannelKind::Telegram => &self.bot_token,
            ChannelKind::Email => &self.password,
        };
        Some(typed.trim()).filter(|typed| !typed.is_empty()).map(Secret::new)
    }

    fn new_channel(&self) -> Result<NewChannel, String> {
        Ok(NewChannel {
            name: self.name.trim().to_string(),
            settings: self.settings()?,
            events: self.events()?,
            enabled: self.enabled,
            secret: self.secret(),
        })
    }

    fn changes(&self) -> Result<ChannelChanges, String> {
        let secret = match (self.secret(), self.remove_secret) {
            (Some(secret), _) => Some(Some(secret)),
            (None, true) => Some(None),
            (None, false) => None,
        };
        Ok(ChannelChanges {
            name: Some(self.name.trim().to_string()),
            settings: Some(self.settings()?),
            events: Some(self.events()?),
            enabled: Some(self.enabled),
            secret,
        })
    }

    /// The fields. The kind's radio buttons show its fields with CSS alone (the ids are fixed,
    /// so a page has one channel form). `editing` is the channel being edited, whose secret is
    /// kept when its field is left empty.
    fn fields(&self, editing: Option<&NotificationChannel>) -> Markup {
        let kept = editing.filter(|channel| channel.has_secret).map(|channel| channel.fields.settings.kind());
        let secret = |id: &str, name: &str, label: &str, kind: ChannelKind| {
            let hint = match (kept == Some(kind), kind.secret()) {
                (true, _) => "Leave it empty to keep the current one.",
                (false, SecretUse::Required(_)) => "Kept encrypted, and never shown again.",
                (false, SecretUse::Optional(_)) => "Optional. Kept encrypted, and never shown again.",
            };
            Input::text(id, name, label, "").kind("password").mono().hint(hint).render()
        };
        let removable = kept.filter(|kind| matches!(kind.secret(), SecretUse::Optional(_)));
        let securities =
            [("starttls", "STARTTLS (usually port 587)"), ("tls", "TLS (usually port 465)"), ("none", "None (a local relay)")];
        html! {
            (Input::text("channel-name", "name", "Name", &self.name).required().placeholder("On-call").render())
            div class="group space-y-4" {
                fieldset {
                    legend class=(LABEL) { "Send to" }
                    div class="mt-2 flex flex-wrap gap-x-6 gap-y-2 text-sm" {
                        @for (kind, label) in [(ChannelKind::Webhook, "A webhook"), (ChannelKind::Slack, "Slack"), (ChannelKind::Discord, "Discord"), (ChannelKind::Telegram, "Telegram"), (ChannelKind::Email, "Email")] {
                            label class="flex items-center gap-2" {
                                input id={ "channel-kind-" (kind.as_str()) } type="radio" name="kind" value=(kind.as_str()) checked[self.kind == kind.as_str()];
                                (label)
                            }
                        }
                    }
                }
                div class="hidden space-y-4 group-has-[#channel-kind-webhook:checked]:block" {
                    (Input::text("channel-url", "url", "URL", &self.url).mono().placeholder("https://example.com/hooks/spinikus")
                        .hint("Gets a JSON POST for each notification.").render())
                    (secret("channel-signing-secret", "signing_secret", "Signing secret", ChannelKind::Webhook))
                    p class="text-xs text-zinc-500 dark:text-zinc-400" {
                        "With a signing secret, each request has an " code { "X-Spinikus-Signature" } " header: " code { "sha256=" } " and the HMAC-SHA256 of the body, in hex."
                    }
                }
                div class="hidden space-y-4 group-has-[#channel-kind-slack:checked]:block" {
                    (secret("channel-slack-url", "slack_url", "Incoming webhook URL", ChannelKind::Slack))
                    p class="text-xs text-zinc-500 dark:text-zinc-400" { "From a Slack app's Incoming Webhooks, for the channel to post in." }
                }
                div class="hidden space-y-4 group-has-[#channel-kind-discord:checked]:block" {
                    (secret("channel-discord-url", "discord_url", "Webhook URL", ChannelKind::Discord))
                    p class="text-xs text-zinc-500 dark:text-zinc-400" { "From the Discord channel's settings, under Integrations, Webhooks." }
                }
                div class="hidden space-y-4 group-has-[#channel-kind-telegram:checked]:block" {
                    (secret("channel-bot-token", "bot_token", "Bot token", ChannelKind::Telegram))
                    (Input::text("channel-chat-id", "chat_id", "Chat ID", &self.chat_id).mono().placeholder("-1001234567890")
                        .hint("The chat, group or channel the bot posts in; the bot must be a member.").render())
                }
                div class="hidden space-y-4 group-has-[#channel-kind-email:checked]:block" {
                    div class="grid gap-4 sm:grid-cols-3" {
                        div class="sm:col-span-2" { (Input::text("channel-host", "host", "SMTP server", &self.host).mono().placeholder("smtp.example.com").render()) }
                        (Input::text("channel-port", "port", "Port", &self.port).mono().render())
                    }
                    (forms::select("channel-security", "security", "Encryption", &securities, &self.security))
                    div class="grid gap-4 sm:grid-cols-2" {
                        (Input::text("channel-username", "username", "Username", &self.username).mono().hint("Empty if the server needs no sign-in.").render())
                        (secret("channel-password", "password", "Password", ChannelKind::Email))
                    }
                    (Input::text("channel-from", "from", "From", &self.from).placeholder("Spinikus <spinikus@example.com>").render())
                    (forms::textarea("channel-to", "to", "To", &self.to, Some("One address per line, at most 10."), Some("ops@example.com")))
                }
            }
            @if removable.is_some() {
                (forms::checkbox("channel-remove-secret", "remove_secret", "Remove the secret", self.remove_secret, false,
                    Some("Removes the webhook's signing secret or the SMTP password, if no new one is typed above.")))
            }
            fieldset {
                legend class=(LABEL) { "Events" }
                p class="text-xs text-zinc-500 dark:text-zinc-400" { "With every one checked, the channel also gets events added in later versions." }
                div class="mt-2 grid gap-2 sm:grid-cols-2" {
                    @for event in NotificationEvent::ALL {
                        label class="flex items-center gap-2 text-sm" {
                            input type="checkbox" name="events" value=(event.as_str()) checked[self.events.iter().any(|checked| checked == event.as_str())]
                                class="size-4 rounded border-zinc-300 text-indigo-600 dark:border-zinc-700";
                            (event.label())
                        }
                    }
                }
            }
            (forms::checkbox("channel-enabled", "enabled", "On", self.enabled, false, Some("A channel that is off gets nothing, but can still be tested.")))
            @if let Some(revision) = self.revision { input type="hidden" name="revision" value=(revision); }
        }
    }
}

/// What the list page says above its content.
enum Alert<'a> {
    None,
    Done(&'a str),
    Problem(&'a str),
}

pub async fn notifications(State(control): State<Control>, signed: Signed) -> Result<Markup, WebError> {
    require_admin(&signed)?;
    Ok(list_page(&control, &signed, Alert::None, &ChannelForm::blank(), Notice::None).await?)
}

pub async fn create(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    require_admin(&signed)?;
    let form = ChannelForm::parse(pairs);
    let refused = match form.new_channel() {
        Ok(new) => match control.create_notification_channel(&signed.principal, new).await {
            Ok(_) => return Ok(redirect(&headers, URL)),
            Err(error) => problem_message(error)?,
        },
        Err(message) => message,
    };
    let page = list_page(&control, &signed, Alert::None, &form, Notice::Problem(&refused)).await?;
    Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
}

/// Sends a test message, and says how that went.
pub async fn test(State(control): State<Control>, signed: Signed, Path(id): Path<String>) -> Result<Response, WebError> {
    require_admin(&signed)?;
    let channel = control.get_notification_channel(&signed.principal, parse_id(&id)?).await?;
    let blank = ChannelForm::blank();
    let (status, page) = match control.test_notification_channel(&signed.principal, channel.id).await {
        Ok(()) => {
            let done = format!("{}: sent a test message.", channel.fields.name);
            (StatusCode::OK, list_page(&control, &signed, Alert::Done(&done), &blank, Notice::None).await?)
        }
        Err(error) => {
            let message = format!("{}: {}", channel.fields.name, problem_message(error)?);
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                list_page(&control, &signed, Alert::Problem(&message), &blank, Notice::None).await?,
            )
        }
    };
    // The page is the list's, whatever URL the button posted to.
    Ok((status, [("hx-replace-url", URL)], page).into_response())
}

pub async fn delete(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, WebError> {
    require_admin(&signed)?;
    match control.delete_notification_channel(&signed.principal, parse_id(&id)?).await {
        Ok(()) => Ok(redirect(&headers, URL)),
        Err(error) => {
            let message = problem_message(error)?;
            let page = list_page(&control, &signed, Alert::Problem(&message), &ChannelForm::blank(), Notice::None).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
    }
}

pub async fn edit(State(control): State<Control>, signed: Signed, Path(id): Path<String>) -> Result<Markup, WebError> {
    require_admin(&signed)?;
    let channel = control.get_notification_channel(&signed.principal, parse_id(&id)?).await?;
    let recent = recent(&control, &signed, &channel).await?;
    Ok(edit_page(&signed, &channel, &ChannelForm::of(&channel), Notice::None, &recent))
}

pub async fn update(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    require_admin(&signed)?;
    let channel = control.get_notification_channel(&signed.principal, parse_id(&id)?).await?;
    let form = ChannelForm::parse(pairs);
    let recent = recent(&control, &signed, &channel).await?;
    let changes = match form.changes() {
        Ok(changes) => changes,
        Err(message) => {
            let page = edit_page(&signed, &channel, &form, Notice::Problem(&message), &recent);
            return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
        }
    };
    match control.update_notification_channel(&signed.principal, channel.id, form.revision.unwrap_or(-1), changes).await {
        Ok(_) => Ok(redirect(&headers, URL)),
        Err(error) => problem(error, |message| edit_page(&signed, &channel, &form, Notice::Problem(message), &recent)),
    }
}

async fn recent(control: &Control, signed: &Signed, channel: &NotificationChannel) -> Result<Vec<Notification>, ControlError> {
    Ok(control.list_notifications(&signed.principal, channel.id, None, RECENT).await?.items)
}

async fn list_page(
    control: &Control,
    signed: &Signed,
    alert: Alert<'_>,
    form: &ChannelForm,
    notice: Notice<'_>,
) -> Result<Markup, ControlError> {
    let channels = control.list_notification_channels(&signed.principal).await?;
    let content = html! {
        @match alert {
            Alert::None => {},
            Alert::Done(message) => (layout::notice(message)),
            Alert::Problem(message) => (layout::alert(message)),
        }
        (tabs(signed, SettingsTab::Notifications))
        div class="space-y-8" {
            section {
                h2 class="mb-1 font-semibold" { "Notification channels" }
                p class="mb-3 text-sm text-zinc-500 dark:text-zinc-400" {
                    "Where Spinikus says when something needs attention: a failed deployment or backup, a crashing service, an HTTPS certificate, a disconnected server."
                }
                @if channels.is_empty() {
                    (layout::empty("No notification channels yet."))
                } @else {
                    ul class={ (CARD) " divide-y divide-zinc-200 dark:divide-zinc-800" } {
                        @for channel in &channels { (channel_row(signed, channel)) }
                    }
                }
            }
            div class="max-w-2xl" {
                (forms::card(signed, "Add a channel", None, URL, notice, form.fields(None), "Add channel"))
            }
        }
    };
    Ok(layout::page(signed, "Notifications · Settings", &[], content))
}

fn kind_label(kind: ChannelKind) -> &'static str {
    match kind {
        ChannelKind::Webhook => "Webhook",
        ChannelKind::Slack => "Slack",
        ChannelKind::Discord => "Discord",
        ChannelKind::Telegram => "Telegram",
        ChannelKind::Email => "Email",
    }
}

/// Where the channel sends, without its secret.
fn destination(settings: &ChannelSettings) -> String {
    match settings {
        ChannelSettings::Webhook { url } => url.clone(),
        ChannelSettings::Slack {} | ChannelSettings::Discord {} => "its webhook".to_string(),
        ChannelSettings::Telegram { chat_id } => format!("chat {chat_id}"),
        ChannelSettings::Email { host, to, .. } => format!("{} via {host}", to.join(", ")),
    }
}

fn channel_row(signed: &Signed, channel: &NotificationChannel) -> Markup {
    let fields = &channel.fields;
    let base = format!("{URL}/{}", channel.id);
    let events = match fields.events.len() {
        0 => "every event".to_string(),
        1 => "1 event".to_string(),
        count => format!("{count} events"),
    };
    let off = "bg-zinc-50 text-zinc-600 ring-zinc-500/20 dark:bg-zinc-500/10 dark:text-zinc-400 dark:ring-zinc-500/20";
    html! {
        li class="flex flex-wrap items-center gap-x-4 gap-y-2 px-5 py-3 text-sm" {
            div class="min-w-0" {
                p class="flex items-center gap-2 font-medium" {
                    (fields.name)
                    @if !fields.enabled { span class={ (BADGE) " " (off) } { "off" } }
                }
                p class="text-xs break-all text-zinc-500 dark:text-zinc-400" {
                    (kind_label(fields.settings.kind())) " · " span class="font-mono" { (destination(&fields.settings)) } " · " (events)
                }
            }
            div class="ml-auto flex gap-2" {
                form method="post" action={ (base) "/test" } {
                    (csrf_field(signed))
                    button type="submit" class={ (BUTTON_SECONDARY) " py-1" } { "Send test" }
                }
                a href=(base) class={ (BUTTON_SECONDARY) " py-1" } { "Edit" }
                form method="post" action={ (base) "/delete" } hx-confirm="Delete this channel? Notifications not sent yet are dropped." {
                    (csrf_field(signed))
                    button type="submit" class={ (BUTTON_SECONDARY) " py-1 text-red-600 dark:text-red-400" } { "Delete" }
                }
            }
        }
    }
}

fn edit_page(
    signed: &Signed,
    channel: &NotificationChannel,
    form: &ChannelForm,
    notice: Notice<'_>,
    recent: &[Notification],
) -> Markup {
    let action = format!("{URL}/{}", channel.id);
    let content = html! {
        (tabs(signed, SettingsTab::Notifications))
        div class="max-w-2xl space-y-8" {
            (forms::card(signed, &format!("Edit {}", channel.fields.name), None, &action, notice, form.fields(Some(channel)), "Save"))
            section {
                h2 class="mb-1 font-semibold" { "Recent notifications" }
                p class="mb-3 text-sm text-zinc-500 dark:text-zinc-400" { "The newest first. Failed sends are retried for about two hours, then given up." }
                @if recent.is_empty() {
                    (layout::empty("Nothing sent to this channel yet."))
                } @else {
                    ul class={ (CARD) " divide-y divide-zinc-200 dark:divide-zinc-800" } {
                        @for notification in recent { (notification_row(notification)) }
                    }
                }
            }
            div class="text-sm" { a href=(URL) class=(LINK) { "Back to notification channels" } }
        }
    };
    layout::page(signed, &format!("{} · Settings", channel.fields.name), &[], content)
}

fn notification_row(notification: &Notification) -> Markup {
    let (label, colors) = match notification.status {
        NotificationStatus::Sent => (
            "sent",
            "bg-emerald-50 text-emerald-700 ring-emerald-600/20 dark:bg-emerald-500/10 dark:text-emerald-400 dark:ring-emerald-500/20",
        ),
        NotificationStatus::Queued if notification.attempts > 0 => (
            "retrying",
            "bg-amber-50 text-amber-700 ring-amber-600/20 dark:bg-amber-500/10 dark:text-amber-400 dark:ring-amber-500/20",
        ),
        NotificationStatus::Queued => {
            ("queued", "bg-zinc-50 text-zinc-600 ring-zinc-500/20 dark:bg-zinc-500/10 dark:text-zinc-400 dark:ring-zinc-500/20")
        }
        NotificationStatus::Failed => {
            ("failed", "bg-red-50 text-red-700 ring-red-600/20 dark:bg-red-500/10 dark:text-red-400 dark:ring-red-500/20")
        }
    };
    html! {
        li class="space-y-1 px-5 py-3 text-sm" {
            div class="flex flex-wrap items-center gap-2" {
                span class="font-medium" { (notification.title) }
                span class={ (BADGE) " " (colors) } { (label) }
                span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400" { (layout::time(notification.created_at)) }
            }
            p class="text-xs text-zinc-500 dark:text-zinc-400" {
                (notification.event.label())
                @if notification.attempts > 1 { " · " (notification.attempts) " attempts" }
            }
            @if let Some(error) = &notification.error {
                p class="font-mono text-xs break-all text-red-600 dark:text-red-400" { (error) }
            }
        }
    }
}
