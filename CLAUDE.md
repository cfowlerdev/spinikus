# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project status

Spinikus is a Rust, self-hosted PaaS (a lighter Coolify/Dokploy, "self-hosted Vercel"). It is early: `spinikus-core` (domain types, IDs, deployment state machine) and `spinikus-store` (SQLite repositories, migrations, envelope encryption) cover the data model in the design doc, except backups and integrations (storage destinations, notification channels, Git connections), which come with their features, `spinikus-control` has the start of the service layer (`Control`: first-run setup, sign-in, sessions and API token authentication, `ControlError` with stable codes, projects, environments and services with permission checks and audit entries, servers with join tokens, the internal CA (`ca.rs`) and agent enrollment, and the agent listener (`agents/`: mutual TLS, `Enroll`, `RenewCertificate`, `Connect`)), `spinikus-api` serves the JSON API under `/api/v1` (setup, login for an API token, `me`, projects, environments, services, servers and their enrollment tokens), `spinikus-proto` defines the agent protocol (`proto/spinikus/agent/v1/agent.proto`, compiled by `protox` in `build.rs`, so no `protoc` is needed), `spinikus-agent` enrolls (`join`, pinning the CA fingerprint), keeps the control stream open (`run`: hello, heartbeats, reconnect with backoff, certificate renewal), reports the containers Spinikus manages through the container runtime (Docker via bollard), and runs `StopDeployment` and `RestartInstance`, with no builder or proxy yet, and the `spinikus` binary has the CLI, config and startup. `spinikus-web` and `spinikus-proxy` are still `cargo new` placeholders. The design is in `docs/spinikus.md` — read it before building any feature; it records decisions (with the alternatives that were rejected) that the code is expected to follow.

### Service layer conventions

`spinikus-control`'s `Control` is the one entry point for the API, web UI, CLI and webhooks: input validation, permission checks (via `Principal::role_in` / `is_instance_admin`) and audit entries (`Control::audit`) happen there, not in the store or the HTTP layers. Errors are `ControlError`; never expose `Internal` details to clients.

