# Troubleshooting

## Where to look

- **A deployment**: its page (the service's **Deployments** tab), or
  `spinikus deployment logs <id>`. Failures end the log with the reason and the error.
- **An app**: the service's **Logs** tab, or `spinikus logs -f`. To look inside its running
  container, open its **Terminal** tab, or run `spinikus shell` (see [Terminal](../apps/terminal.md)).
- **Spinikus itself**: `journalctl -u spinikus -f` on the control plane's server,
  `journalctl -u spinikus-agent -f` on other servers. For more detail, set
  `[logging] level = "debug"` in `/etc/spinikus/spinikus.toml` and restart.
- **Containers**: `docker ps --filter label=spinikus.service` on the server. Each deployment's
  container is named `spinikus-<deployment ID>`.

## The dashboard doesn't load

- Check that Spinikus runs: `systemctl status spinikus`.
- Check that port 8080 (or the dashboard domain's 443) is open in the server's and the cloud
  provider's firewalls.
- If you set `[http] listen` to `127.0.0.1:8080`, the dashboard is reachable only through
  its domain.

## Too many sign-in attempts

After too many wrong passwords, sign-ins from your address wait up to 15 minutes, even with
the right password. Wait, or sign in from elsewhere. An instance admin can reset a
forgotten password in **Settings → Users**. There is no way yet to recover an instance
whose only admin has lost their password, so keep a second instance admin.

## A deployment fails

The reason is on the deployment's page:

| Reason                | Usual causes                                                                 |
| --------------------- | ---------------------------------------------------------------------------- |
| `build_failed`        | The build failed (its output is in the log); the repository or image can't be fetched (wrong URL or branch, deploy key missing at the host, private image); a mistake in `spinikus.json`; Railpack not installed on the server |
| `release_failed`      | The release command exited with an error; its output is in the log          |
| `start_failed`        | The container couldn't start: a wrong start command, or Docker not running  |
| `health_check_failed` | The app didn't answer on its port, or exited. See below                     |
| `timeout`             | A step took too long, such as a very slow build                             |
| `agent_lost`          | The server running it was disconnected for over 2 minutes                   |

### The health check fails

The most common causes:

- **The app listens on `127.0.0.1` or `localhost`.** Inside a container, it must listen on
  `0.0.0.0` (all addresses) to be reachable. Most frameworks have a host setting for this.
- **The port is wrong.** The service's port must be the one the app listens on inside the
  container. Apps that read `PORT` get it set for them.
- **The app crashes on start**, often for a missing variable. Open **Container logs** on the
  deployment's page.
- **The app is slow to start.** Raise the health check's start period.

## A service is marked crashing

A **crashing** badge on a service, in its environment's list and on the project's page,
means its live container keeps failing. It is marked when either is true:

- **It was restarted 3 or more times within 10 minutes.** Docker restarts a container
  whose process exits, waiting a little longer each time.
- **It hasn't been running for over a minute**, or its server doesn't report it at all.

The service's page says which, and since when, with a link to the container's logs, which
usually say why: a missing variable, a database it can't reach, a wrong start command.
`spinikus service show` says the same, and `spinikus logs` prints the logs.

Fix the cause and deploy, or roll back to the previous deployment. The mark goes away once
the container has run for 10 minutes without a restart, or when another deployment goes
live. Stopping the service removes it too. To hear about it as it happens, set up a
[notification channel](notifications.md) for `service.crashing`.

Nothing is done to the container while it crashes, apart from Docker's restarts. A server
whose agent is disconnected keeps its marks as they were until it reconnects. The service's
past crash loops are listed at `GET /api/v1/services/{id}/crash-loops` and kept for 30 days.

## An app's address answers with an error

The proxy answers with a short message:

| Answer                                     | Meaning                                                            |
| ------------------------------------------ | ------------------------------------------------------------------ |
| `404 no app is served at <name>`           | No live app has this hostname on this server: the domain isn't added, the base domain differs, the DNS points at another server, or the service has no live deployment or no port |
| `503 the app is not running`               | The live deployment's container isn't running; start it if it was stopped, else check its logs, and restart or redeploy |
| `502 the app is not reachable`             | The container runs but doesn't accept connections on its port      |

Right after Docker itself restarts, the first request to an app may answer `502` while the
proxy finds the container's new address; the next one works.

## HTTPS doesn't work

The service's **Domains** tab shows **Certificate failing** with the error. Check that:

- an email is set under **Settings → General → HTTPS certificates**;
- the hostname's DNS record points at the server the app runs on (`dig +short <name>`);
- port 80 is open to the internet: Let's Encrypt checks over plain HTTP;
- you haven't hit Let's Encrypt's rate limits, such as by creating and deleting the same
  names repeatedly. While testing, use Let's Encrypt's staging directory
  (`https://acme-staging-v02.api.letsencrypt.org/directory`) under **ACME directory**.

Hostnames that only resolve inside your network, and names like `*.localhost`, never get
certificates.

## Pushes don't deploy

Check **Last delivery** under **Deploy on push** in the service's settings:

- **none yet**: the host isn't sending it. Check the webhook's URL at the host, and that the
  host can reach Spinikus (GitHub.com can't reach a server on a private network).
- **ignored** with a reason: the push was to another branch, changed nothing under the root
  directory, or that commit is already deployed.
- The host's own delivery log (in its webhook settings) shows Spinikus's answer. `401`
  means the secret differs: set it again at the host, or make a new one.

## A server shows disconnected

On that server, `journalctl -u spinikus-agent -f` says why. Check that it reaches the
control plane on port 8443, and that the server wasn't revoked. Agents reconnect by
themselves once they can.

## Getting help

Report bugs at <https://github.com/cfowlerdev/spinikus/issues>, with what you did, what
happened, and the relevant log lines (with secrets removed). Report security problems
privately to the maintainers instead (see [Security](security.md)).
