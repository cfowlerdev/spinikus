# Previews of pull requests

An environment can deploy a **preview** of each pull request: a full copy of the
environment running the pull request's code, at its own addresses, removed when the pull
request closes. Reviewers can try a change before it is merged.

## Turning previews on

Previews need [deploy on push](deploy-on-push.md) on the environment's Git services, with
pull request events sent (the setup there includes them). Then a project admin turns
previews on for the environment: in its **Settings** tab under **Previews**, or with

```sh
spinikus environment previews on production
```

## What a preview is

When a pull request is opened against the branch a Git service of the environment deploys,
Spinikus creates an environment named `PR #<number>` (slug `pr-<number>`) of the kind
**preview**, and copies into it:

- every service of the environment, with its source, settings, placement, deploy key and
  variables;
- every volume, new and empty, so databases in a preview start with no data;
- the environment's variables.

Services built from the pull request's repository build its branch; every other service
deploys what it deploys in the original environment. Variables with references, such as
`${{ postgres.DATABASE_URL }}`, point at the preview's own copies, so a preview never touches
the original environment's data.

Domains, webhooks and backup schedules aren't copied: a preview's apps answer at their
generated hostnames, such as `web-pr-42-shop.apps.example.com`.

## Its lifecycle

- **New commits** to the pull request redeploy the preview's copies of that repository's
  services.
- **Closing or merging** the pull request deletes the preview: its services, their
  containers, their volumes and data, then the environment.
- A project admin can delete a preview by hand with **Delete preview** on its page; the next
  commit to the pull request makes it again.

Pull requests from forks get no preview, since their code would run with the environment's
secret variables.

## Who can see previews

Members of the project see its previews. Members of only the original environment don't:
a preview is an environment of its own.
