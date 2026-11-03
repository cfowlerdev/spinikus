# Introduction

Spinikus is a platform for running your apps on your own servers. You give it a Git
repository or a container image, and it builds the app, runs it, and puts it on the web
with HTTPS. It also runs databases for your apps and backs them up. Think of a hosted
platform like Heroku or Vercel, installed on a machine you control.

One server is enough to start. Spinikus installs as a single program that runs the
dashboard, the API, and everything that deploys and serves your apps. When you need more
machines, you add them as servers, and each one runs a small agent that Spinikus directs.

## What you can do with it

- **Deploy apps** from a Git repository (built from its Dockerfile, or detected and built
  for you when it has none) or from a prebuilt image.
- **Deploy on every push**, and get a **preview environment for each pull request**.
- **Roll back** to an earlier deployment in seconds, and restart an app in place.
- **Serve apps on your domains with HTTPS**, with certificates from Let's Encrypt that
  renew themselves.
- **Run PostgreSQL and Redis** next to your apps, and **back them up** to S3-compatible
  storage on a schedule.
- **Keep configuration in variables and secrets**, shared across a project, an environment
  or a single service.
- **Give your team access** with roles per project or environment, and see every change
  in the audit log.
- **Work from the dashboard, the `spinikus` command-line tool, or the JSON API**: all three
  do the same things.

## How to read this guide

If you are new, read [Installing Spinikus](installation.md), then follow
[Your first deployment](first-deployment.md) from start to finish. It takes you from a fresh
server to an app on the web. [How Spinikus is organized](concepts.md) explains the words the
rest of the guide uses.

After that, the chapters stand on their own: read the ones about what you are doing.
