# The CLI

The `spinikus` command does from a terminal what the dashboard does: deploy, follow logs,
set variables, manage domains, databases and tokens. It talks to Spinikus through its API,
so it works from your laptop or from CI.

## Installing it

The CLI is the same `spinikus` binary that runs the servers, so it is already on them. On
your own machine:

- **Linux** (x86_64 or arm64): download `spinikus-x86_64-unknown-linux-musl.tar.gz` (or
  `aarch64-…`) from the [latest release](https://github.com/cfowlerdev/spinikus/releases/latest),
  and put the `spinikus` binary it holds on your `PATH`:

  ```sh
  curl -fsSL https://github.com/cfowlerdev/spinikus/releases/latest/download/spinikus-x86_64-unknown-linux-musl.tar.gz \
      | tar -xzf - spinikus
  sudo install spinikus /usr/local/bin/
  ```

- **macOS and others**: build it with Rust (install it with [rustup](https://rustup.rs)):

  ```sh
  cargo install --locked --git https://github.com/cfowlerdev/spinikus spinikus
  ```

## Signing in

```sh
spinikus login https://spinikus.example.com
```

It asks for your email and password and saves an API token for this machine as a
**context**, named after the server. Several instances can each have a context:

```sh
spinikus context list
spinikus context use staging.example.com
spinikus whoami
spinikus logout          # revokes this machine's token and forgets it
```

Contexts are kept in `~/.config/spinikus/` (or `$XDG_CONFIG_HOME/spinikus`, or
`$SPINIKUS_CONFIG_DIR`); the tokens in `credentials.toml`, readable only by you.

## Naming things

Commands that act on a project, environment or service take it as an ID or as a path of
slugs: `shop`, `shop/production`, `shop/production/web`.

**Link** a directory, such as your app's repository, to save typing:

```sh
cd ~/code/shop-web
spinikus link shop/production/web
spinikus deploy          # deploys shop/production/web
spinikus logs -f
spinikus env set FEATURE_X=on
```

The link is saved in `.spinikus/link.json` (ignored by Git), and found from any directory
below. Inside a linked directory, a bare slug is relative to the link: `spinikus deploy api`
deploys `shop/production/api`. Commands also use the link's instance, so the right context is
picked by directory.

## Commands

| Command        | What it does                                                                   |
| -------------- | ------------------------------------------------------------------------------ |
| `login`, `logout`, `whoami`, `context` | Signing in and choosing an instance                    |
| `link`         | Remember which environment or service this directory deploys                    |
| `project`      | List, create, show and delete projects                                          |
| `environment`  | List, create, show and delete environments; turn previews on and off            |
| `service`      | List, create (from an image or Git), show and delete services                   |
| `deploy`       | Deploy a service and follow it until it is live                                 |
| `rollback`     | Go back to an earlier deployment                                                |
| `restart`      | Restart a service's running container                                           |
| `stop`, `start` | Stop a service's running container until it is started or deployed again, and start it |
| `deployment`   | List deployments, show one, print or follow its log, cancel it                  |
| `logs`         | Print or follow a service's runtime logs                                        |
| `shell`        | Open a shell, or run a command, in a service's running container ([Terminal](apps/terminal.md)) |
| `env`          | List, set, unset, import and export variables                                   |
| `domain`       | List, add and remove domains                                                    |
| `volume`       | List, add, update and remove volumes                                            |
| `db`           | Create databases; back up, list backups and restore                             |
| `token`        | Create, list and revoke your API tokens                                         |

`spinikus <command> --help` describes each, with its options.

## Output

Results print as tables and sentences. With `-o json`, commands print the API's JSON instead
(lists as one array of every item), for scripts and `jq`:

```sh
spinikus service list -o json | jq -r '.[].slug'
```

Progress, such as a deployment's log while `deploy` follows it, goes to standard error, so it
doesn't mix with results. `--quiet` prints only results and errors; `--no-color` (or
`NO_COLOR`) turns colors off.

## Exit statuses

Scripts can rely on these:

| Status | Meaning                                                                      |
| ------ | ---------------------------------------------------------------------------- |
| 0      | Done                                                                         |
| 1      | Something else went wrong, such as the server being unreachable              |
| 2      | The command line was wrong                                                   |
| 3      | Not signed in, or the token was refused                                      |
| 4      | Signed in, but without the role this needs                                   |
| 5      | What was named doesn't exist, or you can't see it                            |
| 6      | Refused: invalid input, a conflict, or a change made since it was read       |
| 7      | A deployment, backup or restore that was waited for failed or was canceled   |

`shell` is the exception: once the command runs, the CLI exits with the command's own exit
status.

## In CI

Give the pipeline an API token scoped to what it deploys (see
[API tokens](operating/access.md#api-tokens)), and set two variables instead of signing in:

```sh
export SPINIKUS_URL=https://spinikus.example.com
export SPINIKUS_TOKEN=spk_…        # from the CI's secret store
spinikus deploy shop/production/web
```

`deploy` waits for the deployment and exits with status 7 if it fails, so the pipeline fails
with it. Add `--detach` to return as soon as it is queued.
