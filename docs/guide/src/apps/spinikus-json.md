# spinikus.json

Settings that change with your code (how to build it, how it starts, its health check) can
live in the repository, in a `spinikus.json` file in the service's root directory. Each
deployment reads the file from the commit it builds, so the settings and the code always
match, and a rollback returns to both.

```jsonc
{
  // Comments are allowed.
  "build": {
    "dockerfile": "docker/Dockerfile"
  },
  "runtime": {
    "port": 3000,
    "release": "npm run migrate",
    "healthcheck": { "path": "/healthz", "start_period": "30s" },
    "shutdown_timeout": "30s"
  }
}
```

For each field the file sets, the file wins over what the dashboard or API set. In the
dashboard, those fields are shown read-only with the file's values, and saving the form
keeps them. Fields the file leaves out use the service's settings.

The file holds only settings that belong to the code. Domains, variables' values, the
server a service runs on and credentials are never in it; set those in the dashboard, the
CLI or the API.

Images deployed from a registry have no checkout, so for them only the service's settings
apply.

## build

See [Builds](builds.md) for how a strategy is chosen.

| Field        | Type   | Meaning                                                                          |
| ------------ | ------ | -------------------------------------------------------------------------------- |
| `strategy`   | string | `dockerfile` or `railpack`; chosen from the repository when left out             |
| `dockerfile` | string | Dockerfile builds: the Dockerfile's path, `Dockerfile` by default                |
| `context`    | string | Dockerfile builds: the build context, `.` (the root directory) by default        |
| `target`     | string | Dockerfile builds: the stage to build, the last one by default                   |
| `args`       | object | Dockerfile builds: build arguments, as `{ "NAME": "value" }`                     |
| `command`    | string | Railpack builds: the build command, instead of the one Railpack detects          |

Paths are relative to the root directory and must stay inside the repository.

## runtime

| Field              | Type     | Meaning                                                                    |
| ------------------ | -------- | -------------------------------------------------------------------------- |
| `port`             | number   | The port the app listens on in its container; also passed as `PORT`        |
| `start`            | string   | The command that starts the app, run by `/bin/sh -c` instead of the image's own |
| `release`          | string   | A command run once per deployment before it starts, such as migrations     |
| `release_timeout`  | duration | How long `release` may run; `15m` by default                               |
| `healthcheck`      | object   | See below                                                                  |
| `shutdown_timeout` | duration | How long the container gets to exit when it is stopped; `10s` by default   |

`start` runs through a shell, so it can use the container's variables:
`"start": "exec gunicorn app:app --bind 0.0.0.0:$PORT"`. Start it with `exec` so the app
receives the stop signal directly.

### healthcheck

| Field          | Type     | Meaning                                                                  |
| -------------- | -------- | ------------------------------------------------------------------------ |
| `path`         | string   | The path to `GET`, such as `/healthz`; needs a `port`                    |
| `interval`     | duration | Time between checks; `2s` by default                                     |
| `timeout`      | duration | How long one check may take; `5s` by default                             |
| `retries`      | number   | Failures allowed after the start period; 3 by default                    |
| `start_period` | duration | Time in which failures don't count; `60s` with a path, `5s` without      |

See [Health checks](deployments.md#health-checks) for how they run.

### Durations

Durations are strings such as `"10s"`, `"2m"` or `"1m30s"`, at most an hour.

## Mistakes

Unknown fields are refused, so a misspelled setting fails the deployment instead of being
silently ignored. So are fields Spinikus doesn't support yet (`env`, `cron` and `apps`). The
deployment fails as `build_failed`, and its log says what is wrong. The file may be at most
64 KiB.
