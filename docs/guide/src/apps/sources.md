# Sources: Git and images

An app's **source** says what to deploy. Each service has one, set when you create it and
changed in the service's **Settings** tab under **Source**. A change takes effect at the
next deployment.

## A Git repository

Spinikus clones the repository, checks out the newest commit of the branch, and builds an
image from it (see [Builds](builds.md)).

| Field          | What it is                                                                              |
| -------------- | --------------------------------------------------------------------------------------- |
| Repository     | The URL: `https://…`, `ssh://…`, or `git@host:owner/repo.git`                           |
| Branch         | The branch to deploy, `main` by default                                                 |
| Root directory | The directory to build, for repositories that hold several apps; the root by default   |

The root directory is where Spinikus looks for the `Dockerfile` and `spinikus.json`, and
what it builds. With a monorepo, create one service per app, each with its own root
directory.

### Private repositories

Clone private repositories over SSH. When a service's repository URL is an SSH one
(`git@github.com:you/app.git` or `ssh://…`), Spinikus creates a **deploy key** for it, a key
pair whose private half never leaves Spinikus except to go to the server that builds. Its
public key, and its fingerprint, are in the service's **Settings** under **Deploy key**.

Add the public key to the repository as a **read-only deploy key**:

- **GitHub**: the repository's Settings → Deploy keys → Add deploy key.
- **GitLab**: Settings → Repository → Deploy keys.
- **Gitea and Forgejo**: Settings → Deploy keys.

If the key may have leaked, make a new one with the API
(`POST /api/v1/services/{id}/source/deploy-key`), add it to the repository and remove the
old one there. Spinikus uses the new key from the next build.

Don't put a token in an HTTPS URL to reach a private repository: the URL is shown to
everyone who can see the service. Use SSH and a deploy key instead.

## A prebuilt image

Spinikus pulls the image and runs it. Give a full reference such as
`ghcr.io/acme/web:1.4`, `nginx:1.27` or `traefik/whoami`. Each deployment pins the image by
its digest, so a later push to the same tag doesn't change what a deployment runs, and
rolling back returns exactly the old image.

Images must be public for now: Spinikus doesn't yet log in to registries.

## The port

An app that serves HTTP needs its **port**, the port it listens on inside its container, set
in the source form or in the service's runtime settings. Spinikus sets the `PORT` variable
to it, so apps that read `PORT` use it. Without a port, the app gets no web traffic, which
suits workers.
