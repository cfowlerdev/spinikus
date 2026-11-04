# Notifications

Spinikus can tell you when something needs attention: a deployment that failed, a service
whose container keeps crashing, a failed backup, an HTTPS certificate it couldn't get or
that is about to expire, a server whose agent went away. Instance admins set up where these
messages go under **Settings → Notifications**.

## Channels

A channel is one place messages go. Each has a name, the events it gets, and whether it is
on. Add as many as you like: one for the on-call chat, one for an email list, one for your
own tooling.

| Kind         | What you give it                                                    | Secret                         |
| ------------ | ------------------------------------------------------------------- | ------------------------------ |
| **Webhook**  | A URL that gets a JSON `POST` (http or https)                       | A signing secret, optional     |
| **Slack**    | An incoming webhook, from a Slack app's **Incoming Webhooks**       | The webhook URL                |
| **Discord**  | A channel's webhook, from its **Integrations → Webhooks**           | The webhook URL                |
| **Telegram** | A bot (from @BotFather) and the chat ID it posts in; add the bot to the chat first | The bot token   |
| **Email**    | An SMTP server, its encryption, a username if it needs one, the sender, and up to 10 recipients | The password, optional |

Secrets are stored encrypted and never shown again; leave a secret's field empty when
editing to keep it. Changing a channel's kind drops its old secret. Slack and Discord
webhook URLs are treated as secrets because anyone who has one can post to the channel.

Email connects with **STARTTLS** (usually port 587; a server that doesn't offer it is refused), **TLS** from the
start (usually 465), or **None**, for a relay on the same machine or network. SMTP settings
live on each email channel, so different channels can use different servers.

**Send test** sends a message to the channel right away, whether it is on or not, and says
whether it got through, with the server's answer when it didn't.

## Events

| Event                    | When                                                                                   |
| ------------------------ | -------------------------------------------------------------------------------------- |
| `deployment.failed`      | A deployment failed, with its reason (`build_failed`, `health_check_failed`, …)        |
| `service.crashing`       | A live container restarted three times in ten minutes, or hasn't run for a minute      |
| `service.recovered`      | A crashing service's container has run ten minutes without a restart                   |
| `backup.failed`          | A database backup failed, scheduled or not                                             |
| `instance_backup.failed` | A backup of Spinikus itself failed                                                     |
| `certificate.failed`     | Ordering an HTTPS certificate failed (once per run of failures, not on every retry)    |
| `certificate.expiring`   | A certificate in use expires within 14 days (renewal starts well before, so this means renewal keeps failing) |
| `server.disconnected`    | A server's agent has been disconnected for 5 minutes                                    |
| `server.reconnected`     | A server reported disconnected is back                                                  |

A channel gets the events checked on it. With every event checked it also gets events that
later versions add. A channel that is off gets nothing.

Messages link to the page in question (the deployment, the service's backups, …) when the
dashboard has a domain (**Settings → General → Dashboard domain**), since that is the only
address Spinikus knows people reach it at.

## Delivery

Messages are queued when the event happens and sent within a few seconds. A send that
fails (no connection, an answer other than 2xx, an SMTP error) is retried after 30
seconds, 2, 10, 30 and 60 minutes, then given up. A channel's page lists its 20 most recent
messages with their status (queued, retrying, sent, failed) and the last error. Messages are
kept for 30 days.

Requests time out after 15 seconds and don't follow redirects.

## Webhooks

A webhook channel gets a `POST` with a JSON body:

```json
{
  "id": "ntf_01JD…",
  "event": "deployment.failed",
  "title": "A deployment of shop/production/web failed",
  "text": "health_check_failed: …",
  "url": "https://spinikus.example.com/deployments/dep_01JD…",
  "details": { "project_id": "prj_01JD…", "environment_id": "env_01JD…", "service_id": "svc_01JD…" },
  "created_at": "2026-11-04T10:15:00.123Z"
}
```

`url` is `null` without a dashboard domain. The headers say what it is:

- `X-Spinikus-Event`: the event, as in the body.
- `X-Spinikus-Delivery`: the notification's ID, the same on every retry, so a receiver can
  drop duplicates.
- `X-Spinikus-Signature`: with a signing secret, `sha256=` and the hex HMAC-SHA256 of the
  raw body under the secret. Compute it over the bytes received, and compare in constant
  time.

**Send test** posts a `service.recovered` message whose `details` have `"test": "true"`.

## The API

Channels are under `/api/v1/notification-channels` (instance admins): list, create, get,
update with `If-Match`, delete, `POST /notification-channels/{id}/test`, and
`GET /notification-channels/{id}/notifications` for what was sent. A channel's `secret` is
write-only (`has_secret` says whether it has one); on an update, leave it out to keep it,
or send `null` to remove an optional one.
