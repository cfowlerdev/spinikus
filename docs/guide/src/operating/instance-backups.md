# Backing up Spinikus

Spinikus keeps its own state (users, projects, services, settings, variables and the keys
that encrypt secrets) in a SQLite database in `/var/lib/spinikus`. Back it up, so you can
rebuild the control plane if its server is lost. These backups don't include your apps'
databases (see [Backups and restores](../data/backups.md)) or volumes.

A backup is one file holding the database and the **master key** that encrypts the secrets
in it, encrypted with a **passphrase** you choose (with [age](https://age-encryption.org)).
Without the passphrase, nobody can read it, including you: store the passphrase somewhere
other than the backups, such as a password manager.

## On a schedule

An instance admin sets it up under **Settings → Backups**:

1. Add a storage destination first, under **Settings → Storage** (see
   [Adding a storage destination](../data/backups.md#adding-a-storage-destination)).
2. Choose the destination, a cron expression in UTC (such as `0 2 * * *`), how many backups
   to keep, and a passphrase of at least 12 characters. Turn the schedule on and save.

Each backup is uploaded as `<path prefix>instance/<backup ID>.tar.gz.age`. **Back up now**
makes one at any time. The page lists the backups with their status, and older ones beyond
the number to keep are deleted.

The passphrase field is never filled in; leave it empty when changing other settings to keep
the current one.

## By hand

On the control plane's server:

```sh
sudo spinikus admin backup /root/spinikus-backup.tar.gz.age
```

It asks for a passphrase (or reads `SPINIKUS_BACKUP_PASSPHRASE`) and works while Spinikus
runs. Copy the file off the server.

## Restoring

Restore on the same server, or on a new one where you have installed Spinikus with the
install script:

1. Download the backup (with any S3 client for scheduled ones), and copy it to the server.
2. Stop Spinikus, restore, and start it again:

   ```sh
   sudo systemctl stop spinikus
   sudo spinikus admin restore spinikus-backup.tar.gz.age
   sudo systemctl start spinikus
   ```

The restore asks for the passphrase, and keeps the database and key it replaces beside them,
named `*.before-restore-<time>`, in case you need them back. A backup restores into the
version of Spinikus that made it or a newer one.

After restoring on a new server:

- point the dashboard's domain, and your apps' domains for apps on this server, at the new
  server;
- agents on other servers reconnect by themselves if they reach the control plane at the
  same address (they trust the same certificate authority, which is in the backup);
- the new server has none of the old one's containers, images or volumes: deploy each
  service that ran there again, and restore its databases from their own backups (they
  start empty).