- Load access with `Control::access(principal)` and check it with `Access::check_project` / `check_environment`. Something the principal has no role in is `NotFound` (existence is not revealed); something they can see without the needed role is `Forbidden`.
- Roles: instance admins create projects; project admins update and delete them and create and delete environments; environment admins rename environments; developers create and edit services; admins delete them; viewers read.
- Operations take raw input (`NewProject`, `ProjectChanges`, …), validate it (`input::name`, `input::slug`, which derives a slug from the name when none is given), and record an audit entry with `Control::audit_by` (`audit::changed` lists updated fields, never values).
- Lists return `Page<T>` (`has_more`; the next cursor is the last item's ID), with limits clamped to `page::MAX_PAGE_SIZE`.

### Agent connections

`spinikus-control::agents` implements `AgentService` as `AgentGrpc`, served by `agents::serve` over TLS with optional client certificates. The internal CA (`ca.rs`, created on first use and stored in `certificate_authority`, its key sealed under the master key) issues the listener's certificate on every start, for the fixed name `CONTROL_PLANE_SERVER_NAME`, and agents' client certificates, whose subject CN is the server ID (only the CSR's public key is used). The `identify` interceptor turns a verified client certificate into an `AgentIdentity` request extension; that is the only source of an agent's server ID, never IDs inside messages. Without one only `Enroll` works, which takes a single-use join token (`spj_…`, 1 hour); a malformed CSR is rejected before the token is used up. Revoking a server disconnects its agent and refuses its reconnects, renewals and unused join tokens. Tests in `agents/tests.rs` set the identity with an interceptor over plain gRPC; `agents/tls_tests.rs` go through the real TLS listener. `Control::agents()` is the `AgentHub`: the connected agents and a channel to each control stream (`send`, `disconnect`). Connection starts and ends hold `AgentHub::transition()` across the registry change and the database write, so a reconnect is never overwritten by the old connection's disconnect. Proto-to-core conversions live in `agents/convert.rs`, on the control side, never in `spinikus-proto`.

### Agent side

`spinikus-agent` keeps its state in `<data_dir>/agent` (`agent.json`: URL, server ID, CA; `identity.json`: certificate and key, mode 0600), each file replaced atomically. Joining first does a TLS handshake that accepts the control plane only if its chain contains a CA with the pinned fingerprint that signed the listener's certificate (so the listener sends the CA in its chain), then enrolls over normal TLS with that CA as the only root. The agent always verifies the control plane as `CONTROL_PLANE_SERVER_NAME`. `run` returns an error only for what retrying cannot fix (not enrolled, revoked, certificate expired, unsupported protocol); everything else reconnects with `Backoff`. The container runtime is the `runtime::ContainerRuntime` trait: `DockerRuntime` (bollard; `DOCKER_HOST`, else `/var/run/docker.sock`, else Docker Desktop's `~/.docker/run/docker.sock`, connecting lazily so a stopped daemon is `RuntimeError::Unavailable` rather than fatal) and `MemoryRuntime` for tests (behind the `test-support` feature, which the binary's dev-dependencies enable). Managed containers are those labeled `spinikus.service` and `spinikus.deployment` (`SERVICE_LABEL`, `DEPLOYMENT_LABEL`). The reporter (`report.rs`) sends a full `StateReport` after `Welcome` only from a successful listing (a full report replaces the server's instances, so never send one while the runtime is unreachable), then a delta per runtime event, and resyncs with a new full report after the runtime fails. Commands (`commands.rs`) run in their own tasks, outlive reconnects (results go out on the next connection; running IDs go in `Hello`), and are idempotent by ID: a running ID is ignored, a finished one gets its stored result. They are kept in memory only so far. Commands the agent cannot run yet get a failed `CommandResult` right away. Tests that need Docker are `#[ignore]`d (`cargo test -p spinikus-agent --test docker -- --ignored`). The embedded agent (`crates/spinikus/src/embedded.rs`) is this machine's agent on single-server installs: `server run` gets or creates the one `embedded` server, enrolls it with a system-issued join token through the real `join` over loopback (re-enrolling if its files are for another server or CA), and runs it in-process over loopback mTLS; it does not run if its server is revoked. On shutdown the agent listener closes agents' streams (`AgentHub::disconnect_all`) so it can drain, with a 5-second grace period, and on start it marks every server disconnected. `AgentHub::disconnect` only signals the connection to stop; its reader unregisters it and records the disconnect. Tests that need both a control plane and an agent live in `crates/spinikus/tests/` (shared setup in `tests/common`), since only the binary may depend on both.

### API conventions

`spinikus-api` handlers stay thin: extract, call one `Control` method, shape the response. Use the crate's own extractors (`Body`, `Path`, `Query`, `Authenticated`, `IfMatch`, `Client` in `extract.rs`) rather than axum's, so every rejection is a problem detail (`ApiError`, `application/problem+json` with a stable `code`). Request bodies are API-specific structs with `deny_unknown_fields`; responses serialize the core types. Single resources return their revision as `ETag` (`response::tagged`/`created`), updates require it in `If-Match` (428 without, 412 if stale), and lists use `PageParams`/`PageBody` (`?after=&limit=`, `next_cursor`). Authentication is bearer API tokens only; session cookies belong to the web UI. Tests in `crates/spinikus-api/tests/` drive the router with `tower::ServiceExt::oneshot`.

### Store conventions

Each entity gets a repository trait in `spinikus-store` (methods named after the entity, e.g. `create_project`, since `SqliteStore` implements them all) with:
- soft delete (`deleted_at`) for projects, environments, services and domains, with slugs and hostnames unique only among non-deleted rows; creating under a deleted parent is `NotFound`, deleting a parent with active children is `HasChildren`;
- a `revision` column bumped on every user-facing update, and updates that take the expected revision (`RevisionMismatch` otherwise);
- cursor pagination by ID; IDs come from a process-wide monotonic ULID generator, so they sort in creation order;
- multi-statement writes in `SqliteStore::begin_write` (`BEGIN IMMEDIATE`), and timestamps written from Rust via `db::format_timestamp`;
- SQL as `&'static str` only (sqlx 0.9 rejects dynamic strings): when a query varies, build each variant with `concat!` and a small macro (see `columns!`, `insert_under!` in `variables.rs`) rather than `format!`;
- input validation (slugs aside) left to the service layer; the store keeps rows consistent with `CHECK` constraints and unique indexes.
- secrets sealed with `SqliteStore::keyring()` (`secrets.rs`): envelope encryption with XChaCha20-Poly1305, data keys in `data_keys` wrapped by the master key. Store the `Sealed` key ID next to the ciphertext, and pass the owning row's ID as the `context` so a ciphertext can't be moved to another row.

## Commands

```sh
cargo build                          # build the whole workspace
cargo check --workspace
cargo test --workspace               # all tests
cargo test -p spinikus-core          # one crate
cargo test -p spinikus-core it_works # one test by name filter
cargo run -- server run              # start the control plane (applies migrations, serves the API on [http] listen, default 127.0.0.1:8080, agents on [agents] listen, default 0.0.0.0:8443, and the embedded agent unless [agents] embedded = false)
cargo run -- server migrate          # apply pending migrations and exit
cargo run -- admin create-user --email you@example.com --name You   # first admin; password from SPINIKUS_ADMIN_PASSWORD or a prompt
SPINIKUS_JOIN_TOKEN=spj_… cargo run -- agent join https://host:8443 --ca-fingerprint sha256:…   # enroll this machine's agent (token and fingerprint from POST /api/v1/servers/{id}/enrollment-tokens)
cargo run -- agent run               # run the agent; state in <data_dir>/agent
cargo clippy --workspace && cargo fmt
```

Formatting is plain `cargo fmt` on stable, configured by `rustfmt.toml` (stable options only; don't add nightly-only ones such as `trailing_comma`).

The binary loads `spinikus.toml` from the **current working directory** (required), overlaid by env vars with prefix `SPINIKUS` and `__` as the nesting separator (e.g. `SPINIKUS_LOGGING__LEVEL=info`). `RUST_LOG` overrides `logging.level` when set. Run it from the repo root. The database defaults to `<data_dir>/spinikus.db` (`data/` in dev, gitignored). The master encryption key comes from `SPINIKUS_SECRETS__MASTER_KEY` or `[secrets] master_key_file`; with neither, `<data_dir>/master.key` is generated on first start. `server run` refuses to start if the database's data keys were wrapped by a different master key.

Migrations live in `crates/spinikus-store/migrations/` as `NNNN_<name>.sql`, are embedded with `sqlx::migrate!`, and run automatically on startup. Tables are `STRICT`; timestamps are `TEXT` in UTC RFC 3339 with milliseconds (`strftime('%Y-%m-%dT%H:%M:%fZ', 'now')`).

Planned but not yet present (per the design doc): `rust-toolchain.toml`, `deny.toml`, `[workspace.lints]`, cargo-nextest, and an `xtask` crate for OpenAPI generation, asset builds and releases — use `xtask` rather than shell scripts for dev chores once it exists.

## Architecture

One binary, two runtime roles:
- **Control plane** owns state and decisions: service layer, reconciler (desired state in DB vs. state reported by agents), durable deployment state machine, job queue, internal CA/enrollment, SSE event stream.
- **Agent** owns containers, builds and traffic on one server: runtime driver (bollard), builder (Dockerfile/Compose via runtime, Railpack for auto-detect), hyper+rustls proxy with ACME, reporter.

Agents **dial out** to the control plane over a long-lived mTLS gRPC stream (`spinikus.agent.v1`, tonic/prost); the control plane never calls into an agent. On single-server installs the agent runs in-process but still connects over loopback gRPC — there is one code path, don't add an in-process shortcut.

### Crate dependency rules (enforced by crate boundaries — keep them)

| Crate | Role | Internal deps |
| --- | --- | --- |
| `spinikus-proto` | `.proto` + generated code | none |
| `spinikus-core` | domain types, prefixed ULID IDs, deployment state machine as pure logic, `spinikus.json`/`spinikus.toml` schemas; **no I/O** | none |
| `spinikus-store` | repository traits, SQLite (sqlx) impl, migrations, envelope encryption | core |
| `spinikus-control` | control plane (see above) | core, store, proto |
| `spinikus-api` | Axum JSON API under `/api/v1`, auth, OpenAPI | control |
| `spinikus-web` | Maud + HTMX (SSE) UI, embedded vendored assets | control |
| `spinikus-proxy` | proxy, route table, SNI resolver, ACME | core |
| `spinikus-agent` | runtime trait, builder, command handlers, local persistence | core, proto, proxy |
| `spinikus` | binary: clap CLI, hand-written API client, wiring | all |

- `api` and `web` never depend on each other; both are thin layers over the shared service layer in `control`, so the UI can never do something the API can't.
- `agent` and `control` never depend on each other; they share only `proto`.
- `proxy` knows only route/certificate types.
- Planned Cargo features `server` and `agent` (both default) to allow a slim agent-only build later.

Internal crates are referenced via `[workspace.dependencies]` (`spinikus-core.workspace = true`).

### Key design invariants

- `spinikus.toml` is instance config only (data dir/DB, listen addresses, log level, master key source). Everything else lives in the database and is managed via API/UI.
- Per-repo `spinikus.json` holds only settings that change with the code version (build, runtime, health check, env var *names*, cron, monorepo `apps`). Never domains, secret values, placement or credentials. When it sets a field, the file wins over the DB.
- Desired state and observed state live in separate tables; agents write only observed state.
- Deployments are immutable records with a config snapshot. States: `queued → preparing → releasing → starting → verifying → live → draining → superseded`, plus terminal `failed` (with reason code) and `canceled`. Each transition writes a deployment-event row in the same transaction. Rollback = new deployment reusing an old image + snapshot, starting at `starting`.
- IDs are prefixed ULIDs (`svc_…`, `srv_…`); cursor pagination uses the ID.
- Secrets: envelope-encrypted with a key ID per value; master key lives outside the DB. API secrets are write-only.
- API conventions: RFC 9457 problem details with stable `code`, `Idempotency-Key` on side-effecting POSTs, `If-Match` revisions on updates, additive-only changes within `v1`.
