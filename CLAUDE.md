# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project status

Spinikus is a Rust, self-hosted PaaS (a lighter Coolify/Dokploy, "self-hosted Vercel"). It is early: `spinikus-core` (domain types, IDs, deployment state machine) and `spinikus-store` (SQLite repositories, migrations, envelope encryption) cover the data model in the design doc, except backups and integrations (storage destinations, notification channels, Git connections), which come with their features, and the `spinikus` binary has the CLI, config and database startup. The other crates are still `cargo new` placeholders. The design is in `docs/spinikus.md` — read it before building any feature; it records decisions (with the alternatives that were rejected) that the code is expected to follow.

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
cargo run -- server run              # start the control plane (connects to SQLite, applies migrations)
cargo run -- server migrate          # apply pending migrations and exit
cargo clippy --workspace && cargo fmt
```

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
