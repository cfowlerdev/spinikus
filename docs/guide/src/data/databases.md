# Databases

Spinikus runs PostgreSQL and Redis for your apps, as services in the same environment.

| Engine     | Versions      | Image                       |
| ---------- | ------------- | --------------------------- |
| PostgreSQL | 15, 16, 17, 18 | the official `postgres:<version>` |
| Redis      | 7, 8          | the official `redis:<version>`    |

The newest version is the default.

## Creating a database

On the environment's page, choose **New database**, name it (the name's slug is how apps
reach it, such as `postgres`), choose the engine and version, and create it. Or:

```sh
spinikus db create Postgres --engine postgres
spinikus db create Cache --engine redis --version 7
```

Spinikus creates a service of the kind **database** with:

- the engine's official image;
- a `data` volume where the image keeps its data;
- variables holding the credentials, with a generated password:

  | PostgreSQL                                                      | Redis                       |
  | --------------------------------------------------------------- | --------------------------- |
  | `POSTGRES_USER` (`app`), `POSTGRES_PASSWORD`, `POSTGRES_DB` (`app`), `DATABASE_URL` | `REDIS_PASSWORD`, `REDIS_URL` |

  The password and the URLs are secret. Redis runs with a password and with its
  append-only file on, so writes survive restarts.

Then it deploys it. `spinikus db create` waits until it is live; pass `--detach` not to.

To change its settings or variables before it first runs, such as its placement or extra
variables, uncheck **Deploy it now** on the form, or pass `--no-deploy`. It then waits on its
page until you press **Deploy** (or run `spinikus deploy shop/production/postgres`). The
credentials are read only when the database first starts with an empty volume, so change
them, if you want to, before that first deployment.

## Connecting an app

The database's page shows how to connect, under **Connecting**. In the app's variables,
refer to the database's URL:

```text
DATABASE_URL = ${{ postgres.DATABASE_URL }}
REDIS_URL    = ${{ cache.REDIS_URL }}
```

(`postgres` and `cache` being the databases' slugs.) Deploy the app, and it gets the URL
with the password, such as `postgres://app:…@postgres:5432/app`. See
[Referring to other services](../apps/variables.md#referring-to-other-services).

A database is reachable only from the services of its environment, by its slug, on its
engine's port (5432 or 6379). It is not exposed to the internet.

## Working with the data

To open a shell on the database, run on its server:

```sh
docker ps --filter label=spinikus.service --format '{{.Names}}  {{.Image}}'
docker exec -it spinikus-dep_01J… psql -U app app        # PostgreSQL
docker exec -it spinikus-dep_01J… sh -c 'redis-cli -a "$REDIS_PASSWORD"'   # Redis
```

The container is named `spinikus-<deployment ID>`, after the database's live deployment
(on its page, or `spinikus deployment list shop/production/postgres`).

## Things to know

- **Changing the password variables later doesn't change the password**: the images read
  them only when they initialize an empty data volume.
- **Deployments recreate the container**, so a database is unavailable for the few seconds a
  redeploy takes.
- **Upgrading to a new major version is up to you.** PostgreSQL can't open data written by an
  older major version. To upgrade, create a new database of the newer version from a backup
  of the old one (see [Backups and restores](backups.md#a-new-database-from-a-backup)),
  point your apps at it, and delete the old one when you are sure.
- **The data volume can't be moved or removed**; deleting the database is how to drop it.
  Its data stays on the server even then (see [Volumes](../apps/volumes.md)).
- **Back it up.** See [Backups and restores](backups.md).
