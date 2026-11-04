//! Channels, queuing and delivery, against a capturing HTTP server and a fake SMTP server.

use std::time::Duration;

use spinikus_core::{
    notification::NotificationStatus,
    server::{AgentInfo, ServerFacts},
};
use spinikus_store::{AuditLogRepository, ServerRepository};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::mpsc,
};

use super::*;
use crate::test_support::{control, user};

/// A request the capturing server got.
#[derive(Debug)]
struct Request {
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap()
    }
}

/// An HTTP server answering every request with `status`, and passing each on.
async fn capture(status: u16) -> (String, mpsc::UnboundedReceiver<Request>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (sender, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let sender = sender.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let path = line.split_whitespace().nth(1).unwrap_or_default().to_string();
                let mut headers = Vec::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    let (name, value) = line.split_once(':').unwrap();
                    headers.push((name.trim().to_string(), value.trim().to_string()));
                }
                let length = headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .map_or(0, |(_, value)| value.parse().unwrap());
                let mut body = vec![0; length];
                reader.read_exact(&mut body).await.unwrap();
                let _ = sender.send(Request { path, headers, body: String::from_utf8(body).unwrap() });
                let answer = format!("HTTP/1.1 {status} Whatever\r\ncontent-length: 4\r\nconnection: close\r\n\r\nnope");
                reader.into_inner().write_all(answer.as_bytes()).await.unwrap();
            });
        }
    });
    (url, receiver)
}

/// An SMTP server that takes every message, passing on each session's transcript.
async fn smtp() -> (u16, mpsc::UnboundedReceiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let sender = sender.clone();
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut lines = BufReader::new(read).lines();
                let mut transcript = String::new();
                write.write_all(b"220 test ESMTP\r\n").await.unwrap();
                let mut in_data = false;
                while let Ok(Some(line)) = lines.next_line().await {
                    transcript.push_str(&line);
                    transcript.push('\n');
                    let answer: &[u8] = if in_data {
                        if line != "." {
                            continue;
                        }
                        in_data = false;
                        b"250 queued\r\n"
                    } else if line.starts_with("EHLO") {
                        b"250-test\r\n250 8BITMIME\r\n"
                    } else if line == "DATA" {
                        in_data = true;
                        b"354 go on\r\n"
                    } else if line == "QUIT" {
                        write.write_all(b"221 bye\r\n").await.unwrap();
                        break;
                    } else {
                        b"250 OK\r\n"
                    };
                    write.write_all(answer).await.unwrap();
                }
                let _ = sender.send(transcript);
            });
        }
    });
    (port, receiver)
}

/// The next thing received, failing the test rather than hanging it.
async fn next<T>(receiver: &mut mpsc::UnboundedReceiver<T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), receiver.recv()).await.expect("received in time").expect("still open")
}

fn webhook(name: &str, url: &str, secret: Option<&str>, events: Vec<NotificationEvent>) -> NewChannel {
    NewChannel {
        name: name.to_string(),
        settings: ChannelSettings::Webhook { url: url.to_string() },
        events,
        enabled: true,
        secret: secret.map(Secret::new),
    }
}

fn message(event: NotificationEvent) -> Message {
    Message {
        event,
        title: "shop/production/web is crashing".to_string(),
        text: "Its container keeps restarting.".to_string(),
        path: Some("/services/svc_x".to_string()),
        details: BTreeMap::from([("service_id".to_string(), "svc_x".to_string())]),
    }
}

fn retry_once() -> NotificationTimings {
    NotificationTimings { retry_delays: vec![Duration::ZERO], server_down_after: Duration::ZERO, ..Default::default() }
}

