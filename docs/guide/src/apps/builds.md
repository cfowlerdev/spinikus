# Builds

A deployment from Git builds an image on the server it will run on, with Docker's
BuildKit. There are two ways to build, chosen like this:

1. the `strategy` in the build settings, if you set one;
2. otherwise a **Dockerfile** build, if the root directory has a `Dockerfile` (or the
   settings name a Dockerfile or a target);
3. otherwise **Railpack**, which detects the language and framework and builds without a
   Dockerfile.

The build's output is in the deployment's log. Images are tagged `spinikus/<service>:<deployment>`
and removed once their deployment is cleaned up (see
[Deployments](deployments.md#what-is-kept)). BuildKit's cache is kept for 7 days, so
rebuilds are fast.

## Dockerfile builds

Spinikus builds the `Dockerfile` in the root directory, with the root directory as the
build context. The `.git` directory is left out of the context. Change the defaults with
build settings in [`spinikus.json`](spinikus-json.md):

```json
{
  "build": {
    "dockerfile": "docker/Dockerfile.prod",
    "context": ".",
    "target": "release",
    "args": { "NODE_ENV": "production" }
  }
}
```

| Setting      | What it does                                                                    |
| ------------ | ------------------------------------------------------------------------------- |
| `dockerfile` | The Dockerfile's path, relative to the root directory. It must be inside the context. |
| `context`    | The directory sent to the builder, relative to the root directory               |
| `target`     | The stage of a multi-stage Dockerfile to build; the last one by default         |
| `args`       | Build arguments (`ARG` in the Dockerfile)                                       |

### Variables in Dockerfile builds

Variables whose availability is **build** or **both** (see
[Variables](variables.md)) reach the build:

- **Plain** ones are build arguments, read with `ARG NAME`. They override `args` of the same
  name. Build arguments can end up in the image's layers, so don't use them for secrets.
- **Secret** ones are BuildKit secrets, which never end up in the image. Read them in a
  `RUN` step:

  ```dockerfile
  RUN --mount=type=secret,id=NPM_TOKEN \
      NPM_TOKEN="$(cat /run/secrets/NPM_TOKEN)" npm ci
  ```

Secret values (of four characters or more) are replaced with `[redacted]` in the build log.

## Railpack builds

[Railpack](https://railpack.com) looks at the repository (a `package.json`,
`requirements.txt`, `go.mod`, `Gemfile` and so on) and builds an image for it, including the
command that starts the app. It supports Node.js, Python, Go, Ruby, PHP, Java, Rust,
Elixir, Deno, static sites and more; see its documentation for how it detects each and the
`RAILPACK_…` variables that adjust it.

Every variable available at build time is passed to Railpack and to the build as a BuildKit
secret, so none of them end up in the image's layers.

To run a different build command than the one Railpack detects:

```json
{ "build": { "strategy": "railpack", "command": "npm run build:prod" } }
```

Railpack builds take only `strategy` and `command`; `dockerfile`, `context`, `target` and
`args` are for Dockerfile builds.

## When a build fails

The deployment fails with the reason `build_failed`, and the previous deployment keeps
serving. The deployment's log has the builder's output: open the deployment from the
service's **Deployments** tab, or run `spinikus deployment logs <id>`.

Docker Compose files and Cloud Native Buildpacks are not supported yet; a build that asks
for them fails with a message saying so.
