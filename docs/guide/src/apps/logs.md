# Logs

There are two kinds of logs:

- **Deployment logs**: what happened while deploying (the clone, the build, the pull, the
  release command, the health check). Each deployment has its own.
- **Runtime logs**: what the app writes to its standard output and standard error while it
  runs.

## Deployment logs

Open a deployment from the service's **Deployments** tab. While it runs, its log is followed
live; once it has finished, the whole log is on the page, and **Download** saves it. Failures
end the log with the reason and the error. From the CLI:

```sh
spinikus deployment list
spinikus deployment logs dep_01J… --follow
```

`spinikus deploy` follows the log by itself until the deployment is live or has failed.

Each deployment's log keeps up to 10 MiB.

## Runtime logs

The service's **Logs** tab shows the live deployment's container output and follows new
lines as they arrive. You can search the lines, show only standard output or standard
error, choose how many earlier lines to start with, and download them. A failed or older
deployment's container logs are linked from its deployment page as **Container logs**, while
its container is still kept.

From the CLI:

```sh
spinikus logs                         # the last 100 lines of the linked service
spinikus logs -f                      # and follow new ones
spinikus logs -n 0 --since 2026-11-01T09:00:00Z
spinikus logs --deployment dep_01J…   # an earlier deployment's container
```

Runtime logs are read from the container on its server when you ask for them; Spinikus
doesn't store them. Docker keeps them while the container exists, so they are gone once a
deployment is cleaned up.

Runtime logs aren't filtered: an app that prints a secret shows it to everyone who can view
the service.