#[tokio::test]
async fn instance_admins_manage_channels_whose_secrets_are_write_only() {
    let (_dir, control) = control().await;
    let admin = user(&control, "admin@example.com", true).await;
    let someone = user(&control, "someone@example.com", false).await;
    let slack = |secret: Option<&str>| NewChannel {
        name: "Team".to_string(),
        settings: ChannelSettings::Slack {},
        events: vec![NotificationEvent::DeploymentFailed, NotificationEvent::DeploymentFailed],
        enabled: true,
        secret: secret.map(Secret::new),
    };

    let by_someone = control.create_notification_channel(&someone, slack(Some("https://hooks.slack.com/x"))).await;
    let no_url = control.create_notification_channel(&admin, slack(None)).await;
    let plain_http = control.create_notification_channel(&admin, slack(Some("http://hooks.slack.com/x"))).await;
    let channel =
        control.create_notification_channel(&admin, slack(Some("https://hooks.slack.com/services/T/B/x"))).await.unwrap();
    let email = |to: &str| NewChannel {
        name: "Mail".to_string(),
        settings: ChannelSettings::Email {
            host: "smtp.example.com".to_string(),
            port: 587,
            security: SmtpSecurity::Starttls,
            username: Some(" ".to_string()),
            from: "Spinikus <spinikus@example.com>".to_string(),
            to: vec![to.to_string()],
        },
        events: Vec::new(),
        enabled: true,
        secret: None,
    };
    let bad_address = control.create_notification_channel(&admin, email("not an address")).await;
    let mail = control.create_notification_channel(&admin, email("ops@example.com")).await.unwrap();

    // Changing the kind needs the new kind's secret; the old one doesn't carry over.
    let to_telegram =
        ChannelChanges { settings: Some(ChannelSettings::Telegram { chat_id: "-100123".to_string() }), ..Default::default() };
    let without_token = control.update_notification_channel(&admin, channel.id, 1, to_telegram.clone()).await;
    let with_token = ChannelChanges { secret: Some(Some(Secret::new("123:abc"))), ..to_telegram };
    let telegram = control.update_notification_channel(&admin, channel.id, 1, with_token).await.unwrap();
    let renamed = ChannelChanges { name: Some("Pager".to_string()), ..Default::default() };
    let kept = control.update_notification_channel(&admin, channel.id, 2, renamed).await.unwrap();
    control.delete_notification_channel(&admin, mail.id).await.unwrap();

    assert!(matches!(by_someone, Err(ControlError::Forbidden)), "{by_someone:?}");
    assert!(matches!(no_url, Err(ControlError::Invalid { field: "secret", .. })), "{no_url:?}");
    assert!(matches!(plain_http, Err(ControlError::Invalid { field: "secret", .. })), "{plain_http:?}");
    assert_eq!(channel.fields.events, [NotificationEvent::DeploymentFailed], "each event once");
    assert!(channel.has_secret);
    assert!(matches!(bad_address, Err(ControlError::Invalid { field: "settings.to", .. })), "{bad_address:?}");
    let ChannelSettings::Email { username, .. } = &mail.fields.settings else { unreachable!() };
    assert_eq!(*username, None, "a blank username is none");
    assert!(matches!(without_token, Err(ControlError::Invalid { field: "secret", .. })), "{without_token:?}");
    assert_eq!(telegram.fields.settings.kind(), ChannelKind::Telegram);
    assert_eq!(control.store().notification_channel_secret(channel.id).await.unwrap().unwrap().expose(), "123:abc");
    assert!(kept.has_secret && kept.fields.name == "Pager");
    let listed = control.list_notification_channels(&admin).await.unwrap();
    assert_eq!(listed.iter().map(|channel| channel.id).collect::<Vec<_>>(), [channel.id]);
    assert!(!serde_json::to_string(&listed).unwrap().contains("123:abc"));
    let audit = control.store().list_audit_entries(None, None, None, 10).await.unwrap();
    let actions: Vec<_> = audit.iter().map(|entry| entry.action.as_str()).collect();
    assert_eq!(actions[..2], ["notification_channel.delete", "notification_channel.update"]);
    assert_eq!(audit[1].details["changed"], json!(["name"]));
}

