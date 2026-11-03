# Your first deployment

This chapter takes you from a fresh install to an app answering on the web over HTTPS. It
deploys a small public image first, so nothing can go wrong with a build, then shows how
to deploy your own code from Git.

You need Spinikus installed and to be signed in as the first admin (see
[Installing Spinikus](installation.md)).

## 1. Give your apps hostnames

Every app gets a hostname of its own, made from its names:
`<service>-<environment>-<project>.<base domain>`, such as
`web-production-shop.apps.example.com`. On a fresh install the base domain is `localhost`,
which only works on the server itself. Give it a real one:

1. Add a **wildcard** DNS record, `*.apps.example.com`, of type `A` (and `AAAA` for IPv6),
   pointing at your server. Any name under `apps.example.com` then reaches it.
2. In the dashboard, open **Settings → General**. Under **Generated hostnames**, set the
   base domain to `apps.example.com` and save.
3. If you haven't yet, set an email under **HTTPS certificates** on the same page, so apps
   get certificates.

You can use any domain you control; `apps.` is only a convention. If you have no domain
yet, skip this step: you can still reach the app from the server with `curl`, as shown in
step 5.

## 2. Create a project and an environment

A **project** holds one product or system; its **environments**, such as production and
staging, each run their own copy of it.

1. On the dashboard's home page, choose **New project**. Name it `Shop` and create it.
2. On the project's page, choose **New environment**. Name it `Production`, keep the kind
   **Production**, and create it.

The slugs, `shop` and `production`, are derived from the names. They appear in hostnames
and in the CLI.

## 3. Create a service

A **service** is one app (or one database) in an environment.

1. On the environment's page, choose **New service**.
2. Name it `web`.
3. Under **Deploy from**, choose **A prebuilt image**, and enter `traefik/whoami`, a tiny
   web server that answers with details of each request.
4. Set **Port** to `80`, the port the app listens on inside its container.
5. Create the service.

## 4. Deploy it

On the service's page, choose **Deploy**. The deployment's page follows it live:

1. **queued**, then **preparing**: the server pulls the image;
2. **starting**: it starts a container;
3. **verifying**: it checks that the container stays up (or answers a health check, if you
   set one);
4. **live**: the proxy sends the app's traffic to it.

The log under the status shows each step. Deployments of an image take a few seconds.

## 5. Open it

The service's **Domains** tab lists where it answers. With the base domain from
step 1, open `https://web-production-shop.apps.example.com`. The first request may take a
few seconds while the certificate is issued; until then, the plain `http://` address works.

Without a base domain, try it from the server itself, naming the host the proxy should
route:

```sh
curl -H 'Host: web-production-shop.localhost' http://127.0.0.1/
```

## 6. Deploy your own code

Now replace the image with your repository. Create another service (or change this one's
source in its **Settings** tab):

1. Under **Deploy from**, choose **A Git repository**.
2. Enter the repository's URL, such as `https://github.com/you/your-app.git`, and the
   **Branch** to deploy.
3. Set **Port** to the port your app listens on. Spinikus also passes it to the app as the
   `PORT` variable, so apps that read `PORT` need nothing else.
4. Deploy.

A Git deployment clones the repository and builds an image: from its `Dockerfile` if it has
one, otherwise with Railpack, which detects the language and framework (Node.js, Python,
Go, Ruby, PHP, Java, static sites and more). The first build takes a few minutes; later
ones reuse the build cache.

For a private repository, use its SSH URL (`git@github.com:you/your-app.git`). Spinikus
creates a deploy key for the service and shows its public key in the service's
**Settings**; add it to the repository as a read-only deploy key. See
[Sources](apps/sources.md).

## Where to go next

- [Variables and secrets](apps/variables.md): configure the app.
- [Databases](data/databases.md): add PostgreSQL or Redis.
- [Domains and HTTPS](apps/domains.md): serve it on your own domain.
- [Deploy on push](apps/deploy-on-push.md): deploy every push to the branch.
- [The CLI](cli.md): do all of this from your terminal.
