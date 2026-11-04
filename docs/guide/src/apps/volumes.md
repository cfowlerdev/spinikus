# Volumes

A container's files are thrown away at each deployment: the new deployment starts from its
image. Data that must survive, such as uploaded files or a SQLite database, goes in a
**volume**, a directory mounted into the container whose contents stay on the server.

## Adding a volume

In the service's **Settings** tab, under **Volumes**, add one with a **name** (a slug, such
as `uploads`) and a **mount path**, the absolute path in the container (such as
`/app/uploads`). Or:

```sh
spinikus volume add uploads /app/uploads
spinikus volume list
```

Volumes are mounted from the service's **next deployment**, so deploy after adding one.

A service with volumes is deployed by **recreating** it: the old container stops before
the new one starts, since two containers must not write to the same files. It is briefly
unavailable during each deployment.

## Changing and removing

You can rename a volume or change its mount path; the data stays and appears at the new path
from the next deployment:

```sh
spinikus volume update uploads --mount-path /srv/uploads
```

Removing a volume (admins only) stops mounting it from the next deployment:

```sh
spinikus volume remove uploads
```

**Its data stays on the server.** Spinikus doesn't delete it, so a mistake can be undone by
an administrator of the machine; the data is in a Docker volume named after the volume's ID
(`docker volume ls`).

A database's data volume can be renamed, but neither moved nor removed: either would start
the database empty. To get rid of a database's data, delete the database.

## Volumes and servers

A volume's data lives on the server that wrote it. If you move a service to another server
(its **Placement** setting), it starts there with empty volumes; copy the data across
yourself first if you need it: back the volume up and restore it after the move. Back up
important data to storage elsewhere: Spinikus backs volumes up to S3-compatible storage, by
hand or on a schedule, and restores them (see
[An app's volumes](../data/backups.md#an-apps-volumes)):

```sh
spinikus volume backup uploads
```
