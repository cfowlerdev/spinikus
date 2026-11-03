# Configuration file

Almost everything about Spinikus is set in the dashboard, the CLI or the API and kept in its
database. The configuration file holds only what Spinikus needs before it can read its
database: where its data is, where it listens, and where its master key is.

The install script writes it to `/etc/spinikus/spinikus.toml`. After changing it, restart
Spinikus: `sudo systemctl restart spinikus` (or `spinikus-agent` on an agent's server).

## A control plane's file

```toml
data_dir = "/var/lib/spinikus"

[logging]
level = "info"

[http]
listen = "0.0.0.0:8080"

[agents]
listen = "0.0.0.0:8443"

[proxy]
listen = "0.0.0.0:80"
https_listen = "0.0.0.0:443"
```

## Every setting

| Setting                     | Default                      | Meaning                                                               |
| --------------------------- | ---------------------------- | --------------------------------------------------------------------- |
| `data_dir`                  | `data`                       | Where Spinikus keeps its database, keys, logs and the agent's state   |
| `[logging] level`           | `info`                       | How much to log: `error`, `warn`, `info`, `debug`, or a filter such as `info,spinikus=debug` |
| `[logging] json`            | `false`                      | Log one JSON object per line                                          |
| `[database] path`           | `<data_dir>/spinikus.db`     | The SQLite database file                                              |
| `[secrets] master_key_file` | `<data_dir>/master.key`      | The file holding the master key, generated on first start            |
| `[secrets] master_key`      | none                         | The master key itself, base64; meant for an environment variable     |
| `[http] listen`             | `127.0.0.1:8080`             | The dashboard and the API (the install script sets `0.0.0.0:8080`)   |
| `[agents] listen`           | `0.0.0.0:8443`               | Where agents of other servers connect                                 |
| `[agents] embedded`         | `true`                       | Run this server's own agent inside the control plane                  |
| `[proxy] enabled`           | `true`                       | Serve apps on this server                                             |
| `[proxy] listen`            | `0.0.0.0:80`                 | The proxy's HTTP address                                              |
| `[proxy] https_listen`      | `0.0.0.0:443`                | The proxy's HTTPS address                                             |

Relative paths are relative to `data_dir`.

## Environment variables

Every setting can also come from an environment variable: `SPINIKUS_` followed by the
setting's path, with `__` between a table and its key, such as `SPINIKUS_LOGGING__LEVEL=debug`
or `SPINIKUS_HTTP__LISTEN=127.0.0.1:8080`. They override the file. `RUST_LOG` overrides the
log level too.

## The master key

The master key encrypts every secret in the database: variables, deploy keys, storage
credentials and certificates' keys. It is kept outside the database, in
`/var/lib/spinikus/master.key`, readable only by the `spinikus` user. Without it, the secrets
in the database can't be read, so:

- back it up with the database ([Backing up Spinikus](instance-backups.md) does both);
- never change or delete it. Spinikus refuses to start if the database's secrets were
  encrypted with a different key.

To keep the key out of the data directory, move the file elsewhere and set
`[secrets] master_key_file`, or give the key in `SPINIKUS_SECRETS__MASTER_KEY` (for example
from your secret manager through the systemd unit).
