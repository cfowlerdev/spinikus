# Deployments

Each time you deploy, Spinikus makes a **deployment**: a record of what runs (the commit or
image, the service's settings and its variables at that moment) that moves through these
steps:

| Status        | What happens                                                                         |
| ------------- | ------------------------------------------------------------------------------------ |
| **queued**    | Waiting its turn: a service deploys one deployment at a time                         |
| **preparing** | The server clones and builds the repository, or pulls the image                      |
| **releasing** | The release command runs, if the service has one (see below)                         |
| **starting**  | The new container starts                                                             |
| **verifying** | The health check runs                                                                |
| **live**      | The proxy sends the service's traffic to the new container                           |
| **draining**  | A newer deployment went live; this one is stopped once no traffic goes to it         |
| **superseded**| Replaced and stopped; it can still be rolled back to while its image is kept         |
| **failed**    | It didn't make it, with a reason; the previous deployment keeps serving              |
| **canceled**  | Canceled by someone, or replaced in the queue by a newer deployment                  |

Deploy from the service's page with **Deploy**, with `spinikus deploy`, with the API, or on
every push (see [Deploy on push](deploy-on-push.md)). Each deployment's page shows its
status, its history and its log, followed live while it runs.

If you deploy again while a deployment is still queued, the newer one replaces it (the
older ends as `canceled`, reason `superseded_in_queue`).

## Zero-downtime deployments

Apps are deployed **blue-green**: the new container starts next to the old one, and only
once it passes its health check does traffic move to it. The old container then gets its
**shutdown timeout** (10 seconds unless you set one) to finish its requests and exit.

Services with volumes, and databases, are deployed by **recreating** instead: the old
container stops before the new one starts, since two containers must not share a volume.
They are briefly unavailable during each deployment.

## Health checks

Before a deployment goes live, Spinikus checks it:

- With a **health check path**, such as `/healthz`, it sends `GET <path>` to the app's port
  until it answers with a `2xx` or `3xx` status. Failures during the **start period** (60
  seconds by default) don't count, and the first success ends it. After the start period,
  the check is retried every 2 seconds, and the deployment fails once it has failed one
  more time than its retries (3 by default).
- Without a path, the container only has to keep running for 5 seconds.

Set the path and start period in the service's **Settings** under **Runtime**, or in
[`spinikus.json`](spinikus-json.md) with the interval, timeout and retries too. A failed
check ends the deployment as `health_check_failed`; its container is kept, stopped, so you
can read its logs (see [Logs](logs.md)).

## Release commands

A **release command** runs once per deployment, after the build and before the new
container starts. It is the place for database migrations:

```json
{ "runtime": { "release": "npm run migrate" } }
```

It runs in a one-off container from the new image, through `/bin/sh -c`, with the service's
runtime variables and on the environment's network (so it reaches the database by its
slug), but without volumes. Its output goes to the deployment's log. If it exits with an
error, or runs longer than its timeout (`release_timeout`, 15 minutes by default), the
deployment fails as `release_failed` and the previous deployment keeps serving.

Since the previous version is still serving while the release command runs, migrations
must work with both the old and the new code.

## Rolling back

To go back to an earlier version, use **Roll back** next to it in the service's
deployment history, or:

```sh
spinikus rollback                  # to the deployment the live one replaced
spinikus rollback --to dep_01J…    # to a particular one
```

A rollback is a new deployment with the earlier deployment's image, settings and
variables. It skips the build and the release command, and starts right away, so it takes
seconds. Only superseded deployments whose image is still kept can be rolled back to.

## Restarting

**Restart** on the service's page (or `spinikus restart`) restarts the live container in
place, with the same image and settings. It doesn't make a new deployment, and the service
is unavailable for the moment it takes. To pick up changed variables or settings, deploy
instead.

## Stopping and starting

**Stop** on the service's page (or `spinikus stop`) stops the live container, for when you
want a service down for a while. The container and its deployment are kept, and the service
is marked **stopped** on its page and in its environment's list. Requests to its domains
are answered with `503 the app is not running`. It stays stopped when the server reboots.

**Start** (or `spinikus start`) starts the same container again. Deploying or rolling back
also starts a stopped service, since the new deployment's container runs as usual.

While a service is stopped, it can't be restarted, opened in a terminal, backed up or
restored. A stopped database's scheduled backups fail until it is started.

## Canceling

A deployment that is queued or in progress can be canceled from its page with **Cancel**,
or with `spinikus deployment cancel <id>`. The previous deployment keeps serving.

## Timeouts

A deployment fails as `timeout` if preparing takes over 30 minutes, or a later step takes
much longer than its settings allow. It fails as `agent_lost` if the server running it is
disconnected for more than 2 minutes.

## What is kept

After each deployment, Spinikus cleans up old ones on their servers, removing their
containers and images. It keeps:

- the live deployment and those in progress;
- the 5 newest superseded deployments of each service, so you can roll back to them
  (change the number under **Settings → General → Old releases**);
- up to 10 failed or canceled deployments newer than the last one that went live, so you
  can read their containers' logs.

Every deployment's record and log stay, so the history is complete.
