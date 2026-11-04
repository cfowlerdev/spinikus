# Spinikus Feature List

Spinikus is a Rust-based, self-hosted PaaS, a lighter alternative to Coolify and Dokploy, in the spirit of a self-hosted Vercel. Features are grouped by area and marked **MVP** (needed to be usable), **v1** (needed to compete with Coolify and Dokploy) or **Later** (differentiators and polish).

## Deployment core

- **Git-based deploys** (MVP): GitHub, GitLab, Gitea/Forgejo and generic Git over SSH, with auto-deploy on push via webhooks.
- **Build strategies** (MVP): Dockerfile, Docker Compose, and auto-detected builds via Railpack or Cloud Native Buildpacks.
- **Prebuilt image deploys** (MVP): from Docker Hub, GHCR or private registries.
- **Zero-downtime deploys and instant rollback** (v1): health-check-gated cutover, with old releases kept around.
- **Preview deployments per PR** (v1): ephemeral URLs and automatic cleanup. This is the most "Vercel" feature.
- **Build caching and a build queue** (v1): layer caching, concurrent build limits, cancelable builds.
- **Monorepo support** (v1): multiple apps per repo, with path-based build triggers.

## Networking and routing

- **Built-in reverse proxy with automatic HTTPS** (MVP): Let's Encrypt, wildcard certs via DNS-01, custom domains. A Rust-native proxy (Pingora, or hyper/axum-based) rather than shelling out to Traefik would be a real differentiator.
- **Per-app and wildcard domains** (MVP), with HTTP to HTTPS redirects.
- **Path routing, basic auth, rate limiting, IP allowlists** (v1).
- **Tunnels / private networking** (Later): WireGuard mesh between servers, with optional Cloudflare Tunnel integration.

## Configuration and secrets

- **Environment variables** (MVP): per environment (production, staging, preview), with build-time versus runtime separation.
- **Encrypted secrets at rest** (MVP).
- **Shared variables across a project** (v1), plus `.env` import and export.
- **External secret managers** (Later): Vault, Infisical, 1Password.

## Databases and services

- **One-click databases** (MVP): Postgres, MySQL/MariaDB, Redis/Valkey, MongoDB.
- **Volume management and persistent storage** (MVP).
- **Scheduled backups to S3-compatible storage** (v1), with one-click restore and retention policies.
- **One-click service templates** (v1): a community-driven catalog (Plausible, n8n, Umami, Ghost and so on) defined in a simple declarative format.
- **Backup verification** (Later): automatically test-restore backups. Almost nobody does this, and it builds trust.

## Multi-server and scaling

- **Multiple servers from one control plane** (v1), managed over SSH or a lightweight agent. A single Rust agent binary is a natural fit.
- **Server provisioning and hardening** (v1): a one-line install script, firewall setup, Docker install, updates.
- **Horizontal scaling** (Later): replicas across nodes with load balancing.
- **Cloud provider integration** (Later): Hetzner, DigitalOcean and others, spinning up servers from the UI.

## Observability

- **Live build and runtime logs** (MVP): streamed over WebSocket or SSE, searchable.
- **Resource metrics** (v1): CPU, memory, disk and network per container and server. Done: per service (its containers summed) and per server, disk for servers only.
- **Alerts and notifications** (v1): Discord, Slack, Telegram, email, webhooks for deploy status, crashes, disk space, cert expiry. Done, disk space included.
- **Uptime and health monitoring** (v1).
- **OpenTelemetry export** (Later).

## Access and teams

- **API tokens** (MVP).
- **Multi-user with roles (RBAC)** (v1): projects and environments as scopes.
- **SSO** (v1): OIDC and GitHub login, with SAML later.
- **2FA and passkeys** (v1).
- **Audit log** (v1).

## Developer experience

- **Web dashboard** (MVP): keep it fast and simple; speed is where Spinikus can beat the competition.
- **CLI** (MVP): deploys, logs, env management and shell access.
- **Full REST or gRPC API, plus an OpenAPI spec** (MVP).
- **Optional per-repo config file** (v1): `spinikus.json` in the repo root, covering build and runtime settings only (build command, port, health check, monorepo paths), with `$schema` support. Domains, secrets and environments stay in the database. A declarative `spinikus apply` command and a Terraform provider are Later.
- **Web terminal into containers** (v1).
- **Cron jobs and one-off tasks** (v1).

## Platform and operations

- **Single static binary with an embedded database** (MVP): SQLite for the control plane, with optional Postgres. Install takes under a minute.
- **Instance configuration** (MVP): `spinikus.toml` holds only what Spinikus needs to start (data directory and DB location, listen addresses and proxy ports, log level, master encryption key source, and node discovery later). Everything else lives in the database.
- **Low resource footprint** (MVP): Spinikus should idle in tens of MB, not hundreds. This is the clearest reason for it to be Rust.
- **Self-update and config backup/restore** (v1).
- **Container runtime choice** (Later): Docker first, with Podman or containerd supported later.

## Web UI

The web dashboard is the main way users will configure and monitor Spinikus, so it gets its own plan.

### Core screens

- **Overview**: all projects, apps, databases and servers with live status, recent deploys, and anything needing attention (failed deploys, disk nearly full, cert about to expire). The **Needs attention** list (also `GET /api/v1/attention`) is computed on each request from what is already recorded (lasting crash loops, each service's latest non-canceled deployment, each database's and each app volume's latest finished backup (one item per service), hostname certificate records, server status and low disk, the latest instance backup, each channel's latest notification), rather than kept in a table of its own, so nothing has to be cleared by hand: an item goes once its cause does. Each principal gets only what they may see.
- **Project / environment / app hierarchy**: production, staging and preview environments per project, with per-environment config.
- **App detail page**: current release, deploy history, rollback button, domains, env vars and secrets, resource limits, health status.
- **Live logs**: build and runtime logs streamed in the browser, with search, filtering and download.
- **Metrics**: CPU, memory, disk and network graphs per app and per server.
- **Databases and backups**: create a database, see connection details, browse backups, restore with one click.
- **Servers**: add a server, see its health, run the install script, view hardening status.
- **Settings**: users and roles, SSO, API tokens, notification channels, Git provider connections, audit log.

### Interactive features

- **Deploy flow**: connect a repo, auto-detect the build strategy, review the detected config, deploy.
- **Web terminal** into running containers.
- **Export view**: an app's build and runtime settings can be exported as `spinikus.json`, so users can commit it to Git.
- **Service template catalog** with one-click install.
- **Command palette** (Cmd/Ctrl+K) for fast navigation.

### Architecture decisions

1. **API-first**: the public JSON API covers everything the UI does, and the CLI uses it. The UI's HTML endpoints and the API are thin layers over the same internal service functions, so the UI never has capabilities the API lacks.
2. **Realtime updates**: SSE (through the HTMX SSE extension) for logs, deploy progress and status changes.
3. **Embedded in the single binary**: HTMX and the JS islands are vendored and embedded (for example with `rust-embed`), so the one-binary install promise holds and Cargo alone builds everything.
4. **Frontend stack**: Axum with server-rendered Maud templates and HTMX. Small vendored JS islands cover what HTMX does poorly: the web terminal (xterm.js), charts, a virtualized log viewer and the command palette.
5. **Light footprint**: pages are rendered by the control plane with little client-side JavaScript, which keeps idle memory low and suits mobile monitoring.
6. **First-run experience**: a setup wizard on first visit (admin account, domain, first server) so nobody has to touch config files.

## Differentiators to consider

1. **Resource efficiency**: a measurable advantage over Node-based competitors on small VPSes.
2. **API-first, declarative where it counts**: everything in the UI is also available through the API, and apps can keep their build and runtime settings in the repo.
3. **Built-in proxy**: fewer moving parts than Coolify plus Traefik.
4. **Sleep-on-idle / scale-to-zero** for hobby apps, so a small VPS can host many projects.
5. **Previews and rollbacks treated as first-class**, not bolted on.
6. **Secure defaults**: automatic hardening, rootless containers, per-app network isolation.

## Suggested MVP scope

The first release should cover Git deploy, Dockerfile and Railpack builds, a built-in proxy with automatic HTTPS, env vars and secrets, Postgres and Redis with backups, live logs, a CLI, and a single-binary install. That alone would make a credible first release.

Next step: decide the architecture (control plane vs. agent, proxy approach, container runtime interface) or sharpen the MVP cut line.

## Decisions

