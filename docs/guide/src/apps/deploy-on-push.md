# Deploy on push

A service built from Git can deploy each push to its branch by itself. The repository's
host (GitHub, GitLab, Gitea or Forgejo) tells Spinikus about pushes through a **webhook**.

Spinikus must be reachable from the host for this: from the internet for GitHub.com and
GitLab.com, at the address you use for the dashboard. Give the dashboard its own domain
first (see [Installing Spinikus](../installation.md#serve-the-dashboard-over-https)) so the
webhook URL uses HTTPS.

## Turning it on

1. Open the service's **Settings** tab and find **Deploy on push**.
2. Choose **Turn on deploy on push**. Spinikus shows a **secret**, once: copy it now.
3. The card lists the webhook's URL for each kind of host. At the repository's host, add a
   webhook with that URL and the secret:

   - **GitHub**: Settings → Webhooks → Add webhook. Payload URL: the GitHub URL. Content
     type: `application/json`. Secret: the secret. Events: choose **Let me select
     individual events**, then **Pushes** and **Pull requests**.
   - **GitLab**: Settings → Webhooks → Add new webhook. URL: the GitLab URL. Secret token:
     the secret. Triggers: **Push events** and **Merge request events**.
   - **Gitea and Forgejo**: Settings → Webhooks → Add webhook → Gitea (or Forgejo). Target
     URL: the Gitea URL. Content type: `application/json`. Secret: the secret. Events:
     **Push** and **Pull Request**.

The host usually sends a test delivery right away. The card shows the **last delivery** and
what Spinikus made of it, so you can check that it arrived.

## What deploys

A push to the service's branch deploys the pushed commit. Spinikus ignores, and says why in
the last delivery:

- pushes to other branches, and tags;
- a deleted branch;
- a push that changed no file under the service's root directory (so in a monorepo, each
  app deploys only when its own code changes);
- a commit that is already being deployed or is already live.

Pull request events make previews when the environment has them turned on (see
[Previews of pull requests](previews.md)); otherwise they are ignored.

Each delivery is checked against the secret before anything else, so only the host can
trigger deployments.

## Changing the secret, turning it off

- **New secret** replaces the secret; the URL stays the same. Update it at the host, since
  deliveries with the old one are refused from then on.
- **Turn off** removes the webhook: its URL stops working. Remove it at the host too.
