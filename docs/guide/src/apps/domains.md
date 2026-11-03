# Domains and HTTPS

Every app with a port answers on two kinds of hostnames:

- its **generated hostname**, `<service>-<environment>-<project>.<base domain>`, such as
  `web-production-shop.apps.example.com`, from the base domain in
  **Settings → General** (see [Your first deployment](../first-deployment.md#1-give-your-apps-hostnames));
- the **domains** you add, such as `shop.example.com`.

The service's **Domains** tab lists both, with the HTTPS status of each.

## Adding a domain

1. Point the domain at the server the app runs on: an `A` record (and `AAAA` for IPv6) with
   the server's address, or a `CNAME` to a name that has them.
2. Open the service's **Domains** tab and add the hostname, or run
   `spinikus domain add shop.example.com`.

The app answers on it as soon as it has a live deployment. A hostname can belong to only
one service, and names under the base domain are kept for generated hostnames.

## HTTPS

Once an email is set under **Settings → General → HTTPS certificates**, Spinikus gets a
certificate from Let's Encrypt for every hostname its apps answer on, and renews it well
before it expires. Let's Encrypt checks that you control the name by requesting a file
from it over plain HTTP, so:

- the DNS record must already point at the server;
- port 80 must be open to the internet.

The Domains tab shows each hostname's status: **HTTPS** once it has a certificate, **No
certificate yet** while one is on its way, and **Certificate failing** with the error when
it couldn't get one (Spinikus keeps retrying, waiting longer each time). Fix the cause,
usually DNS or a firewall, and it gets one on the next try.

Once a hostname has a certificate, plain HTTP requests to it are redirected to HTTPS.

Wildcard domains (`*.example.com`) can be added and are served, but over HTTP only:
Spinikus gets certificates only for single names.

Want certificates from a CA other than Let's Encrypt? Set its ACME directory URL on the same
settings page.

## Redirecting a domain

To send visitors from one name to another, such as `www.example.com` to `example.com`, add
both to the service, then set the first to redirect to the second (in the dashboard, or
`spinikus domain add www.example.com --redirect-to example.com`). Requests are answered with
a permanent redirect (`308`) that keeps the path and query.

## What the app sees

The proxy passes requests on with the original `Host` header, and adds
`X-Forwarded-Proto` (`http` or `https`) and `X-Forwarded-For` (the client's address). Apps
that build absolute URLs or check for HTTPS should read these.

WebSockets work without any setting.

## Removing a domain

Use **Remove** on the Domains tab, or `spinikus domain remove shop.example.com`. A service
must have no domains left before it can be deleted.
