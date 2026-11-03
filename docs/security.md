# Security

How Spinikus protects an instance, what the first security review found, and the risks it
accepts for now. Report vulnerabilities privately to the maintainers rather than in an issue.

## Trust model

- **Instance admins** are trusted with everything: servers, users, settings, storage
  destinations and the instance's backups. Settings that make the control plane connect
  somewhere (storage endpoints, the ACME directory) are theirs alone.
- **Project and environment members** get the role they were given (viewer, developer,
  admin) in that scope only. Something a user has no role in answers "not found".
- **Developers run code on your servers.** Builds (Dockerfile or Railpack) and the
  containers they produce run whatever the repository says, with network access. Give the
  developer role only to people you would give a shell on the servers.
- **The control plane commands every agent.** Agents dial out to it over mutual TLS and run
  what it sends, so a compromised control plane is a compromised fleet. An agent can speak
  only for its own server; its identity comes from its client certificate, never from
  message contents.
- **The `spinikus` system user is in the `docker` group**, which is as powerful as root on
  that machine. Running containers takes it.

## What protects what

- **Passwords** are hashed with Argon2id; at least 12 characters. Sign-in takes the same
  time whether or not the account exists, and every failure looks the same.
- **Sign-in throttling**: 5 wrong passwords for one email from one address, or 50 from one
  address for any emails, within 15 minutes, and that address waits out the rest of the
  15 minutes, right password or not (`429`, `Retry-After`). Counting by email and address
  together keeps someone guessing from locking the real user out from elsewhere.
- **Sessions** are random secrets stored only as hashes, in an `HttpOnly`, `SameSite=Lax`
  cookie (`Secure` over HTTPS), for 14 days. Changing a password ends the user's other
  sessions; an admin's reset or disabling a user ends all of them, and a disabled user's API
  tokens stop working.
- **API tokens** are stored as hashes, carry a role and optionally a scope and expiry, and
  are never wider than the token that made them.
- **Forms** need a same-origin request (`Sec-Fetch-Site`, `Origin` against `Host`) and,
  with a session, a CSRF token. The dashboard's CSP allows no inline scripts or styles and no
  framing. Templates escape everything they render.
- **Secrets** (variables marked secret, deploy keys, storage credentials, webhook secrets,
  certificates' keys, the instance backup passphrase) are encrypted with envelope
  encryption under the master key, which lives outside the database, and each is bound to
  its row. The API never returns them, except a reveal of a variable by a scope admin,
  which is audited. Build output is redacted of secret build variables.
- **Webhooks** are checked by signature (HMAC-SHA256 for GitHub and Gitea, GitLab's token
  compared in constant time) before anything is parsed, with a size limit.
- **Agents** enroll with a single-use, one-hour join token (stored hashed) and pin the
  control plane's CA by fingerprint. Revoking a server disconnects its agent and refuses it
  from then on.
- **Containers** publish their ports on `127.0.0.1` only; the proxy is the way in. Volumes
  are named Docker volumes, never host paths. `git` runs with no user or system config and
  only the `https`, `http` and `ssh` protocols.
- **The proxy** times out TLS handshakes after 10 seconds and request headers after 30.
- **Every change** a user makes is in the audit log, with the acting user or token.

## The first review (November 2026)

Areas reviewed: authentication and sessions, CSRF and response headers, access checks on
every resource taken by ID (rollback targets, redirect domains, log deployments, backups
to restore), webhooks, agent enrollment and the protocol, secrets at rest and in logs,
command arguments built from user input (Git URLs, branches, commits, image references,
mount paths), file permissions, the proxy, and the redirect after sign-in.

Fixed:

- **Nothing limited sign-in attempts.** Passwords could be guessed as fast as the server
  hashed them, which also let anyone keep its CPUs busy. Now throttled as above.
- **The proxy had no header timeout**, so a client sending headers slowly could hold
  connections open indefinitely. Now 30 seconds.
- **Repository URLs starting with `-`** were accepted. Git already refuses them as hosts;
  they are now refused when saved too, along with spaces and control characters.
- **Mount paths with `:` or `,`** were accepted and only failed at deployment, since Docker
  reads them as separators. Now refused when saved.
- **A database's data volume could be moved** through the API or CLI (only the dashboard
  left it out), which started the database empty at its next deployment. Moving it is now
  refused for every client.

## Accepted for now

- **The dashboard's own listener** (`[http] listen`, port 8080) has no header timeout.
  Serve the dashboard on its own domain through the proxy (Settings → General) and bind
  `[http] listen` to `127.0.0.1:8080`.
- **No HSTS header.** Sending one from a server whose HTTPS a user may still be setting up
  could lock them out of plain HTTP; it can come as a setting.
- **Containers run with Docker's defaults**: as the image's user (often root inside the
  container), with Docker's default capabilities. Options such as `no-new-privileges`,
  dropped capabilities and resource limits are on the roadmap with service settings.
- **Builds and apps can reach your private network**, including cloud metadata endpoints,
  like any process on the server. Restrict it with the host's firewall if that matters.
- **Runtime logs are not redacted**; an app that prints its secrets shows them to viewers.
- **`X-Forwarded-For` is believed from any process on the same machine**, since that is
  where the proxy runs. Anything else on the host could set the address recorded for
  sessions and the audit log, and so for sign-in throttling.
- **The database file** relies on its directory for privacy (`/var/lib/spinikus` is
  `0750`, owned by `spinikus`). Its secrets are encrypted regardless.
- **Sessions have no idle timeout**, only their 14-day lifetime, and there is no list of
  them to end one by one: changing your password ends your other sessions.