- **`spinikus.toml` is instance configuration only**: data directory and DB location, listen addresses and proxy ports, log level, master encryption key source, and node discovery later.
- **Everything else lives in the database**: projects, apps, deployments, Git connections, domains, env vars, secrets, and instance-wide settings such as ACME email, SSO providers and notification channels. These are managed through the UI and the API.
- **The API covers everything the UI does**, so the CLI and a future Terraform provider work on the same footing.
- **The master encryption key lives outside the database** (TOML, env var or key file), so encrypted secrets stay usable after a restore.
- **A full backup is the database, `spinikus.toml` and the key.**
- **The optional per-repo file is `spinikus.json`**, covering build and runtime settings only. It supports `$schema` for editor validation and allows comments. Domains, secrets and environments stay in the database. Declarative apply comes later. A TOML variant (`spinikus.app.toml`) was considered and rejected to avoid confusion with the instance config.
- **For any setting `spinikus.json` covers, the file wins.** The UI shows those settings read-only with a note that the file sets them. The full scope is in the `spinikus.json` scope section below.
- **The UI does not need to be complete for every feature at MVP.** Features reach the UI one step at a time.
- **Mobile support is limited to monitoring and rollback for now.** Full configuration is desktop-only.
- **The frontend is Axum, Maud and HTMX with SSE, plus vendored JS islands** (web terminal, charts, log viewer, command palette). HTML endpoints and the public API share one service layer. An SPA (React) and a Rust-native frontend (Leptos, Dioxus) were considered and not chosen.
- **Styles are Tailwind, built by `cargo xtask css` and committed**: the classes are written in the Maud templates, and the Tailwind standalone CLI (pinned by version and SHA-256, downloaded into `target/xtask/`) scans the crate's Rust source and writes `crates/spinikus-web/assets/app.css`, which is committed, so Cargo alone still builds Spinikus and only someone changing classes needs the CLI (`--check` tells whether the committed file is current). Hand-written CSS and a vendored classless framework (Pico) were the alternatives; Tailwind keeps the styling next to the markup and the output small.
- **The dashboard and the API share one listener**: the API answers under `/api/v1` and `/hooks`, the dashboard everywhere else. Assets are embedded with `include_bytes!` and linked with their content hash (`?v=`), so the current version is cached for good and an old one never is.
- **Web sessions are cookies, with CSRF protection by token and by origin**: the session cookie holds the session's secret (`HttpOnly`, `SameSite=Lax`, `Secure` when a proxy says the browser came over HTTPS). Every request that changes something must come from the site itself (`Sec-Fetch-Site` and `Origin`, when sent) and, with a session, carry its CSRF token: a hash of the cookie, so it needs no storage, changes with each session, and can't be made without the cookie. Forms send it as a hidden field, HTMX as the `X-CSRF-Token` header. Sign-in and setup have no session yet and rely on the origin checks.
- **Pages allow only their own scripts and styles**: a strict Content Security Policy (no inline scripts or styles, no frames), so HTMX runs with `includeIndicatorStyles` and `allowEval` off. Its history cache is off too, since pages may show secrets, and pages are `Cache-Control: no-store`. Links and forms are boosted (`hx-boost`); error responses are pages and are swapped in like any other, a redirect after a form is `HX-Location` (a plain redirect would leave the old URL in the address bar), and a lost session sends HTMX to the sign-in page with `HX-Redirect`.
- **Deployments are followed with one SSE stream per page, carrying HTML**: the deployment page connects to its own stream while the deployment is in progress; log lines arrive rendered and are appended, a status change makes the summary (status, actions, history) reload itself from a fragment endpoint, and the end closes the connection. The stream replays the log from the start, so the page renders the log itself only once the deployment has finished; a reconnect resumes after the last line through `Last-Event-ID`. Pushing every part of the summary over the stream was rejected as duplicating the page's rendering; reloading one fragment on each status change costs a request per phase. The service page's deployment list, which changes far less, polls every 2 seconds only while a deployment is in progress.
- **Pages offer only what the user may do**: buttons follow the user's role in the environment (`Control::environment_role`), and every operation still checks for itself. A refusal the user can act on (no source to deploy, nothing to roll back to, a deployment already finished) is shown on the page it came from; other errors are error pages.
- **Editing is one form per section, posted normally**: a settings page has a card per section (general, source, runtime) and a danger card for deleting, each a plain form that redirects back with `?saved=<section>` or re-renders the page with what was typed and the problem (422, with `HX-Push-Url: false` so the address bar keeps the page's URL). Forms carry the revision they were made from, so a change made meanwhile is refused with a message instead of overwritten. Saving fields one at a time inline was rejected as more requests and states for little gain. Creating a service takes its source and port in the same form, so a new service can be deployed right away; the Git or image fields are switched with CSS alone (the CSP allows no inline scripts).
- **Runtime settings that `spinikus.json` sets are shown read-only**, with the values the newest Git build read, and saving the form keeps the stored values for them (the file wins at the next build anyway). Which fields the file set comes from the deployment snapshot (`from_file`), so it is known only after a build.
- **Projects, environments and services have tabs** for their pages (services: deployments, variables, domains, settings), each shown only when the user may open it; settings moved from header buttons into the tabs.
- **Each scope has one variables page**, listing its own variables, then those it inherits with the ones it overrides struck through, so where a value comes from is visible in one place. Secret values never reach a page except through Reveal, a POST (it is audited) that only admins see, which HTMX swaps into the row; editing a secret shows no value and an empty one keeps it. Showing every scope's variables merged in one list was rejected as hiding where to edit each.
- **Domains are managed on their service's Domains tab**, with the generated hostname and each domain's certificate status (HTTPS, failing with the error, none yet, or HTTP only for names that can't get one). A domain serves the app or redirects to another domain of the service that does; hostnames aren't edited in place, since a new hostname means a new certificate anyway (remove and add instead).
- **A database's page says how to connect to it** (its slug and port on the environment's network, and the `${{ <slug>.DATABASE_URL }}` reference to put in another service's variables), and its Backups tab holds backing up now, the backups and restores (refreshing every 2 seconds while one is under way), and schedules. Restores ask for confirmation in the browser and are listed with their state; the safety backup taken first is labeled as such.
- **Settings are one area with tabs**, at `/settings`, linked from the header for everyone: instance admins get General (the base domain, HTTPS certificates and releases kept, a card per group, each setting saved at its own revision; an empty base domain or email turns that feature off), Servers and Storage, and everyone gets their own API tokens. A destination's secret access key is write-only on the page as in the API: never shown, and kept when the field is left empty. Testing a destination writes and deletes a small object from the control plane and shows the storage's own error if that fails.
- **Servers are added on the dashboard and enrolled from the server**: adding one leads to its page, where a button creates a join token and shows it once, inside the `spinikus agent join` command with the CA fingerprint. The command uses the host the browser reached the dashboard at and the agent listener's default port, since the control plane doesn't know the address its agents reach it at; the page says to change them if they differ. A setting for the public agent URL was left for when installs need it. The control plane's own server can't get a join token or be revoked from the page, since its agent enrolls itself. Each server shows its status (waiting for its agent, connected, disconnected, revoked), when it was last seen, and what its agent reported (host, system, agent version).
- **Placement is chosen on the forms**: creating a service or database, and a service's settings, offer a server select when there is more than one server; the default is the control plane's server. A revoked server stays shown for a service that still names it, but can't be chosen. Servers are listed for anyone signed in for this, as storage destinations are.
- **API tokens are created on the dashboard with a role, a scope and a lifetime** (30, 90 or 365 days, 7 days, or never; 90 by default). The secret is shown once on the page that answers the form (with `HX-Replace-Url`, so a reload doesn't post again) and never again; the list shows each token's scope, last use and expiry, and revokes them.
- **Runtime logs are a service tab, streamed like a deployment's log**: the page connects to its own SSE stream, which opens a log session on the agent and sends each line rendered (time, text, stderr in red, search matches marked). Search and the stdout/stderr choice are applied on the control plane to the lines read, so the agent and the API stay unchanged; the page starts with the last 100, 1,000 or 10,000 lines and follows from there. A refusal (nothing live, no container, the agent away) is the log box's last line rather than a failed connection, which the browser would retry forever. A reconnect resumes from the time of the last line it got (`Last-Event-ID`), not the tail again. Download gives the same lines as text without following. A failed or replaced deployment's container can be read until cleanup removes it, linked from the deployment page, which also downloads the deployment's own log. Filtering in the browser was rejected because a search would then only see lines already on the page; a virtualized viewer is left for when long sessions need it.
- **The command palette is a small script of our own** (`palette.js`, the one JS island so far): Cmd/Ctrl+K or the header's Search button opens a `<dialog>`, which fetches `/palette` (links to every project, environment and service the user can see, and the pages they may open, at most 2,000) each time it opens and filters them in the browser as the user types; arrow keys move, Enter follows the link (boosted like any other), Escape closes. Searching on the server per keystroke was rejected as a request per key for lists that are small; keeping the index between opens, as possibly stale.
- **Users, the audit log and your account live in Settings**: instance admins get Users (the list, adding with an initial password, and a user's page with their account, a password reset, their memberships, their API tokens to revoke, and disabling) and Audit log; everyone gets Account (who they are, and changing their password). Projects get Members and Audit log tabs and environments a Members tab, for their admins. An audit entry shows when, the action, its target (a link when the reader may open it), who did it (a user, a server, Spinikus itself or a Git webhook, and whether through an API token) and its details as `key=value`, long values shortened.
- **Setup and sign-in are pages**: on first visit the dashboard asks for the first admin and signs them in; signing in returns to the page that asked, and only to a path on this site.

## `spinikus.json` scope

The test for what belongs in the file: does the setting change with the code version? If it must ship in the same commit as the code (a new start command, a new port), it goes in `spinikus.json`. If it describes where or how the app is hosted, it stays in the database.

### Build

- Build strategy override (Dockerfile, Compose, Railpack or buildpacks) when auto-detection guesses wrong.
- Dockerfile path, build context and target stage.
- Install and build commands, and a root directory for monorepos.
- Non-secret build args.
- Watch paths, so only relevant changes trigger a build.

### Runtime

- Start command and listening port. `runtime.start` is run by `/bin/sh -c` in place of the image's command, so it can use the container's environment; Redis databases use it to set their password.
- Health check (path, interval, timeout, retries, start period), which gates zero-downtime cutover.
- Graceful shutdown timeout.
- Release command that runs once before cutover, such as database migrations.
- Extra processes (worker, scheduler), in the spirit of a Procfile.
- Persistent volume mount paths.
- Cron jobs: schedule and command.

### Environment variable declarations

Names of required variables, plus non-secret defaults, so a deploy fails early with a clear message when something is missing. Values for secrets never go in the file.

### Monorepos

An `apps` array, each entry with its own root directory, build settings and trigger paths.

### Gray areas, decided

- **Resource limits** (CPU, memory): allowed as optional hints in the file, which the UI can override.
- **Preview deployment behavior** (enabled, branch filters): stays in the database for MVP.
- **Service dependencies** (such as "this app needs Postgres"): not specified yet; too early.

### Never in the file

Domains, env var values and secrets, server placement, Git credentials, users and roles, notification channels, and backup schedules.

### Example

```json
{
  "$schema": "https://spinikus.dev/schema/v1.json",
  "build": { "strategy": "dockerfile", "dockerfile": "docker/Dockerfile" },
  "runtime": {
    "port": 3000,
    "start": "node dist/server.js",
    "release": "npm run migrate",
    "healthcheck": { "path": "/healthz", "interval": "10s" }
  },
  "env": { "required": ["DATABASE_URL"] },
  "cron": [{ "schedule": "0 3 * * *", "command": "node dist/cleanup.js" }]
}
```

The `$schema` URL is a placeholder.

## Backend architecture

Spinikus is one Rust binary with two runtime roles: a control plane that owns state and decisions, and an agent that owns containers, builds and traffic on one server. On a single-server install the agent runs inside the control plane's process, behind the same interface a remote agent uses, so multi-server in v1 needs no rewrite. Agents connect out to the control plane over a long-lived mTLS stream.

&#91;embedded content: control plane and agent · 8 components, 1 connection\]

The agent opens the connection, so servers behind NAT work and nothing connects in over SSH.

### Control plane

- **API layer (Axum)**: JSON API, Maud/HTMX pages, SSE and auth, all calling one shared service layer.
- **Core and store**: the domain model (projects, apps, environments, deployments) in SQLite, with secrets encrypted by the master key.
- **Reconciler**: compares desired state in the database with what agents report, and acts on the difference.
- **Deployments as durable state machines**: queued, building, starting, health-checking, then live, or failed and rolled back. A restart never loses track of one.
- **Event stream**: logs, progress and status changes flowing to the UI over SSE.

### Agent

- **R\****untime driver**: a trait over the container runtime, with Docker first.
- **Builder**: clones the repo, builds the image and reports progress.
- **Proxy and certificate manager**: routes and TLS for the apps on that server, built on hyper and rustls.
- **Reporter**: streams logs, stats and container state back to the control plane.

### Decided

- **Agents dial out to the control plane** over a long-lived mTLS gRPC stream, instead of the control plane connecting in over SSH. It works through NAT, avoids SSH key handling, and lets agents push events. SSH is used only once, by the install script.
- **The proxy is built on hyper and rustls**, sharing the Tokio runtime with the rest of Spinikus, instead of Pingora.
- **The proxy runs on every agent**, with the control plane distributing route tables. Traffic never has to cross servers just to reach an app, and the single-server case works the same way.
- **ACME uses instant-acme** with our own certificate manager, storing certificates in the database and using an SNI resolver in the proxy. HTTP-01 works: the certificate manager runs on the control plane and the challenges go to the agents' proxies (see Agent protocol). It was checked against Pebble, Let's Encrypt's test CA.
- **Container runtime**: bollard behind our own trait, covering Docker and Podman.
- **Builds**: Dockerfile and Compose builds go through the runtime; auto-detected builds shell out to Railpack, which produces an image. Railpack is the successor to Nixpacks, which is in maintenance mode.
- **The agent clones with the `git` CLI**, not libgit2 or gitoxide: a shallow fetch of the branch (or the exact commit), with submodules, in a fresh directory per deployment that is removed afterwards. Git must be installed on the server. It runs with no system or user config, never prompts, and allows only `https`, `http` and `ssh` (local paths only in tests).
- **Private repositories use deploy keys for now**: one Ed25519 key per service, created when an SSH source is set and rotated on request, its private half sealed in the database and sent with each build. The agent trusts a host key the first time it sees it (`known_hosts` in the agent directory). Git provider connections (GitHub Apps and the like) come later and will not replace deploy keys for generic Git over SSH.
- **Dockerfile builds use BuildKit through bollard**, sending the context as a tar stream without `.git`. The image is tagged `spinikus/<service>:<deployment>` and run by its image ID, so a later build can't change what a deployment runs. Plain build variables become build arguments and secret ones BuildKit secrets (`RUN --mount=type=secret,id=<NAME>`), which stay out of the image and its history; secret values are also redacted from the build output. bollard starts a BuildKit session of its own for every BuildKit build and serves only registry credentials there, with that plumbing private, so builds with secrets open their own session (`/session` upgraded to h2c, over which Docker calls the agent's gRPC secrets service) and send their own `/build` naming it, reading the output as bollard would; builds without secrets still go through bollard. We rejected writing secrets into the context or passing them as build arguments, which would leave them in layers or the history. This needs Docker on a Unix socket or plain TCP, which is how the agent reaches it. Compose and buildpacks are refused with a clear message until they exist.
- **Railpack builds without a Dockerfile.** With no `build.strategy`, a root directory with a Dockerfile (or a `build.dockerfile`/`target` setting, so a missing named Dockerfile is an error rather than a different build) is built from it, and anything else by Railpack; `strategy: "railpack"` forces it, and `build.command` replaces the build command it detects. The agent runs `railpack prepare` (the binary from `SPINIKUS_RAILPACK` or the `PATH`, not bundled: it is the server operator's to install and update) and builds the plan through the runtime with Railpack's BuildKit frontend, which reads the plan and the context as BuildKit local sources rather than from an uploaded tarball, so these builds go through the agent's own BuildKit session, which serves the context directory over `FileSync.DiffCopy` (fsutil's protocol, honoring the `followpaths` and exclude patterns BuildKit sends), with `remote=client-session`. The frontend is named by `BUILDKIT_SYNTAX`, pinned to the frontend tag of the installed `railpack --version` since plan and frontend must match, and the service ID is the `cache-key`, so services don't share cache mounts. We rejected running `railpack build`, which needs its own BuildKit daemon and wouldn't stream through the runtime. Railpack passes build variables as BuildKit secrets (the plan lists their names), so every build variable, secret or not, goes to the build as one through the agent's own BuildKit session, with `secrets-hash` (a digest of the values) as a build argument so changing them invalidates the cache, as `railpack build` does; none end up in the image. The plan is written into the context as `.spinikus-railpack-plan.json` and excluded from the image.
- **`PORT` is set in every container** to the runtime `port`, unless a variable sets it, since most frameworks (and Railpack's start commands) listen on it.
- **The agent cleans up after builds and environments on its own**: every hour it prunes build cache no build has used for 7 days (layers and Railpack's cache mounts) and removes Spinikus's networks that no container is on any more, stopped containers included, since they could not start again without theirs. The control plane doesn't direct it because neither is tied to one deployment. A size cap on the cache was considered; age alone keeps what active services rebuild with.
- **Deployment logs** (the clone, build, pull and health check) are files on the control plane, `<data_dir>/logs/deployments/<id>.log`, capped at 10 MiB each, and served as plain text at `GET /deployments/{id}/logs` and live over SSE at `GET /deployments/{id}/stream` (see API).
- **Database**: SQLite only for MVP, with repository traits so Postgres can be added later.
- **Logs and metrics**: logs in rotated files on disk with a retention limit; metrics with short retention and downsampling.

## Data model

The data model is built around a split between desired state (what users configured) and observed state (what agents report). The reconciler acts on the difference. Logs live on disk, not in the database.

&#91;embedded content: main entities · 12 boxes, 1 hierarchy\]

A service owns its source, deployments, domains, variables and volumes; instances link it to the servers it runs on.

### Entities

| Entity                                           | Holds                                                                         | Relates to                                                              |
| ------------------------------------------------ | ----------------------------------------------------------------------------- | ----------------------------------------------------------------------- |
| Project                                          | Name and slug                                                                 | Has environments and members                                            |
| Environment                                      | Kind (production, staging, preview); PR number for previews                   | Belongs to a project; has services                                      |
| Service                                          | Kind (app, database, template); kind-specific JSON config; current deployment | Belongs to an environment; has a source, variables, volumes and domains |
| Source                                           | Git repo (provider connection, branch, root directory) or image reference     | Belongs to a service                                                    |
| Deployment                                       | Commit or image, trigger, status, timestamps, config snapshot; immutable      | Belongs to a service                                                    |
| Deployment event                                 | One row per state transition                                                  | Belongs to a deployment                                                 |
| Server                                           | Agent identity (mTLS), status, last-seen time                                 | Runs instances                                                          |
| Instance                                         | A container as reported by an agent: ID, health, status (observed state)      | Links a service to a server                                             |
| Job                                              | Durable queue entry for builds, backups and cleanups                          | Standalone                                                              |
| Domain                                           | Hostname and redirect setting                                                 | Points at a service; uses a certificate                                 |
| Certificate                                      | Certificate, encrypted key, expiry, ACME account details                      | Used by domains                                                         |
| Variable                                         | Name, encrypted value, build-time or runtime, secret flag                     | Scoped to a project, environment or service                             |
| Volume                                           | Persistent mount path                                                         | Belongs to a service                                                    |
| Storage destination, backup schedule, backup run | S3-compatible target, schedule, run results (verification later)              | Backups belong to database services, and to apps with volumes           |
| User, membership, API token, session             | Accounts and access; a membership scopes a role to a project or environment   | Memberships link users to projects or environments                      |
| Audit log                                        | Append-only record of actions                                                 | References actors and entities                                          |
| Notification channel, Git connection, setting    | Instance-wide integrations and key/value settings                             | Standalone                                                              |

Logs, and metrics beyond short retention, are deliberately not stored in the database.

### Decided

- **IDs are prefixed ULIDs** (for example `svc_01J…`): sortable by creation time and safe to expose in the API and URLs. Human-friendly slugs are separate and unique per parent.
- **One service table with a kind** (app, database, template) and kind-specific JSON config, since all three end up as containers with ports, volumes and variables.
- **Desired vs observed state** are kept in separate tables, with agents writing only observed state.
- **Deployments are immutable records** with a config snapshot. Rollback redeploys an old deployment's image and snapshot, with no rebuild.
- **Variable resolution** follows one precedence chain (project, then environment, then service; most specific wins), resolved at deploy time into the deployment snapshot.
- **Variable references**: a value can include another service's variable as `${{ postgres.PASSWORD }}`, naming a service of the same environment by its slug (`$${{` writes a literal `${{`). It is resolved when a deployment is created, from the variables that service gets, so a database's generated password reaches its apps without being copied, and changing it means redeploying them. The referenced value is used as it is, without following references inside it, so there are no chains or cycles. A variable that takes a secret's value is treated as secret. A reference to a service or variable that doesn't exist fails the deployment's creation; when saving, only the syntax is checked, since the other service may come later. Injecting every database's variables into every service automatically was rejected as too implicit, and copying the connection URL by hand because rotating a password would mean editing every app.
- **Variables API**: `/projects/{id}/variables`, `/environments/{id}/variables`, `/services/{id}/variables` and `/variables/{id}`. Developers of a scope set its variables and viewers list them; secret values are write-only, and `POST /variables/{id}/reveal` returns one only to admins of its scope, audited and marked `Cache-Control: no-store`. A secret can't be made plain again. Values are at most 64 KiB.
- **Volumes API**: `/services/{id}/volumes` and `/volumes/{id}`. Developers add and change volumes; deleting one needs the admin role. Changes apply from the next deployment. The data stays on the server after a volume is deleted, until volume cleanup exists. The dashboard edits them in a Volumes card on the service's settings, for apps only: a database's data volume is listed but not edited there, since moving it away from the image's data path would start the database empty. The card says that anything outside a volume is lost on redeploy, and that volumes make deployments `recreate`, with a short downtime. The CLI has `spinikus volume list|add|update|remove` (and `backup|backups|restore`), which finds a volume by name. The service layer refuses to move or delete a database's data volume (a `mount_path` change or a delete of the volume at its engine's data path is `rejected`, so the API answers 409 and the CLI exits 6): either would start the database empty, and deleting the database is the way to drop its data. Renaming it, and adding, moving or deleting other volumes of a database, are allowed.
- **One-click databases are ordinary services**: `POST /environments/{id}/databases` with an engine (`postgres` 15–18 or `redis` 7–8, the newest by default) creates a service of kind `database` with the engine's official image (by major version) as its source, a `data` volume where the image keeps its data, runtime settings, and variables with a generated password: `POSTGRES_USER`, `POSTGRES_PASSWORD`, `POSTGRES_DB` and `DATABASE_URL` for Postgres, `REDIS_PASSWORD` and `REDIS_URL` for Redis, the password and the URLs secret. It is then deployed, unless `deploy` is `false`: then it waits, with `deployment: null` in the answer, so its settings and variables can be changed before its first run (the dashboard's **Deploy it now** checkbox, `spinikus db create --no-deploy`). One started from a backup is always deployed, since the restore waits for it. Putting every setting on the create form was considered; waiting reuses the settings page instead. Apps use it through references (`DATABASE_URL = ${{ postgres.DATABASE_URL }}`), reaching it by its slug on the environment's network. Each part is created through the same operations as by hand, so permissions (developers of the environment) and audit entries are the same, and everything stays editable afterwards. Databases are deployed with `recreate`, accepting a short downtime, since two containers must not share the data. The images read the credentials only when they initialize an empty volume, so changing the variables later doesn't change the password; the URLs are stored as whole values rather than references for the same reason. Health checks only wait for the container to keep running for now, and a major version upgrade is the user's to plan (Postgres can't open data from an older major version). Separate tables for databases, and a generic template format, were left for later: templates will produce the same parts.
- **Backups go straight from the database's server to S3-compatible storage**: the control plane sends the agent of the server the database is live on a `BackupDatabase` command with the destination's credentials (over mutual TLS), and the agent runs the engine's own dump in the container (`pg_dump --format=custom`, or `redis-cli --rdb` to a temporary file in the container, then `cat`, since written to stdout it ends with the diskless transfer's end marker, which isn't RDB; with the container's own credentials) and streams its stdout to the bucket as a multipart upload. Nothing passes through the control plane, and a dump never has to fit in memory or on disk. A dump that exits with an error aborts the upload and fails the backup with its stderr. Relaying the dump through the control plane was rejected because it doubles the traffic and makes the control plane's link the bottleneck. The client is `object_store`, using rustls with ring, so AWS, R2, B2, MinIO and RustFS all work, with path-style addressing as an option. Dumps are keyed `<path prefix><service ID>/<backup ID>.pgdump|rdb`.
- **An app's volumes are backed up one at a time, as tar archives made in a helper container**: a backup of an app (`POST /services/{id}/backups` with `volume_id`, which may be left out when the app has one volume) holds one volume's files as a gzipped tar, keyed `<prefix><service ID>/<backup ID>.tar.gz`; `backups` records either the database's `engine` or the `volume_id` (not a foreign key, since volumes are deleted and their backups kept). A schedule on an app backs up every volume at each run, and keeps `keep_last` backups of each volume. The control plane sends `BackupVolume` to the server the app last ran on, where its volume's data is (the live deployment's server, else the newest drained or superseded one's), with the image and command; the agent starts a helper container of `busybox` (pinned by tag in core, `HELPER_IMAGE`) with the volume mounted read-only at `/volume`, no network and its own one-off label, runs `tar -czf - -C /volume .` in it through exec, streams stdout to the bucket as a database dump is, and removes the helper. A restore (`RestoreVolume`, environment admins, after a safety backup of the same volume) downloads the archive first, then stops the app's live container (with its shutdown timeout) if there is one, mounts the volume read-write in a helper, unpacks the archive into a staging directory inside the volume, and only once it unpacked whole deletes the volume's entries and moves the staged ones in; the container is started again whether or not that succeeded, and a failure before the swap leaves the files unchanged. A volume restore is refused while a deployment of the app is in progress (a new container could start on the volume midway) and for a volume since deleted. Backups run while the app runs, so a file being written may be copied half-written; stopping the app for every backup was rejected as downtime on a schedule. Running `tar` in the app's own container was rejected because images need not have it (distroless ones have no shell at all); `docker cp`'s archive endpoints were rejected because a restore can't remove what the archive lacks; reading the volume's host directory was rejected because the agent doesn't run as root and Docker's data root may be remote. A database's data volume isn't archived: a file copy of a running database isn't consistent, so databases keep their dumps.
- **Storage destinations belong to the instance**: instance admins manage them (`/storage-destinations`, with `POST …/test` writing and deleting a small object from the control plane), and anyone signed in can list them to pick one. The secret access key is write-only, sealed with the destination's ID as context. A destination that a schedule uses can't be deleted; deleted destinations stay readable to their backups, so those can still be deleted.
- **Schedules and retention**: a database's environment admins manage its schedules (`/services/{id}/backup-schedules`, `/backup-schedules/{id}`): a five-field cron expression in UTC, a destination, and `keep_last` (default 7, at most 1000). Runs missed while the control plane was down collapse into one. The runner deletes each schedule's successful backups beyond `keep_last` (the object, then the row, which is kept with `deleted_at`); backups made by hand are never pruned. Developers make backups by hand (`POST /services/{id}/backups`), viewers list them, and admins delete them. A backup fails if the database has nothing live, if its agent stays away more than 2 minutes, or after 6 hours.
- **Restore overwrites the database, after a safety backup**: environment admins restore a succeeded backup with `POST /backups/{id}/restore` into the live database it came from. The control plane first queues a safety backup of the current data to the same destination and sends the restore only once it succeeded (the restore fails if it fails), so a mistaken restore is undone by restoring the safety backup, which is an ordinary backup and is never pruned. The agent downloads the dump and streams it into the container's stdin; Postgres writes it to a temporary file (a dump written to a pipe has no data offsets, so `pg_restore` needs to seek) and runs `pg_restore --clean --if-exists --no-owner --single-transaction`, so a failed restore changes nothing. One restore runs at a time per database, and backups being restored are neither pruned nor deletable. Redis replaces its data only by loading files on start, and with `--appendonly yes` it loads its append-only file, not `dump.rdb`, so its restore script makes the dump the base of a new multi-part AOF (an RDB base file and a one-line manifest), checks it with `redis-check-aof` before changing anything, switches persistence off in the running server (`CONFIG SET save ''`, `appendonly no`, waiting out a background save or rewrite) so nothing it writes replaces the new files, swaps them in for `appendonlydir` and `dump.rdb`, and the agent then restarts the container and runs a ready command (`PING` answered with `PONG`) until it passes, for up to five minutes; the command line puts persistence back on. Writes between the swap and the restart are lost, as a restore implies. Stopping the container and swapping the files from a helper container was rejected because the agent would have to know the volume layout and run another image; replaying the dump through `RESTORE` commands was rejected because it needs a client that parses RDB. The engine's `RestorePlan` (command, `restart`, `ready_command`) is what the control plane sends. A backup can also go into a new database (`from_backup` on `POST /environments/{id}/databases`, or "Restore into a new database" on a backup), to look at old data or recover from a mistake without touching the database it came from: the new database gets the backup's engine at the source's version or a newer one (dumps don't load into older versions), and a restore without a safety backup, since there is nothing to protect, which waits until its first deployment is live and fails if that deployment does. Copying a database's data takes the admin role in its environment, as restoring does, plus the developer role where the new one goes. Restores are listed at `/services/{id}/restores` and `/restores/{id}`.
- **`spinikus.json` interplay**: the snapshot records the merged result and which fields came from the file, so the UI can show those settings read-only. The agent reads the file from the checkout (in the root directory, at most 64 KiB, comments allowed), uses its `build` section for the build and returns the file with the image; the control plane merges `runtime` into the snapshot before starting. The file wins field by field, a nested object such as `healthcheck` is replaced whole, and `from_file` lists the overridden fields (`runtime.port`). A file that doesn't parse or validate fails the build.
- **Secret encryption** is envelope-style, with each value carrying a key ID, so the master key can be rotated without re-entering secrets.
- **No organization entity at MVP**: memberships scope directly to projects and environments.
- **Deletion**: soft delete for projects, services and domains; hard delete for ephemeral records such as sessions and old events.
- **SQLite specifics**: WAL mode, enforced foreign keys, strict tables, JSON columns for kind-specific config, and automatic migrations on startup.

## Deployment state machine

A deployment is an immutable record that moves through a fixed set of states. Cutover happens only after the health check passes, so a failed deployment never receives traffic and the previous deployment simply stays live. Every transition writes one deployment event row (from, to, reason, actor, time) in the same transaction that changes the status.

&#91;embedded content: deployment states · 8 states, 2 ways to end early\]

A deployment that fails or is canceled before going live never receives traffic, and the previous deployment stays live.

### States

| State        | Meaning                                                                                                          | Moves to                          |
| ------------ | ---------------------------------------------------------------------------------------------------------------- | --------------------------------- |
| `queued`     | Created, waiting for its turn                                                                                    | `preparing`, `canceled`           |
| `preparing`  | The agent clones and builds, or pulls the image                                                                  | `releasing`, `failed`, `canceled` |
| `releasing`  | The release command (such as migrations) runs once in a one-off container; skipped if there is none              | `starting`, `failed`, `canceled`  |
| `starting`   | New containers start from the snapshot config                                                                    | `verifying`, `failed`, `canceled` |
| `verifying`  | The health check (path, interval, retries, start period from `spinikus.json`) must pass                          | `live`, `failed`, `canceled`      |
| `live`       | Routes have switched; this is the service's current deployment                                                   | `draining`                        |
| `draining`   | A replaced deployment finishes in-flight requests, up to the graceful shutdown timeout, then its containers stop | `superseded`                      |
| `superseded` | Replaced by a newer live deployment (terminal)                                                                   | None                              |
| `failed`     | Terminal, with a reason code                                                                                     | None                              |
| `canceled`   | Terminal; cancellation is not possible once the deployment is live                                               | None                              |

### Decided

- **State set**: building and image pull are merged into `preparing`, and the old deployment has an explicit `draining` state.
- **Failure causes live in a reason code**, not in more states: `build_failed`, `release_failed`, `health_check_failed`, `timeout`, `agent_lost`, `canceled_by_user`, `superseded_in_queue`.
- **Two strategies**: `blue_green` is the default for apps (start the new containers, verify, switch routes, drain the old ones). `recreate` is used for databases and anything with a persistent volume (stop the old, start the new), since two containers shouldn't share a volume. Databases accept brief downtime on redeploy.
- **Failure after going live**: no automatic rollback in MVP. Docker restarts a crashing container with its own backoff (the `unless-stopped` policy), so the control plane only notices: each deployment runner pass ends by feeding every live deployment's reported container (status and Docker's restart count) to a `Watch` for that deployment (`spinikus_core::crash`, pure logic over explicit times). Three restarts within ten minutes, or a container not running (or not reported) for a minute, is a crash loop; running ten minutes without a restart is recovery. Crash loops are rows (`crash_loops`, `crl_` IDs; observed state, one lasting per deployment), with the reason, the restarts since just before it began, and when it began and ended; they also end when the deployment stops being live or its service is stopped or deleted, are left alone while the server's agent is away, and are deleted 30 days after ending. Viewers see them as a `crashing` badge, a note with a link to the container's logs, `GET /services/{id}/crash-loops`, and in `spinikus service show`. Watches are in memory, so after the control plane starts a lasting loop ends only once its container has run ten minutes again. Counting restarts on the agent was rejected because the agent reports what is, and the windows and decisions belong with the control plane, where notifications are sent. Automatic rollback can be an opt-in setting later.
- **Notifications go through an outbox**: events (`spinikus_core::notification::NotificationEvent`: a failed deployment, a crash loop starting or ending, a failed database or instance backup, a certificate that failed on its first failure in a row or expires within 14 days, a server disconnected for 5 minutes and back) become a `Message`, queued as one `notifications` row (`ntf_`) per enabled channel that wants the event, in the same place the event is decided (the deployment runner's `fail`, `watch_crashes`, the backup runners, the certificate runner, and the notification runner's own check of servers). `Control::run_notifications` sends due rows every few seconds, retrying after 30 s, 2, 10, 30 and 60 minutes and then marking them failed, and deletes them after 30 days. Channels (`notification_channels`, `ntc_`, instance admins) are a webhook (JSON, signed with HMAC-SHA256 when it has a secret), Slack and Discord incoming webhooks, a Telegram bot, or email over SMTP (lettre); each has one secret sealed with its ID as context, write-only, and its events (none listed means every event, including later ones). SMTP settings are on each email channel rather than a general setting, so channels can differ and nothing else needs SMTP yet. Messages link to the dashboard only when `dashboard_domain` is set. Sending inline from where the event happens was rejected: a slow or failing channel would hold up the runner, and retries would be lost on restart. Low-disk notifications come with metrics.
- **Deleting a service ends its deployments through the runner**: the next pass cancels those in progress (reason `canceled_by_user`, with "its service was deleted" on the event) and drains the live one like any replaced deployment, so the proxy stops routing to it before its container stops; cleanup then removes all its finished deployments' containers and images, keeping no releases. A dedicated `service_deleted` reason was considered; it would need the `deployments` table rebuilt to change its CHECK constraint, for a reason the event message already explains. Volume data stays on the server, as when a volume is deleted.
- **Concurrency**: one active deployment per service. A newer deployment cancels older queued ones (`superseded_in_queue`). An in-progress build finishes unless the user cancels it.
- **Release commands run in a one-off container** from the deployment's image (`runtime.release`, through `/bin/sh -c`, with `release_timeout`, 15 minutes by default), with the runtime variables and on the environment's network, so migrations reach the database by its slug, but without the service's network alias, a published port or volumes: two containers shouldn't share a volume, and other platforms' release phases don't mount them either. Its output goes to the deployment log with secret values redacted, and the container is removed when it ends. It runs while the previous deployment still serves, so migrations must be compatible with the version still running, as with any blue-green deploy. It is labeled as a one-off rather than as one of the deployment's containers, so it is never reported, routed to or reused as the deployment's instance; canceling the deployment kills it by name. Running it with `docker exec` inside the new container was rejected: the container would have to start first, against a schema it may not support yet.
- **Rollback** creates a new deployment with trigger `rollback`, reusing the target's image and snapshot. It starts at `starting` and skips the build and, by default, the release command. It also reuses the target's variables and its server, since the image lives there. Only a deployment that went live and was replaced can be a target, as long as it is still kept; without one, the newest such deployment is the target.
- **Restart restarts the live container in place**: `POST /services/{id}/restart` (developers; the dashboard's Restart button, `spinikus restart`) sends `RestartInstance` with the container and the service's shutdown timeout to the agent of the live deployment's server, waits for its answer (the timeout plus 30 seconds) and answers with the live deployment; it is audited as `service.restart`. Nothing about the deployment changes, so changed variables or settings need a deployment, and requests fail while the container is down; the health check doesn't run again. Restart commands (`restart/<deployment>/<n>`) are not tracked by the runner or resent: the caller waits, and a restart whose answer was lost needs no second one. A new deployment of the same image (zero downtime, picking up changed variables) was considered as what restart means; that is what Deploy does for an image source, and restart is for a stuck process, which is what users reach for it for. Docker may give the container a new host port, so the proxy's first request after it can fail once before it looks the address up again.
- **Stop and start act on the live container, and stopped is a mark on the service**: `POST /services/{id}/stop` and `/start` (developers; the dashboard's Stop and Start buttons, `spinikus stop`/`start`) send `StopInstance` (with the shutdown timeout) or `StartInstance` to the live deployment's agent the way restart does (commands `stop/…` and `start/…`, waited for, not resent), then set or clear `services.stopped_at`, which isn't a setting and doesn't change the revision; audited as `service.stop`/`service.start`. The deployment stays live, so its routes stay and the proxy answers `503 the app is not running`; Docker's `unless-stopped` policy keeps a stopped container stopped across reboots. A deployment going live clears the mark in the same transaction, so Deploy and Rollback start a stopped service. While stopped, restart, terminals, manual backups and restores are refused, and scheduled backups fail. A `stopped` deployment status was rejected: the deployment state machine records how code got to run, and moving a live deployment out of `live` would drop its routes and make it unreachable by rollback and cleanup rules that key on `live`. Stopping by scaling to zero also needs replicas, which there are not.
- **Old releases are kept for rollback and then cleaned up**: each service keeps its newest `releases_kept` replaced deployments (an instance setting, default 5), plus failed and canceled ones newer than its last live deployment (at most 10), so a failure's container and logs stay until a fix goes live. Anything older gets a `RemoveDeployment` command, which removes its containers and image, and is marked `cleaned_at`. An image is removed only when no kept deployment on that server runs it, since rollbacks and cached builds share images. Never forcing removal means an image that some other container uses stays. Images are removed by reference (the build's `spinikus/<service>:<deployment>` tag, or the digest a pull pinned), so a tag that someone else also uses keeps the image. The BuildKit cache is left to Docker for now.
- **Agent loss**: deployments in progress on a server whose agent disconnects are failed with `agent_lost` after a timeout. When the agent reconnects, it reports what is actually running and the reconciler reconciles that against the database.
- **Timeouts**: `preparing` may take 30 minutes. `starting` and `verifying` together may take the health check's longest possible run (start period, then every retry timing out) plus a 2-minute margin. Health check parameters and the shutdown timeout come from the `runtime` object of the service config, in the same shape as `runtime` in `spinikus.json`, with durations written like `10s` or `1m30s`. A control plane that restarts counts timeouts and agent loss from its own start at the earliest, so agents have time to reconnect.
- **Placement**: a service names the server it runs on (`server_id`), and without one it runs on the embedded server. Anyone signed in can list servers (`GET /servers`, `/servers/{id}`) to choose one; adding, revoking and join tokens stay with instance admins. A deployment fixes its server when it is created, so moving a service affects only later deployments. Choosing a server per deployment, and spreading replicas over several servers, were left for later.
- **Cutover waits for the proxy**: when a deployment goes live, the previous one is `draining`, but it is stopped only once its server's agent has applied a route table that no longer points at it. Stopping it right away would fail the requests still routed there.
- **Driving deployments**: a runner in the control plane moves each deployment forward by sending commands whose IDs are the deployment ID and the step (`dep_…/prepare`, `start`, `stop`, and `cleanup` for a container a start left behind after a cancel). An unanswered command is sent again after 30 seconds and whenever the agent reconnects, and only the agent of the deployment's server can report on it.

## Agent protocol

Agents dial out to the control plane over gRPC with mTLS, so the control plane can never call an RPC on an agent: everything it wants an agent to do travels down a stream the agent opened. tonic and prost are the likely implementation, with the protocol in a versioned package such as `spinikus.agent.v1`.

&#91;embedded content: agent connections · 5 RPCs, 1 control stream\]

`Connect` is the main channel; the other streams carry enrollment, on-demand sessions and bulk metrics.

### RPCs

| RPC                | Kind                 | Called by                         | Purpose                                                                  |
| ------------------ | -------------------- | --------------------------------- | ------------------------------------------------------------------------ |
| `Enroll`           | Unary                | Agent (no client certificate yet) | Exchange a one-time join token and a CSR for a signed client certificate |
| `RenewCertificate` | Unary                | Agent                             | Rotate its client certificate before expiry                              |
| `Connect`          | Bidirectional stream | Agent                             | The control channel: hello, heartbeats, commands, acks and state reports |
| `OpenSession`      | Bidirectional stream | Agent, when told to               | On-demand sessions such as the web terminal and live log tails           |
| `PushStats`        | Client stream        | Agent                             | Container and server metrics samples                                     |

### Messages on `Connect`

- **Control plane to agent**: `SetRoutes` and `SetCertificates` (full snapshots with a revision number), `PrepareDeployment` (clone and build, or pull), `RunRelease`, `StartDeployment`, `StopDeployment`, `RemoveDeployment` (a finished deployment's containers and image, once it is no longer kept), `BackupDatabase` (dump a database and upload it), `RestoreDatabase` (download a dump and load it), `BackupVolume` and `RestoreVolume` (archive a volume in a helper container and upload it, or download an archive and unpack it, stopping the app meanwhile), `RestartInstance`, `StopInstance`, `StartInstance`, `OpenSession` (the request, not the stream) and `CancelCommand`.
- **Agent to control plane**: `Hello` (protocol version, agent version, server facts), `Heartbeat`, `StateReport` (a full instance snapshot on connect, deltas afterwards), and `CommandProgress` and `CommandResult`. The health check result for the `verifying` state comes back as a command result, since the agent runs the check locally.

### Decided

- **Runtime logs stay on the agent that runs the container** and are streamed on demand through a session, which keeps constant log traffic off the control plane. Build logs stream live to the control plane and are stored with the deployment.
- **The embedded agent connects over loopback gRPC**, exactly like a remote agent, so there is one code path.
- **Metrics**: agents push samples to the control plane, which downsamples them and keeps short retention. The agent samples every 15 seconds (containers through the runtime's stats, one-shot so no call waits for a second sample; the machine from `/proc` and `statvfs` of Docker's data root), computes rates from successive counters (CPU in cores, network in bytes per second), and queues one mean per container and for the server per minute, labeled with the minute's end; it pushes the queue once a minute over `PushStats` (a client stream of its own, never the control stream), keeping a bounded backlog while disconnected. The control plane accepts points only from a connected agent, maps container IDs to deployments and services through the instances that agent reported (containers it didn't report are dropped), and stores them in SQLite (`metric_points`: minute rows for 24 hours, hourly means for 30 days, rolled up and pruned every 10 minutes; `INSERT OR REPLACE` by subject and period, so a resent push is harmless). This is the "short retention" the data model allows in the database; anything longer belongs in an external system later (OpenTelemetry export). A service's points are its containers' summed per minute, so both count during a blue-green switch. Graphs bucket the points to at most a few hundred per range (`spinikus_core::metrics::bucket`) and are inline SVG drawn on the server, one measure per chart, with no script. Disk use of 90% notifies `server.disk_low` once, until it is under 85% (hysteresis in memory). Rejected: computing CPU percentages through Docker's two-sample stats (a second's wait per container per sample), a time-series database (another service to run, for a few MB of data), and per-container disk use (Docker's size calculation is too slow to run every minute).
- **Identity**: agents connect to a separate listener that requires mTLS. The certificate subject carries the server ID (`srv_…`), and revoking a server blocks it even if its certificate has not expired.
- **Enrollment**: the install script includes a single-use, short-lived join token and the control plane CA's fingerprint. The agent generates its keypair locally and sends a CSR with the token. Certificates come from an internal CA whose key is stored encrypted under the master key. They are valid for 30 days by default (configurable) and are renewed over the live connection once about two thirds of the lifetime has passed. An agent that stays offline past expiry must re-enroll with a new join token.
- **Streams**: one control stream for low-volume reliable messages, with separate streams for bulk data, so a burst of metrics can never delay a deploy command.
- **Commands and snapshots**: deployments are multi-step workflows, so they are commands with IDs. Routes and certificates are versioned snapshots, and the agent acknowledges the revision it applied. The agent keeps the last routes and certificates on disk, so the proxy keeps serving traffic when the control plane is down or the agent restarts.
- **Delivery**: at-least-once, with command IDs and idempotent handlers. The agent persists in-flight command state locally so it can resume after a restart.
- **Reconnect and liveness**: exponential backoff with jitter. The first message after every connect is a full `StateReport`, which the reconciler compares against the database. Liveness uses application heartbeats every 10 seconds plus HTTP/2 keepalive. The agent-lost timeout is configurable and defaults to 2 minutes: deployments in progress on a server that has been disconnected that long fail with `agent_lost`, while a shorter outage resumes from the agent's persisted command state.
- **On-demand sessions**: for a web terminal, the control plane sends an `OpenSession` command on the control stream, and the agent opens a new stream tagged with the session ID. The same pattern serves live log tails. An `OpenSession` is not a tracked command: it gets no `CommandResult`, is not resent, and does not outlive the connection. The control plane waits up to 10 seconds for the stream and accepts it only from the server it asked. The agent's first frame names the session; a log tail then sends `log_line` frames and `closed` (with an error, if any). Either side ends it by ending its half: when nobody reads the log any more, the control plane closes its side and the agent stops following. The agent waits for the control plane to close before dropping the call, since dropping it would discard frames not yet sent.
- **Terminals** are sessions too: `TerminalSession` names the container, the size and the command (the agent's default is `bash` if the image has it, else `sh`). The control plane's `data` frames are keystrokes and its `resize` frames window changes; the agent runs the command through the runtime with a TTY (`TERM=xterm-256color`), as the container's user, sends the output as `data`, and ends with `closed` carrying the exit code. Only a running container can have one: a deployment whose health check failed has its container stopped, so its logs are the way in. When the control plane closes its side, the agent types Ctrl+D, which ends a shell at its prompt, and then closes the command's input. Docker has no hangup for an exec, so a program running in the foreground can outlive the window until it exits or its container is replaced; finding and killing it (a wrapper recording its PID) was considered and left for later, since it needs a shell in the image.
- **Runtime logs are the app's own output** and are sent as they are, not redacted: an app that prints its secrets shows them to whoever can read its logs, as it would on any host.
- **Versioning**: additive changes only within `v1`. `Hello` carries the protocol version, and the control plane enforces a minimum supported agent version. Agent self-update is a later command.
- **Routes**: each server gets the routes of the live deployments that run on it, and no others, so traffic never crosses servers. The control plane rebuilds every server's table on each runner pass and sends one only when it changed or was not applied. A changed table gets a revision that is at least the current time in milliseconds, so a table built after a database restore never reuses a revision an agent already has. The agent saves the table and acknowledges it with `SnapshotApplied`, and reports the revision it has in `Hello`.
- **Generated hostnames**: every app service is reachable at `<service>-<environment>-<project>.<base domain>`, one label under the instance setting `base_domain`, so one wildcard DNS record and, later, one wildcard certificate cover them all. The base domain defaults to `localhost`, which resolves to the machine itself without DNS, and `null` turns generated hostnames off. Slugs cannot contain `--`, but they do contain hyphens, so two services can produce the same label; then the newer service, and any label over 63 characters, ends with the last characters of the service ID. Custom domains under the base domain are refused. `<service>.<environment>.<project>.<base domain>` was considered and rejected, because a wildcard certificate covers only one level.
- **The proxy**: HTTP/1 and HTTP/2 (by ALPN over TLS, by prior knowledge over plain HTTP), WebSocket upgrades, and `X-Forwarded-For`, `-Host` and `-Proto`. Redirect domains answer with a 308 that keeps the port, path and query. HTTPS (`[proxy] https_listen`, default port 443) picks the certificate by SNI, exact names before wildcards, and a handshake for a name without a certificate fails. Plain HTTP answers ACME HTTP-01 challenges for any host, and otherwise redirects with a 308 to HTTPS every host that has a certificate. Hosts without one, such as `localhost` names, stay on plain HTTP.
- **Certificates**: the control plane gets them, never the agents, so there is one ACME account and one place for rate limits and failures. Getting certificates is off until an admin sets the `acme_email` setting, which agrees to the CA's terms; `acme_directory_url` defaults to Let's Encrypt's production directory. Each public hostname in the route tables (custom domains, redirect domains and generated hostnames) gets its own certificate through HTTP-01, and it is renewed once two thirds of its lifetime have passed. Names that cannot be public (`localhost`, `.local`, `.test`, `.example`, `.internal` and the like) are skipped, and so are wildcards, which need DNS-01; a wildcard certificate for the base domain can come with DNS providers. Where each hostname stands lives in `hostname_certificates`: its current certificate, the failures in a row, the last error and when to retry, backing off 5 minutes, 15 minutes, 1 hour, 6 hours, then daily. The domains list shows it. Certificate rows never change; a renewal adds one. `domains.certificate_id` is not used for ACME certificates, since generated hostnames have no domain row; it is left for uploaded certificates.
- **The dashboard gets its own domain through the proxy**: the instance setting `dashboard_domain` (`null` by default) adds a route of its own kind (`RouteAction::Dashboard`, `Route.dashboard` in the protocol) to the embedded server's route table, ahead of every app's, so it gets a certificate and the HTTP-to-HTTPS redirect like any routed hostname. Only the embedded agent knows where the dashboard listens (`ProxyListeners.dashboard`, the `[http] listen` address, loopback for one bound to every address); another agent sent the route would answer 404, and an install without the embedded agent has no route, so it puts its own proxy in front. Custom domains can't take the dashboard's hostname, and it can't take a custom domain's; it may take a generated hostname's, which then serves the dashboard. The proxy keeps `Host` and sets `X-Forwarded-Proto`, so the origin check, `Secure` session cookies and absolute URLs (webhooks) work unchanged, and the client address recorded for sessions and the audit log is the last `X-Forwarded-For` entry when the request comes from loopback, the peer's otherwise (earlier entries are whatever the client sent). Once it works, `[http] listen` can move to `127.0.0.1:8080`. A separate listener with its own certificate in the control plane was considered; going through the proxy reuses ACME, renewals, the redirect and the ports already open, and keeps one place that terminates TLS.
- **Certificate snapshots**: each runner pass also gives every server the certificates of the hostnames it routes that have not expired (`server_certificates`, with revisions like route tables), sent with their private keys as `SetCertificates` over mTLS. The agent saves them to a file only its user can read, so HTTPS keeps working while the control plane is down, and reports the revision in `Hello`.
- **ACME challenges**: every connected agent serves every pending HTTP-01 challenge, for any host, since the CA connects to whichever server DNS points at. They are an `AcmeChallenges` snapshot (`SNAPSHOT_KIND_ACME_CHALLENGES`), held in memory on both sides and sent again to an agent that connects. Before telling the CA to validate, the control plane waits up to 10 seconds for the connected agents to acknowledge the challenge. Running the ACME client on each agent was rejected: every agent would need its own account and state, and placements with several servers behind one name would race.
- **Reaching containers**: the agent publishes each app container's port on `127.0.0.1` with a host port the runtime picks, and the health check and the proxy (both in the agent process, on the host) connect there. It works the same on Linux and on Docker Desktop, where container IPs are not reachable from the host, and exposes nothing publicly. Connecting to container IPs on a Spinikus bridge network was rejected for that reason.
- **Services reach each other on their environment's network**: every container joins a bridge network per environment (`spinikus-<environment ID>`, created by the agent when first needed), where its service's slug is its DNS name, so an app reaches its database at `postgres:5432`. Environments can't reach each other, and nothing is published on the host for it. One network per server, with names like `<service>.<environment>.<project>`, was rejected because every environment could reach every other's databases; host ports were rejected because they differ between Linux and Docker Desktop and expose ports on the host. It only works between services on the same server until there is a network between servers. Renaming a service's slug changes its name from its next deployment, and networks without containers are not removed yet.
- **Secrets**: build-time and runtime variables travel inside commands over mTLS. The agent keeps them in memory or in the container environment only, and redacts them from logs.

## Workspace layout

Spinikus is a Cargo workspace of nine crates plus an `xtask` crate, built into one binary. The crate boundaries enforce the architecture decisions: the API and the web UI share only the control crate, the proxy knows nothing about the control plane, and the agent and control plane share only the protocol definitions.

&#91;embedded content: crate dependencies · 9 crates\]

The agent and the control plane never depend on each other; they share only the protocol crate.

### Layout

```text
spinikus/
├── Cargo.toml              # workspace, shared dependency versions and lints
├── rust-toolchain.toml     # pinned stable toolchain
├── deny.toml               # cargo-deny: licenses and advisories
├── xtask/                  # dev tasks: the stylesheet (`cargo xtask css`), the OpenAPI document (`cargo xtask openapi`), the user guide (`cargo xtask guide`), release archives (`cargo xtask release`)
├── install/install.sh      # installs and upgrades a server (control plane or agent) with systemd
├── docs/guide/             # the user guide (mdBook), published to GitHub Pages
├── .github/workflows/      # CI, releases on v* tags, and the guide's publishing
└── crates/
    ├── spinikus/           # the binary: CLI, client, wiring
    ├── spinikus-proto/
    ├── spinikus-core/
    ├── spinikus-store/
    ├── spinikus-control/
    ├── spinikus-api/
    ├── spinikus-web/
    ├── spinikus-proxy/
    └── spinikus-agent/
```

### Crates

| Crate               | Responsibility                                                                                                                                   | Depends on         |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------ |
| `spinikus-proto`    | `.proto` files and the generated tonic/prost code                                                                                                | Nothing internal   |
| `spinikus-core`     | Domain types, prefixed ULID IDs, the deployment state machine as pure logic, schemas for `spinikus.json` and `spinikus.toml`; no I/O             | Nothing internal   |
| `spinikus-store`    | Repository traits and the SQLite implementation, migrations, envelope encryption for secrets                                                     | core               |
| `spinikus-control`  | The control plane: service layer, reconciler, deployment orchestrator and job queue, agent gRPC server, internal CA and enrollment, event stream | core, store, proto |
| `spinikus-api`      | Axum JSON API, auth, OpenAPI spec                                                                                                                | control            |
| `spinikus-web`      | Maud templates, HTMX handlers, embedded static assets                                                                                            | control            |
| `spinikus-proxy`    | hyper and rustls proxy, route table, SNI resolver, ACME certificate manager                                                                      | core               |
| `spinikus-agent`    | Container runtime trait with the bollard implementation, builder (Railpack runner), command handlers, reporter, local persistence                | core, proto, proxy |
| `spinikus` (binary) | clap CLI with `server`, `agent` and client subcommands, the API client, and the wiring of the above                                              | All of the above   |

### Decided

- **Dependency rules**: `api` and `web` never depend on each other and meet only in `control`, so the compiler enforces the shared service layer. `core` has no I/O, so the state machine and config resolution can be tested exhaustively and fast. `proxy` knows only route and certificate types, so it can be tested on its own against local backends. `agent` and `control` do not depend on each other and share only `proto`, which is what makes loopback gRPC for the embedded agent work.
- **Granularity**: nine crates, enough to enforce these boundaries without much overhead for a solo project.
- **Slim agent builds**: Cargo features (`server` and `agent`, both on by default) are set up now, so an agent-only binary without Axum, SQLite and the web assets can be published later. The full binary ships first.
- **SQLite library**: sqlx, which is async, can check queries at compile time, and eases a later Postgres backend behind the repository traits.
- **API client**: lives as a module in the binary crate. If a Terraform provider or other tooling needs it, it gets extracted into `spinikus-client`.
- **Releases are static musl binaries on GitHub Releases**: a `v*` tag runs `.github/workflows/release.yml`, which builds `cargo xtask release --target <target> --tag <tag>` natively on GitHub's x86_64 and arm64 Linux runners and publishes `spinikus-<target>.tar.gz` for both, `SHA256SUMS` and `install.sh` as a release; tags with a suffix (`v0.2.0-rc.1`) are prereleases, which `releases/latest` skips. Every crate has the workspace's one version, and `--tag` must match it. musl makes one binary that runs on any distribution, which the "single static binary" promise needs; mimalloc replaces musl's allocator, which is slow with many threads. Binaries use the `dist` profile (thin LTO, one codegen unit, debug info stripped but symbols kept for backtraces). `cargo xtask release --docker` builds the same archive locally in `rust:<toolchain>-alpine` for testing an install before tagging. Cross-compiling with zig or `cross` was the alternative; native runners need neither, and the toolchain is pinned in `rust-toolchain.toml`. Signing the archives (minisign or Sigstore) and an agent-only build come later.
- **Servers are installed by one script with systemd**: `install.sh` (run as root, from `curl … | sudo sh`) installs the binary to `/usr/local/bin`, git, Docker through get.docker.com and Railpack through its own script when missing (each can be skipped), a `spinikus` system user in the `docker` group (which is as powerful as root, and is what running containers takes), `/etc/spinikus/spinikus.toml` (written only if missing) and `/var/lib/spinikus`, and a systemd unit: `spinikus` (`server run`, the control plane with its embedded agent) or, with `--join <url> --token … --ca-fingerprint …`, `spinikus-agent` (`agent run`, after `agent join` as the `spinikus` user). The unit binds ports 80 and 443 through `CAP_NET_BIND_SERVICE` rather than running as root, with `ProtectSystem=full`, `ProtectHome` and `PrivateTmp`. The archive is checked against `SHA256SUMS`. Running the script again upgrades: the binary is replaced and the service restarted, the config and data kept; an agent server is recognized by its unit. `--uninstall` removes the units and the binary and keeps the data. The whole script is one function called on its last line, so a download cut short runs nothing. The dashboard's join card shows the command for its own version's script. The binary finds its config through `--config`/`SPINIKUS_CONFIG_FILE` (the unit sets it), else `./spinikus.toml` (development), else `/etc/spinikus/spinikus.toml`, and stops on SIGTERM as on Ctrl+C, draining the same way. Generating the units from the binary (`spinikus install`) was considered; a shell script is what `curl | sh` needs anyway, and the units are short. A firewall and unattended upgrades are still to come. The dashboard listens on `0.0.0.0:8080` over plain HTTP until a dashboard domain is set (Settings → General), and the script says so.
- **Instance backups are one file encrypted with a passphrase**: `spinikus admin backup [file]` writes the database (a `VACUUM INTO` snapshot, so it runs while the server does, without applying migrations) and the master key, with a manifest (format, Spinikus version, schema version, master key ID), as a gzipped tar inside an [age](https://age-encryption.org) file encrypted to a passphrase (scrypt; at least 12 characters, prompted or from `SPINIKUS_BACKUP_PASSPHRASE`), so the file can be stored off the server and still be opened by hand with `age -d`. `spinikus admin restore <file>` needs the server stopped (`server run` holds a lock on `<data_dir>/spinikus.lock`), refuses backups whose schema is newer than the binary knows, checks the database's integrity and that the key opens its data keys, then swaps them in, keeping the replaced database and key as `*.before-restore-<time>`, and migrates. Both may run as root and give the files they leave in the data directory to its owner. Leaving the key out by default (backed up separately) was rejected because a backup without its key restores nothing, and including it unencrypted makes the file as sensitive as the key. Deployment logs, the agent's own files (the embedded agent re-enrolls) and `spinikus.toml` are not in the backup.
- **Scheduled instance backups are made by the control plane, with a stored passphrase**: instance admins set one schedule (Settings → Backups, or `PUT /instance-backup-schedule`): a storage destination, a cron expression, how many to keep, and the passphrase, sealed in the database like other secrets and write-only. Storing it is safe because a backup file needs only the passphrase to be opened, not the database it came from; the alternative, an age public key whose private half the admin keeps, was rejected as one more thing to lose and explain. The control plane makes the file itself, in the format of `spinikus admin backup`, in the data directory, and uploads it in parts to `<prefix>instance/<ID>.tar.gz.age`; one runs at a time, a backup interrupted by a restart fails when the control plane starts again, and retention deletes only the schedule's own backups, never ones made by hand. Restoring is still `spinikus admin restore` on the server, with the file downloaded from the bucket by any S3 client: a restore replaces the database the dashboard runs on, so it can't be started from the dashboard.
- **A setup code guards the first admin**: an installed instance answers before anyone has set it up, so until a user exists `server run` keeps a random code in `<data_dir>/setup-code` (mode 0600, made on the first start, kept across restarts, removed once there is a user), and the setup page and `POST /api/v1/setup` need it (`setup_code`; `GET /api/v1/setup` says whether with `setup_code_required`). The install script prints the setup URL with the code. The code is never logged, only the file's path. `spinikus admin create-user` on the server needs no code, since reading the database already takes the instance's user. Binding the dashboard to loopback until setup, or having the script prompt for the first admin, were the alternatives; the first breaks reaching a new VPS from a browser and the second breaks unattended installs.
- **Tooling**: Rust edition 2024, shared `[workspace.dependencies]` and `[workspace.lints]`, rustfmt and clippy in CI, cargo-nextest for tests, cargo-deny for licenses and advisories, and an `xtask` crate for dev chores instead of shell scripts. End-to-end tests that need Docker can become a separate dev-only crate later.

## API and CLI

The API covers everything the UI does, and the CLI is a client of it. The public API is JSON over HTTP under `/api/v1`. The web UI, the CLI and inbound Git webhooks all reach the same service layer in the control crate.

&#91;embedded content: entry points · 3 clients, 1 service layer\]

Whatever the entry point, the same services enforce the same rules, so the UI never has capabilities the API lacks.

### API

JSON over HTTP under `/api/v1`. The OpenAPI 3.1 spec is generated from the code (utoipa: `#[utoipa::path]` on each handler, `ToSchema` derived next to `Serialize` on the core types behind their `openapi` feature, and the routes registered through utoipa-axum's `OpenApiRouter`, so a route can't exist without its operation). It is served unauthenticated at `/api/v1/openapi.json` and committed as `docs/openapi.json`, which a test keeps current. The API's tests check every response they get against it. Changes within `v1` are additive only.

| Group          | Main routes                                                                                       | Notes                                                                              |
| -------------- | ------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------- |
| Hierarchy      | `projects`, `projects/{id}/environments`, `environments/{id}/services`, then flat `services/{id}` | Flat after creation, since IDs are globally unique                                 |
| Deployments    | `services/{id}/deployments` (list, trigger), `deployments/{id}`                                   | Includes the config snapshot and event history                                     |
| Actions        | `POST deployments/{id}/cancel`, `POST services/{id}/rollback`, `POST services/{id}/restart`, `/stop`, `/start` | Actions return the new or affected resource; a rollback returns the new deployment |
| Config         | `variables`, `domains`, `volumes`                                                                 | Scoped by project, environment or service                                          |
| Infrastructure | `servers`, `servers/{id}/enrollment-tokens`                                                       |                                                                                    |
| Databases      | `services/{id}/backups`, `backup-schedules`, `storage-destinations`                               |                                                                                    |
| Access         | `users`, `memberships`, `tokens`, `audit-log`, `settings`                                         |                                                                                    |

**Realtime**: SSE for deployment events (state transitions and build logs), dashboard status and runtime log tails (served through an agent session). The web terminal is the one WebSocket, since it needs to be bidirectional: `GET /api/v1/services/{id}/terminal` (bearer token; `deployment_id`, `rows`, `columns`, and `command` as a JSON array) for clients such as `spinikus shell`, and `/services/{id}/terminal/socket` for the dashboard (session cookie; `Origin` must be this site, since browsers send cookies with any site's WebSocket and there is no CSRF token on a handshake). Binary messages are the terminal's bytes both ways; text messages are JSON (`spinikus_core::terminal`): `{"type":"resize","rows","columns"}` from the client, and `{"type":"exit","code"}` or `{"type":"error","message"}` from the server before it closes. The API refuses before upgrading (a problem detail the CLI can show); the dashboard upgrades first and sends the refusal as an `error` message, which a browser can show where a failed handshake says nothing. Only environment admins may open one, since it runs anything as the container's user, and each is audited (`service.terminal`, with the deployment and the program, not the rest of the command, which may hold secrets). The dashboard draws it with vendored xterm.js on a page loaded in full rather than boosted; xterm.js sets inline styles as it draws, so that page alone allows `'unsafe-inline'` styles (still no inline scripts).

**Inbound webhooks** live outside `/api/v1`, at `/hooks/git/{provider}/{id}`. They verify signatures and trigger deployments.

#### Decided

- **Webhooks are per service for now**: a service with a Git source turns on deploying on push and gets a URL (`/hooks/git/{github|gitlab|gitea}/whk_…`; Forgejo uses Gitea's) and a secret, shown once, to paste into the repository's webhook settings. The dashboard turns it on in the service's settings (Deploy on push), where it lists the URL for each provider with the events to send, push and pull requests, and the last delivery's outcome, which is where to look when a push deployed nothing. A monorepo with several services has one webhook per service. One webhook per repository, fanning out to every matching service, was considered and left for Git provider connections, which can register webhooks themselves; matching repository URLs across https, ssh and scp forms by hand was the deciding cost.
- **Signatures**: GitHub's `X-Hub-Signature-256` and Gitea's `X-Gitea-Signature` (HMAC-SHA256 of the body), and GitLab's `X-Gitlab-Token` (the secret itself). A delivery that doesn't match is a 401 and deploys nothing.
- **A push deploys its own commit**, not the branch head when the build starts, so a delivery that arrives late builds what was pushed. Deliveries for a commit already in progress or live deploy nothing, since providers deliver again on timeouts.
- **Monorepo services deploy only when their root directory changed**, judged by the files the payload lists. When the list may be incomplete (forced pushes, new branches, more commits than the payload lists), the push deploys anyway. Changes to shared code outside the root directory don't deploy it; a list of extra paths to watch can come later.
- **Previews of pull requests are environments copied from their base**: an environment with `previews` on (set by project admins, since previews are environments of the project; `PATCH /environments/{id}` with `previews`, never on a preview) gets, for each pull request to the branch its webhook's Git service deploys, an environment of kind `preview` named `PR #<n>`, slug `pr-<n>`, that records its `base_environment_id`. It holds a copy of every service of the base: settings, placement, source, deploy key, variables (and the environment's), and its volumes, new and empty; not domains, webhooks or backup schedules, so a preview answers at its generated hostnames (`web-pr-7-shop.<base domain>`) and never on the base's domains. Copies of Git services of the same repository (the URL as written) build the pull request's head branch and commit; the rest deploy what they do in the base, so a preview's database is a fresh one with the same credentials, which references in its apps' variables resolve to inside the preview. New commits redeploy the copies of that repository's services at the head commit, and closing or merging the pull request deletes the preview's services and the environment, and the runner then removes their containers, images and, unlike other deleted services, their volumes, whose data has no other use (`RemoveDeployment.volumes`, sent with a service's last deployment on a server once the others there are cleaned up, so no container still mounts them). Pull requests from forks get no preview, since their code would run with the environment's secrets; a setting to allow them can come later. Events arrive through the services' webhooks (GitHub's and Gitea's `pull_request`, GitLab's `Merge Request Hook`), which must also send pull request events; since every service's webhook gets the same event, previews are made and removed one at a time and a commit already being deployed to a copy is skipped. A project admin can delete a preview by hand, services and all; the next commit makes it again. Copying the environment rather than deploying a branch next to the base's services was chosen so a preview has its own database and variables and can't change the base's data; one environment per pull request is what the data model planned. Members of only the base environment don't get the preview; project members do. Comments on the pull request with the URL need Git provider connections and come with them.
- **Deployment streams** (`GET /deployments/{id}/stream`) are SSE: `log` events carry one line of the deployment log each, with the line's end offset in the file as the event ID, `status` events carry `{status, reason, message}` when the status changes (starting with the current one, before the lines that follow it; a final status comes after the deployment's last lines), and `end` carries the final status once the deployment is live or finished. The log file is the source of truth: a reconnect with `Last-Event-ID` resumes after that offset, and followers are woken when lines are appended. The status is checked every half second, rather than published by the runner, which keeps the runner unaware of followers. WebSocket was rejected, as everywhere but the terminal.
- **Runtime logs** (`GET /services/{id}/logs?deployment_id=&since=&tail=&follow=`) are SSE too: `log` events carry `{at, stream, text}` and `end` carries `{error}`. They read the live deployment's container unless a deployment is named, any viewer can read them, and they need the server's agent to be connected. `tail` defaults to 100 lines (0 means all, at most 10,000), so a busy app's history isn't sent by accident.
- **Every delivery's outcome is kept on the webhook** (`last_delivery`): deployed, a preview deployed or removed, ignored and why, refused, or failed. That's the first place to look when a push didn't deploy.

### CLI

The CLI is the same `spinikus` binary as the server.

```text
# running Spinikus
spinikus server run | migrate | backup | restore | check-config
spinikus admin create-user          # bootstrap the first admin
spinikus agent run | join <url> --join-token <t>

# talking to a Spinikus instance
spinikus login | logout | whoami | context list|use|remove
spinikus link [project/environment[/service]]   # remember what this directory deploys
spinikus project|environment|service list|create|show|delete
spinikus deploy [service]           # waits and streams build logs
spinikus rollback [service] [--to <deployment>]
spinikus restart [service]          # the running container, in place
spinikus stop|start [service]       # stop it until started or deployed again
spinikus deployment list|show|logs|cancel
spinikus logs [service] -f
spinikus shell [service] [--deployment <id>] [-- command…]   # a terminal in the running container
spinikus env list|set|unset|import|export     # variables
spinikus domain list|add|remove
spinikus volume list|add|update|remove|backup|backups|restore
spinikus db create|backup|backups|restore
spinikus token list|create|revoke
```

- **Global flags**: `--server`, `--token`, `--context`, `-o table|json`, `--quiet` and `--no-color`. `SPINIKUS_URL` and `SPINIKUS_TOKEN` work for CI.
- **Config**: named contexts (server URL plus token) in `~/.config/spinikus/`, with the token in the OS keychain if available, otherwise in a file with 0600 permissions.
- **`deploy`** waits by default and streams build logs, exiting non-zero on failure. `--detach` returns immediately.
- **Stable exit codes**, and JSON output that matches the API's shapes, so scripts do not have to parse tables.

### Decided

- **The public API is REST/JSON** with an OpenAPI spec, not gRPC. gRPC stays for the agent protocol.
- **The CLI client is hand-written**, in the binary crate. The OpenAPI spec is used for docs and contract tests, not for client generation.
- **The OpenAPI spec is code-first with utoipa**, not a hand-written YAML file. A hand-written spec drifts from the handlers unless something checks it, and the check would amount to generating it anyway. aide was the alternative: it infers more from axum's extractors, but our own extractors and response helpers (`tagged`, `created`) would have needed wrappers just the same, and utoipa is more widely used. What every operation shares (bearer auth, problem-detail errors) is added in one pass over the built document rather than repeated in each annotation.
- **The CLI knows which service you mean through flags plus a link file** created by `spinikus link`. It cannot be `spinikus.json`, which holds no hosting details.
- **Git-based deploys are MVP.** Deploying from a local directory is Later.
- **Errors** use RFC 9457 problem details, with a stable machine-readable `code` for every error.
- **Pagination** is cursor-based. ULIDs sort by creation time, so the cursor is just an ID.
- **Idempotency**: an `Idempotency-Key` header on POSTs that create deployments or other side effects.
- **Concurrent edits**: every mutable resource carries a revision, and updates use `If-Match`, so the UI and CLI cannot silently overwrite each other.
- **Secrets are write-only.** Reads return metadata only, and a separate reveal endpoint requires the right role and writes to the audit log.
- **The CLI is a plain API client**, with no access the API doesn't give: `spinikus login` exchanges an email and password for an API token (`POST /login`, named "spinikus CLI"), saved per context; `logout` revokes it, and signing in again replaces (and revokes) the old one. Client commands don't read `spinikus.toml`, so they work anywhere.
- **Tokens are kept in a file for now** (`~/.config/spinikus/credentials.toml`, mode 0600, apart from `config.toml`), not the OS keychain: a keychain needs a platform library on each OS (Secret Service over D-Bus on Linux, often absent on servers and in CI) and a fallback anyway. The keychain can come later behind the same `ConfigDir`.
- **Which instance a command uses**: `--context`, else `--server` (with `--token`, or the context for that server), else the context for the directory's link, else the current context. So a directory linked to one instance keeps using it after switching the current context.
- **The link file is `.spinikus/link.json`** (instance, project, environment and optionally service IDs), found by walking up from the current directory like `.git`, with a `.gitignore` of its own inside: the IDs belong to one instance, not to the code, so they stay out of the repository without touching the project's `.gitignore`. Names elsewhere are IDs, slug paths (`shop/production/web`), or slugs relative to the link (`web`, `production/web`).
- **Variables are `spinikus env`; environments are `spinikus environment`**: the short name went to the command used most, as in other platforms' CLIs. `env` acts on the linked service unless `-s`, `-e` or `-p` names a service, environment or project (without a value: the linked one). `env set NAME` alone reads the value without echoing it, or from stdin, so secrets stay out of shell history; `env export` writes plain values and leaves secrets as comments unless `--reveal` (audited, admins).
- **Output**: results on stdout, progress and deployment logs on stderr, so `spinikus deploy -o json > result.json` captures only the deployment. `-o json` prints the API's own JSON (lists as one array of every page's items); tables read only the fields they show, so new API fields never break an older CLI. YAML was left out: JSON covers scripts, and the YAML crates for serde are unmaintained.
- **Exit statuses are stable**: 1 any other failure, 2 usage, 3 not signed in or token rejected, 4 forbidden, 5 not found, 6 refused (invalid, conflict, stale revision), 7 a deployment, backup or restore that was waited for failed or was canceled.
- **`deploy` follows the deployment's SSE stream** (reconnecting with `Last-Event-ID`) and ends with where the app answers; `rollback`, `db create`, `db backup`, `db restore`, `volume backup` and `volume restore` wait the same way (backups and restores by polling), and `--detach` prints the ID and returns. Deleting asks for confirmation on a terminal and needs `--yes` without one.
- **`agent join` takes `--join-token`** (or `SPINIKUS_JOIN_TOKEN`), since `--token` is the API token on every client command.
- **Authentication**: a session cookie for the web UI (HttpOnly, SameSite, CSRF protection) and bearer API tokens for the CLI and automation. Tokens have a recognizable prefix, are shown once, are stored hashed, are scoped like memberships (role plus project or environment), and can expire. Users manage only their own (`GET`/`POST /tokens`, `DELETE /tokens/{id}`; revoked ones are not listed, and someone else's is not found). The scope must be a project or environment the user can see, and a token created with a token can't widen it: its role is at most that token's, and a scoped token can only create scoped ones within its scope. Instance admins may also revoke anyone's token (`DELETE /tokens/{id}`) and list a user's (`GET /users/{id}/tokens`).
- **Failed sign-ins are throttled in memory, by email and address together**: 5 wrong passwords for one email from one address, or 50 from one address overall, within 15 minutes, and that address is refused, without checking the password, until the 15 minutes from its first failure are up (`too_many_attempts`, `429` with `Retry-After`); a success clears that email's count. Locking the account itself was rejected because anyone could then lock an admin out; counts in the database were rejected as more writes for no gain on a single control plane, at the cost of a restart clearing them. `docs/security.md` has the security model and the review.
- **Users are managed by instance admins** (`/users`: list, create, edit name, email and admin flag, disable and enable, reset the password). There are no invitations or email yet: an admin sets an initial password (the same policy as setup, at least 12 characters) and gives it to the person, who changes it with `PUT /me/password`, giving the current one. Changing your own password ends your other sessions and keeps the one in use; an admin's reset ends all of them. Disabling a user ends their sessions, their API tokens stop working, and their memberships and history stay; nobody can disable themselves, and the last instance admin can't lose the flag. Users are never deleted, since audit entries and deployments name them.
- **Members are managed by the scope's admins** (`/projects/{id}/members`, `/environments/{id}/members`, `/memberships/{id}`): a project's admins (and so instance admins) manage its and its environments' members, an environment's admins that environment's. Members are added by email, so the user must exist and be active; a user has at most one membership per scope, and changing it is an update of its role, not a second membership. Project members have their role in every environment; an environment membership can only add to it.
- **The audit log is read by admins**: `GET /audit-log` gives instance admins every entry and a project's admins that project's (`?project_id=`), newest first (`?before=`). Since a project admin can't look up arbitrary users, the response carries the users who acted (`users`), so a client can name them. Entries are never changed or removed.
