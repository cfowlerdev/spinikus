# Variables and secrets

Variables are environment variables for your services: connection strings, API keys,
feature flags. Set them on any of three levels:

- a **project**: shared by every service in every environment of the project;
- an **environment**: shared by every service in it;
- a **service**: for that service only.

When the same name is set on several levels, the most specific wins: a service's beats its
environment's, which beats the project's. The **Variables** tab of a service lists what it
inherits too, and marks what is overridden.

Changes take effect at the service's **next deployment**; running containers keep the
values they started with.

## Setting variables

In the dashboard, open the **Variables** tab of the project, environment or service, and
use **Add variable**. With the CLI:

```sh
spinikus env set NODE_ENV=production LOG_LEVEL=info     # on the linked service
spinikus env set --secret STRIPE_KEY                    # asks for the value, hidden
spinikus env set -e production SENTRY_DSN=https://…     # on an environment
spinikus env import .env                                # every variable in a .env file
spinikus env export > .env                              # secrets left out (--reveal: admins)
spinikus env unset LOG_LEVEL
```

Names are letters, digits and underscores, not starting with a digit, and are
case-sensitive. Names starting with `SPINIKUS_` are reserved. Values can be up to 64 KiB.

## Secrets

A variable marked **secret** is write-only: once saved, its value is never shown in the
dashboard, the CLI or the API, and build logs replace it with `[redacted]`. You can still
replace it. A secret can't be made plain again.

When you really need to read a secret back, an admin of its project, environment or service
can **Reveal** it in the dashboard. Each reveal is recorded in the audit log.

Every value, secret or not, is stored encrypted.

## When variables are available

Each variable has an **availability**:

- **At runtime** (the default): in the running container's environment.
- **At build time**: passed to the build only (see
  [Variables in Dockerfile builds](builds.md#variables-in-dockerfile-builds)).
- **Both**.

## Referring to other services

A value can include another service's variable with `${{ <service slug>.<NAME> }}`. This is
how apps get their database's connection string:

```text
DATABASE_URL = ${{ postgres.DATABASE_URL }}
```

References are resolved when a deployment is made, from services in the same
environment. The referenced value is taken as it is (references inside it aren't followed),
and the result is secret if any value it uses is. A reference to a service or variable that
doesn't exist stops the deployment from being made, with a message naming it. To write
`${{` literally, use `$${{`.

## Variables Spinikus sets

- `PORT`: the service's port, unless you set `PORT` yourself.
- Databases get their own credentials as variables; see [Databases](../data/databases.md).