#[tokio::test]
async fn queues_for_the_channels_that_want_it_and_signs_webhooks() {
    let (_dir, control) = control().await;
    let control = control.with_notifications(retry_once(), delivery::TELEGRAM_API);
    let admin = user(&control, "admin@example.com", true).await;
    let (url, mut requests) = capture(200).await;
    let signed = control
        .create_notification_channel(&admin, webhook("Signed", &url, Some("s3cret"), vec![NotificationEvent::ServiceCrashing]))
        .await
        .unwrap();
    control
        .create_notification_channel(&admin, webhook("Backups only", &url, None, vec![NotificationEvent::BackupFailed]))
        .await
        .unwrap();
    let off = NewChannel { enabled: false, ..webhook("Off", &url, None, Vec::new()) };
    control.create_notification_channel(&admin, off).await.unwrap();

    control.notify(message(NotificationEvent::ServiceCrashing)).await;
    control.advance_notifications().await;
    let request = next(&mut requests).await;
    let sent = control.list_notifications(&admin, signed.id, None, 10).await.unwrap().items;

    let body = request.json();
    assert_eq!(request.path, "/");
    assert_eq!(body["event"], "service.crashing");
    assert_eq!(body["title"], "shop/production/web is crashing");
    assert_eq!(body["details"]["service_id"], "svc_x");
    assert_eq!(body["url"], serde_json::Value::Null, "no dashboard domain, so no link");
    assert_eq!(request.header("x-spinikus-event"), Some("service.crashing"));
    assert_eq!(request.header("x-spinikus-delivery"), Some(sent[0].id.to_string().as_str()));
    let expected = format!("sha256={}", delivery::signature(&Secret::new("s3cret"), request.body.as_bytes()));
    assert_eq!(request.header("x-spinikus-signature"), Some(expected.as_str()));
    assert!(requests.try_recv().is_err(), "only the channel that wants it, and is on");
    assert_eq!((sent.len(), sent[0].status, sent[0].attempts), (1, NotificationStatus::Sent, 1));
}

#[tokio::test]
async fn retries_then_gives_up() {
    let (_dir, control) = control().await;
    let control = control.with_notifications(retry_once(), delivery::TELEGRAM_API);
    let admin = user(&control, "admin@example.com", true).await;
    let (url, mut requests) = capture(503).await;
    let channel = control.create_notification_channel(&admin, webhook("Down", &url, None, Vec::new())).await.unwrap();

    control.notify(message(NotificationEvent::DeploymentFailed)).await;
    control.advance_notifications().await;
    let retried = control.list_notifications(&admin, channel.id, None, 10).await.unwrap().items.remove(0);
    tokio::time::sleep(Duration::from_millis(5)).await;
    control.advance_notifications().await;
    let given_up = control.list_notifications(&admin, channel.id, None, 10).await.unwrap().items.remove(0);
    control.advance_notifications().await;

    assert_eq!((retried.status, retried.attempts), (NotificationStatus::Queued, 1));
    assert_eq!(retried.error.as_deref(), Some("answered 503 Service Unavailable: nope"));
    assert_eq!((given_up.status, given_up.attempts, given_up.next_attempt_at), (NotificationStatus::Failed, 2, None));
    for _ in 0..2 {
        next(&mut requests).await;
    }
    assert!(requests.try_recv().is_err(), "not tried again");
}

