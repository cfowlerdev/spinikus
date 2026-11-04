# Backups and restores

Spinikus backs databases, and the files in apps' [volumes](../apps/volumes.md), up to
S3-compatible object storage: Amazon S3, Cloudflare R2, Backblaze B2, MinIO, RustFS and
others. Keep backups with a provider other than the one your server runs on, so one outage
can't take both.

## Adding a storage destination

An instance admin adds one under **Settings → Storage**:

| Field                 | What to enter                                                                  |
| --------------------- | ------------------------------------------------------------------------------ |
| Name                  | Anything, such as `r2-backups`                                                 |
| Endpoint              | The S3 API's URL, such as `https://<account>.r2.cloudflarestorage.com`; empty for AWS |
| Region                | The bucket's region; `us-east-1` if empty                                      |
| Bucket                | The bucket's name; create it first at the provider                             |
| Path prefix           | A folder in the bucket for the backups, such as `spinikus/`; the root if empty |
| Path-style addressing | Turn it on for MinIO and other servers that need the bucket in the path        |
| Access key ID, secret access key | Credentials that may read, write, list and delete objects in the bucket |

**Test** writes a small file to the bucket and deletes it, to check the settings. The
secret access key is stored encrypted and never shown again; leave it empty when editing to
keep it.

## Backing up

On a database's **Backups** tab (an app's is described [below](#an-apps-volumes)):

- **Back up now** makes a backup to a destination you choose.
- **Add a schedule** backs up regularly. A schedule has a **destination**, a **cron
  expression** in UTC (`0 3 * * *` is every day at 03:00 UTC; `0 */6 * * *` every six hours),
  and how many of its backups to keep (7 by default); older ones are deleted. Schedules can
  be turned off and on.

From the CLI:

```sh
spinikus db backup shop/production/postgres          # waits until it is done
spinikus db backups shop/production/postgres
```

PostgreSQL backups are `pg_dump` archives in its custom format (`.pgdump`); Redis backups
are RDB snapshots (`.rdb`). They are stored at
`<path prefix><database service ID>/<backup ID>.<extension>` in the bucket, so you can
download them with any S3 client and use them without Spinikus.

The database must have a live deployment to be backed up. A backup runs on the database's
server, which uploads it straight to the bucket.

## Restoring

**Restore** next to a backup (or `spinikus db restore <backup ID>`) replaces the database's
data with the backup's. It first makes a **safety backup** of the current data to the same
destination, and restores only once that succeeded; to undo the restore, restore the safety
backup. Safety backups are labeled as such and are never deleted by a schedule.

- **PostgreSQL** restores in one transaction with `pg_restore --clean`: if anything fails,
  nothing changes.
- **Redis** replaces its data files and restarts. Writes made during the restore are lost.

Only one restore runs at a time per database. Restoring takes the admin role in the
database's environment.

## An app's volumes

An app with [volumes](../apps/volumes.md) has a **Backups** tab too. A backup holds **one
volume's files**, as a gzipped tar archive (`.tar.gz`, stored like a database's backups).
**Back up now** asks which volume when the app has more than one; a **schedule** backs up
every volume of the app at each run and keeps that many backups of each. Volumes are copied
while the app runs, so a file being written at that moment may be copied half-written; for
data that must be consistent, such as a SQLite database, prefer a time when nothing writes.

```sh
spinikus volume backup uploads -s shop/production/web   # waits until it is done
spinikus volume backups -s shop/production/web          # every volume's, or name one
spinikus volume restore bkp_01J…
```

**Restore** replaces the volume's files with the backup's: like a database's restore, it
makes a safety backup of the volume first. The app's container is then **stopped** (with its
shutdown timeout), the archive unpacked, and the container **started again**, whether the
restore succeeded or not; a restore that fails partway leaves the files as they were. Files
added since the backup are deleted.

The copying is done by a small helper container (`busybox`, pulled from Docker Hub the first
time) that mounts the volume, so the app's image needs no tools of its own. It runs on the
server the app last ran on, which is where the volume's data is: an app must have been
deployed once, and keeps its backups working while it is stopped. A volume restore waits for
no deployment, so it is refused while one of the app is in progress. A removed volume's
backups stay, but can't be restored. A database's data volume isn't backed up this way: its
backups are the database's own dumps.

## A new database from a backup

**Restore into a new database** next to a backup creates another database, of the same
engine at the same or a newer version, and fills it with the backup's data. Use it to look
at old data without touching the live database, to copy production data to staging, or to
upgrade PostgreSQL to a new major version. From the CLI:

```sh
spinikus db create Postgres18 --from-backup bkp_01J… --version 18 -e shop/staging
```

## Backing up Spinikus itself

These backups don't include Spinikus's own data (projects, settings, secrets). For that,
see [Backing up Spinikus](../operating/instance-backups.md).
