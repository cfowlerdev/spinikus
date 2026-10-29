# Spinikus

## Crates

| Crate             | Responsibility                                                                                                                                   | Depends On         |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------ |
| spinikus-proto    | .proto files and thew generated cpde                                                                                                             | Nothing internal   |
| spinikus-core     | Domain types, prefixed ULID IDs, the deployment state machine as pure logic, schemas for spinikus.json and spinikus.toml; no I/O                 | Nothing internal   |
| spinikus-store    | Repository traits and the SQLite implementation, migrations, envelope encryption for secrets                                                     | core               |
| spinikus-control  | The control plane: service layer, reconciler, deployment orchestrator and job queue, agent gRPC server, internal CA and enrollment, event stream | core, store, proto |
| spinikus-api      | Axum JSON API, auth, OpenAPI spec                                                                                                                | control            |
| spinikus-web      | Maud templates, HTMX handlers, embedded static assets                                                                                            | control            |
| spinikus-proxy    | hyper and rustls proxy, route table, SNI resolver, ACME certificate manager                                                                      | core               |
| spinikus-agent    | Container runtime trait with the bollard implementation, builder (Railpack runner), command handlers, reporter, local persistence                | core, proto, proxy |
| spinikus (binary) | clap CLI with server, agent and client subcommands, the API client, and the wiring of the above                                                  | All of the above   |