#[tokio::test]
async fn sends_to_slack_discord_and_telegram_without_leaking_their_secrets() {
    let (url, mut requests) = capture(200).await;
    let (_dir, control) = control().await;
    let control = control.with_notifications(NotificationTimings::default(), &url);
    let notification = |event| Notification {
        id: NotificationId::new(),
        channel_id: NotificationChannelId::new(),
        event,
        title: "a <b> & c".to_string(),
        text: "line".to_string(),
        url: Some("https://spinikus.example.com/services/svc_x".to_string()),
        details: BTreeMap::new(),
        status: NotificationStatus::Queued,
        attempts: 0,
        error: None,
        created_at: jiff::Timestamp::now(),
        next_attempt_at: None,
        sent_at: None,
    };
    let channel = |settings| NotificationChannel {
        id: NotificationChannelId::new(),
        fields: ChannelFields { name: "x".to_string(), settings, events: Vec::new(), enabled: true },
        has_secret: true,
        revision: 1,
        created_at: jiff::Timestamp::now(),
        updated_at: jiff::Timestamp::now(),
    };
    let delivery = &control.notifications.delivery;
    let secret_url = Secret::new(format!("{url}/hooks/T/B/token"));

    delivery
        .deliver(&channel(ChannelSettings::Slack {}), Some(&secret_url), &notification(NotificationEvent::DeploymentFailed))
        .await
        .unwrap();
    let slack = next(&mut requests).await;
    delivery
        .deliver(&channel(ChannelSettings::Discord {}), Some(&secret_url), &notification(NotificationEvent::ServiceRecovered))
        .await
        .unwrap();
    let discord = next(&mut requests).await;
    let telegram = channel(ChannelSettings::Telegram { chat_id: "-100".to_string() });
    delivery
        .deliver(&telegram, Some(&Secret::new("123:token")), &notification(NotificationEvent::DeploymentFailed))
        .await
        .unwrap();
    let to_telegram = next(&mut requests).await;
    let unreachable = delivery::Delivery::new("http://127.0.0.1:1");
    let refused = unreachable
        .deliver(&telegram, Some(&Secret::new("123:hidden")), &notification(NotificationEvent::DeploymentFailed))
        .await;

    assert_eq!(slack.path, "/hooks/T/B/token");
    assert_eq!(
        slack.json()["text"],
        "*a &lt;b&gt; &amp; c*\nline\n<https://spinikus.example.com/services/svc_x|Open in Spinikus>"
    );
    let embed = &discord.json()["embeds"][0];
    assert_eq!((embed["title"].as_str(), embed["color"].as_u64()), (Some("a <b> & c"), Some(0x16a34a)));
    assert_eq!(discord.json()["allowed_mentions"]["parse"], json!([]));
    assert_eq!(to_telegram.path, "/bot123:token/sendMessage");
    assert_eq!(to_telegram.json()["chat_id"], "-100");
    assert_eq!(to_telegram.json()["text"], "a <b> & c\n\nline\n\nhttps://spinikus.example.com/services/svc_x");
    let error = refused.unwrap_err();
    assert!(!error.contains("hidden") && !error.contains("127.0.0.1:1"), "{error}");
}

#[tokio::test]
async fn emails_through_smtp_and_tests_a_channel() {
    let (port, mut sessions) = smtp().await;
    let (_dir, control) = control().await;
    let admin = user(&control, "admin@example.com", true).await;
    let new = NewChannel {
        name: "Mail".to_string(),
        settings: ChannelSettings::Email {
            host: "127.0.0.1".to_string(),
            port,
            security: SmtpSecurity::None,
            username: None,
            from: "Spinikus <spinikus@example.com>".to_string(),
            to: vec!["ops@example.com".to_string(), "dev@example.com".to_string()],
        },
        events: Vec::new(),
        enabled: false,
        secret: None,
    };
    let channel = control.create_notification_channel(&admin, new).await.unwrap();
    let (url, _requests) = capture(500).await;
    let broken = control.create_notification_channel(&admin, webhook("Broken", &url, None, Vec::new())).await.unwrap();

    control.test_notification_channel(&admin, channel.id).await.unwrap();
    let session = next(&mut sessions).await;
    let failed = control.test_notification_channel(&admin, broken.id).await;

    assert!(session.contains("MAIL FROM:<spinikus@example.com>"), "{session}");
    assert!(session.contains("RCPT TO:<ops@example.com>") && session.contains("RCPT TO:<dev@example.com>"), "{session}");
    assert!(session.contains("Subject: [Spinikus] A test from Spinikus"), "{session}");
    assert!(session.contains("Notifications from Spinikus reach the channel `Mail`."), "{session}");
    match failed {
        Err(ControlError::Rejected { problem, .. }) => {
            assert_eq!(problem, "the test was not delivered: answered 500 Internal Server Error: nope")
        }
        other => panic!("{other:?}"),
    }
    assert!(control.list_notifications(&admin, channel.id, None, 10).await.unwrap().items.is_empty(), "tests aren't kept");
}

