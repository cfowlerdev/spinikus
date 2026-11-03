# How Spinikus is organized

## Projects, environments and services

Everything you deploy lives in a three-level tree:

- A **project** is one product or system, such as `shop`.
- An **environment** is one running copy of the project, such as `production` or `staging`.
  Each environment's services are separate: their own containers, variables, data and
  network. Environments of the kind **preview** are made for pull requests (see
  [Previews](apps/previews.md)).
- A **service** is one thing that runs in an environment: an **app** (from a Git
  repository or an image), or a **database**.

Each has a name, and a **slug** made from it (`Shop` becomes `shop`) that appears in
hostnames, in the CLI (`shop/production/web`) and in references between services. Slugs are
lower-case letters, digits and hyphens, and are unique among their siblings.

Services in the same environment reach each other by slug: an app connects to its database
at `postgres:5432`, or to another app at `api:3000`. Services in other environments can't
reach them.

## Deployments

A **deployment** is one attempt to run a version of a service. It records what was
deployed (the commit or image, the settings and variables at that moment) and goes through
these steps:

`queued → preparing → releasing → starting → verifying → live`

When a newer deployment goes live, the old one is **drained** (it stops receiving traffic,
then stops) and becomes **superseded**. A deployment that doesn't make it ends as
**failed**, with a reason, or **canceled**, and the previous deployment keeps serving.

Deployments never change once made. Rolling back makes a new deployment from an older
one's image and settings. See [Deployments](apps/deployments.md).

## Servers and agents

A **server** is a machine that runs containers. The one Spinikus is installed on is the
first; you can add more (see [Servers](operating/servers.md)). Each server runs an
**agent** that starts containers, builds images and runs the **proxy**, which receives your
apps' web traffic on ports 80 and 443 and passes it to the right container.

The **control plane**, the part that serves the dashboard and the API and keeps the
database, decides what runs where; agents connect to it and do what it says. On a
single-server install, both run in the same program.

## Variables, domains, volumes

- **Variables** configure services; set them on a project, an environment or a service.
  Secrets are variables whose values are never shown again.
- **Domains** are hostnames you own that serve an app, with HTTPS certificates Spinikus
  gets for you. Every app also gets a generated hostname.
- **Volumes** are directories in a service's container whose data survives deployments.

## People

**Users** sign in to the dashboard and the CLI. An **instance admin** may do anything.
Everyone else gets a **role** in a project or an environment: **viewer**, **developer** or
**admin**. See [Users, roles and API tokens](operating/access.md).
