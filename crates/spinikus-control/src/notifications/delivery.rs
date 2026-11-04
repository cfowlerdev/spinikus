//! Sending one notification to one channel. Errors are short and never hold the channel's
//! secret: request errors leave the URL out, since Slack's and Discord's URLs and Telegram's
//! request path carry it.

use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Tokio1Executor,
    message::{Mailbox, header::ContentType},
    transport::smtp::authentication::Credentials,
};
use serde_json::{Value, json};
use sha2::Sha256;
use spinikus_core::{
    notification::{ChannelSettings, Notification, NotificationChannel, SmtpSecurity},
    secret::Secret,
};

/// How long one attempt may take.
pub const DELIVERY_TIMEOUT: Duration = Duration::from_secs(15);

/// Telegram's Bot API.
pub const TELEGRAM_API: &str = "https://api.telegram.org";

/// The most of an answer's body put in an error.
const MAX_ANSWER: usize = 200;

pub(crate) struct Delivery {
    http: reqwest::Client,
    telegram_api: String,
}

impl Delivery {
    pub(crate) fn new(telegram_api: &str) -> Self {
        // reqwest uses rustls without choosing a provider; this is the control plane's.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .timeout(DELIVERY_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("Spinikus/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("the HTTP client builds");
        Self { http, telegram_api: telegram_api.trim_end_matches('/').to_string() }
    }

    /// Sends `notification` to `channel`, saying why not if it couldn't.
    pub(crate) async fn deliver(
        &self,
        channel: &NotificationChannel,
        secret: Option<&Secret>,
        notification: &Notification,
    ) -> Result<(), String> {
        let missing = || "the channel has no secret".to_string();
        match &channel.fields.settings {
            ChannelSettings::Webhook { url } => self.webhook(url, secret, notification).await,
            ChannelSettings::Slack {} => self.post(secret.ok_or_else(missing)?.expose(), &slack(notification), &[]).await,
            ChannelSettings::Discord {} => self.post(secret.ok_or_else(missing)?.expose(), &discord(notification), &[]).await,
            ChannelSettings::Telegram { chat_id } => {
                let token = secret.ok_or_else(missing)?;
                let url = format!("{}/bot{}/sendMessage", self.telegram_api, token.expose());
                let body = json!({ "chat_id": chat_id, "text": plain(notification), "disable_web_page_preview": true });
                self.post(&url, &body, &[]).await
            }
            ChannelSettings::Email { host, port, security, username, from, to } => {
                let server = Smtp { host, port: *port, security: *security, username: username.as_deref(), password: secret };
                email(server, from, to, notification).await
            }
        }
    }

    async fn webhook(&self, url: &str, secret: Option<&Secret>, notification: &Notification) -> Result<(), String> {
        let body = json!({
            "id": notification.id.to_string(),
            "event": notification.event.as_str(),
            "title": notification.title,
            "text": notification.text,
            "url": notification.url,
            "details": notification.details,
            "created_at": notification.created_at.to_string(),
        });
        let mut headers = vec![
            ("x-spinikus-event", notification.event.as_str().to_string()),
            ("x-spinikus-delivery", notification.id.to_string()),
        ];
        if let Some(secret) = secret {
            headers.push(("x-spinikus-signature", format!("sha256={}", signature(secret, body.to_string().as_bytes()))));
        }
        self.post(url, &body, &headers).await
    }

    /// Posts `body` as JSON; any 2xx answer is success.
    async fn post(&self, url: &str, body: &Value, headers: &[(&str, String)]) -> Result<(), String> {
        let body = body.to_string();
        let mut request = self.http.post(url).header(reqwest::header::CONTENT_TYPE, "application/json");
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        let response = request.body(body).send().await.map_err(|error| error.without_url().to_string())?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let answer = response.text().await.unwrap_or_default();
        let answer: String = answer.trim().chars().take(MAX_ANSWER).collect();
        Err(if answer.is_empty() { format!("answered {status}") } else { format!("answered {status}: {answer}") })
    }
}

/// The hex HMAC-SHA256 of `body` with the secret, as in `X-Spinikus-Signature: sha256=…`.
pub fn signature(secret: &Secret, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.expose().as_bytes()).expect("HMAC takes keys of any length");
    mac.update(body);
    mac.finalize().into_bytes().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The title, text and link as plain text.
fn plain(notification: &Notification) -> String {
    let mut text = format!("{}\n\n{}", notification.title, notification.text);
    if let Some(url) = &notification.url {
        text.push_str("\n\n");
        text.push_str(url);
    }
    text
}

/// Slack's `mrkdwn` needs `&`, `<` and `>` escaped.
fn slack(notification: &Notification) -> Value {
    let escape = |text: &str| text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let mut text = format!("*{}*\n{}", escape(&notification.title), escape(&notification.text));
    if let Some(url) = &notification.url {
        text.push_str(&format!("\n<{}|Open in Spinikus>", escape(url)));
    }
    json!({ "text": text })
}

fn discord(notification: &Notification) -> Value {
    let color = if notification.event.is_good_news() { 0x16a34a } else { 0xdc2626 };
    let title: String = notification.title.chars().take(256).collect();
    let description: String = notification.text.chars().take(4096).collect();
    let mut embed = json!({ "title": title, "description": description, "color": color });
    if let Some(url) = &notification.url {
        embed["url"] = json!(url);
    }
    // No pings, whatever the text says.
    json!({ "embeds": [embed], "allowed_mentions": { "parse": [] } })
}

struct Smtp<'a> {
    host: &'a str,
    port: u16,
    security: SmtpSecurity,
    username: Option<&'a str>,
    password: Option<&'a Secret>,
}

async fn email(server: Smtp<'_>, from: &str, to: &[String], notification: &Notification) -> Result<(), String> {
    let parse = |address: &str| address.parse::<Mailbox>().map_err(|error| format!("`{address}`: {error}"));
    let mut message = lettre::Message::builder().from(parse(from)?).subject(format!("[Spinikus] {}", notification.title));
    for address in to {
        message = message.to(parse(address)?);
    }
    let message = message.header(ContentType::TEXT_PLAIN).body(plain(notification)).map_err(|error| error.to_string())?;

    let builder = match server.security {
        SmtpSecurity::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(server.host),
        SmtpSecurity::Starttls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(server.host),
        SmtpSecurity::None => Ok(AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(server.host)),
    };
    let mut builder = builder.map_err(|error| error.to_string())?.port(server.port).timeout(Some(DELIVERY_TIMEOUT));
    if let Some(username) = server.username {
        let password = server.password.map(|password| password.expose().to_string()).unwrap_or_default();
        builder = builder.credentials(Credentials::new(username.to_string(), password));
    }
    let transport = builder.build();
    match tokio::time::timeout(DELIVERY_TIMEOUT, transport.send(message)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("the SMTP server did not answer in time".to_string()),
    }
}
