# Spinikus

A self-hosted PaaS: deploy apps from Git or images, with HTTPS, databases and backups, on your own servers.

## Install

On a Linux server with systemd (x86_64 or arm64), as root:

```sh
curl -fsSL https://github.com/cfowlerdev/spinikus/releases/latest/download/install.sh | sudo sh
```

It installs Docker, git and Railpack if they are missing, runs Spinikus as the `spinikus` systemd service, and prints the URL to create the first admin, with a one-time setup code. Run it again to upgrade. Other servers are added from the dashboard (Settings → Servers), which gives the command that installs and enrolls their agent. See the comments at the top of [`install/install.sh`](install/install.sh) for its options.

### Back up and restore

`spinikus admin backup` writes the database and the master key to one file encrypted with a passphrase (it asks for one, or reads `SPINIKUS_BACKUP_PASSPHRASE`). It works while Spinikus runs:

```sh
sudo spinikus admin backup /root/spinikus-backup.tar.gz.age
```

Keep the file and the passphrase somewhere other than the server. To restore, on the same or a new server with Spinikus installed:

```sh
sudo systemctl stop spinikus
sudo spinikus admin restore spinikus-backup.tar.gz.age
sudo systemctl start spinikus
```

The restore keeps the database and key it replaces beside them, as `*.before-restore-<time>`. A backup restores into the version of Spinikus that made it or a newer one.

## Develop

```sh
cargo run -- server run   # with ./spinikus.toml; the dashboard is on http://127.0.0.1:8080
cargo test --workspace
```

`CLAUDE.md` describes the code, and `docs/spinikus.md` the design and its decisions.

## Crates

| Crate             | Responsibility                                                                                                                                   | Depends On         |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------ |
| spinikus-proto    | .proto files and the generated code                                                                                                             | Nothing internal   |
| spinikus-core     | Domain types, prefixed ULID IDs, the deployment state machine as pure logic, schemas for spinikus.json and spinikus.toml; no I/O                 | Nothing internal   |
| spinikus-store    | Repository traits and the SQLite implementation, migrations, envelope encryption for secrets                                                     | core               |
| spinikus-control  | The control plane: service layer, reconciler, deployment orchestrator and job queue, agent gRPC server, internal CA and enrollment, event stream | core, store, proto |
| spinikus-api      | Axum JSON API, auth, OpenAPI spec                                                                                                                | control            |
| spinikus-web      | Maud templates, HTMX handlers, embedded static assets                                                                                            | control            |
| spinikus-proxy    | hyper and rustls proxy, route table, SNI resolver, ACME certificate manager                                                                      | core               |
| spinikus-agent    | Container runtime trait with the bollard implementation, builder (Railpack runner), command handlers, reporter, local persistence                | core, proto, proxy |
| spinikus (binary) | clap CLI with server, agent and client subcommands, the API client, and the wiring of the above                                                  | All of the above   |
