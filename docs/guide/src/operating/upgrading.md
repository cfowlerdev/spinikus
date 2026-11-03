# Upgrading and uninstalling

## Upgrading

Run the install script again, on the control plane's server first and then on each other
server:

```sh
curl -fsSL https://github.com/cfowlerdev/spinikus/releases/latest/download/install.sh | sudo sh
```

It replaces the binary and restarts the service, keeping the configuration and the data. On
start, Spinikus updates its database to the new version by itself. To install a particular
version, add `-s -- --version v<x.y.z>`.

[Back Spinikus up](instance-backups.md) before upgrading. A backup restores into the same
version or a newer one, so you can't go back to an older version with a backup made after
the upgrade.

While the control plane restarts, the dashboard and API are briefly unavailable, but your
apps keep serving: the proxy and the containers don't depend on it. Agents reconnect by
themselves.

## Uninstalling

```sh
curl -fsSL https://github.com/cfowlerdev/spinikus/releases/latest/download/install.sh \
    | sudo sh -s -- --uninstall
```

This stops and removes the Spinikus services and the binary. It keeps:

- `/etc/spinikus` (the configuration);
- `/var/lib/spinikus` (the database, the keys and the agent's identity);
- the containers, images and volumes Spinikus created, which keep running.

To remove everything, also stop and remove the containers (they are labeled
`spinikus.service`), their images and volumes with `docker`, and delete those two
directories. This can't be undone, so back up first.