#[tokio::test]
async fn reports_servers_that_stay_disconnected_and_come_back() {
    let (_dir, control) = control().await;
    let control = control.with_notifications(retry_once(), delivery::TELEGRAM_API);
    let admin = user(&control, "admin@example.com", true).await;
    let (url, mut requests) = capture(200).await;
    control.create_notification_channel(&admin, webhook("Ops", &url, None, Vec::new())).await.unwrap();
    let server = control.create_server(&admin, crate::NewServer { name: "web-1".to_string() }).await.unwrap();
    let never = control.create_server(&admin, crate::NewServer { name: "web-2".to_string() }).await.unwrap();
    let facts = ServerFacts {
        hostname: "web-1".to_string(),
        os: "linux".to_string(),
        arch: "x86_64".to_string(),
        cpu_count: 1,
        memory_bytes: 1,
    };
    let info = AgentInfo { agent_version: "0".to_string(), protocol_version: 1, facts };
    control.store().record_agent_hello(server.id, info).await.unwrap();

    control.advance_notifications().await;
    let down = next(&mut requests).await.json();
    control.advance_notifications().await;
    let _agent = crate::deployments::tests::connect(&control, server.id);
    control.advance_notifications().await;
    let back = next(&mut requests).await.json();

    assert_eq!(
        (down["event"].as_str(), down["title"].as_str()),
        (Some("server.disconnected"), Some("Server web-1 is disconnected"))
    );
    assert_eq!(back["event"], "server.reconnected");
    assert!(requests.try_recv().is_err(), "once each, and nothing for a server that never connected ({})", never.id);
}

#[tokio::test]
async fn a_failed_deployment_is_reported_with_its_service() {
    use spinikus_proto::agent::v1::{self as pb, command_result::Outcome, control_message};

    let setup = crate::deployments::tests::setup(crate::DeploymentTimings::default()).await;
    let control = &setup.control;
    let mut agent = crate::deployments::tests::connect(control, setup.server.id);
    let (url, _requests) = capture(200).await;
    let channel = control.create_notification_channel(&setup.admin, webhook("Ops", &url, None, Vec::new())).await.unwrap();

    let deployment = control.create_deployment(&setup.admin, setup.service.id).await.unwrap();
    control.advance_deployments().await;
    let prepare = loop {
        let message = agent.try_recv().expect("a prepare command");
        if let Some(control_message::Message::Command(command)) = message.message {
            break command;
        }
    };
    let failed = pb::CommandFailed { reason: pb::FailureReason::Internal.into(), message: "cannot pull".to_string() };
    let result = pb::CommandResult { command_id: prepare.command_id, outcome: Some(Outcome::Failed(failed)) };
    control.deployment_result(setup.server.id, result).await;
    let queued = control.list_notifications(&setup.admin, channel.id, None, 10).await.unwrap().items;

    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].event, NotificationEvent::DeploymentFailed);
    assert_eq!(queued[0].title, "A deployment of shop/production/web failed");
    assert!(queued[0].text.starts_with("build_failed: ") && queued[0].text.contains("cannot pull"), "{}", queued[0].text);
    assert_eq!(queued[0].details["deployment_id"], deployment.id.to_string());
    assert_eq!(queued[0].details["environment_id"], setup.environment.id.to_string());
}
