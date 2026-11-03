# Installing Spinikus

## What you need

- **A Linux server with systemd**, x86_64 or arm64, that you can reach over SSH and run
  commands on as root (directly or with `sudo`). A fresh virtual machine from any cloud
  provider works well.
- **A public IP address** for the server, if your apps should be reachable from the
  internet.
- **A domain name** you can add DNS records to, for HTTPS. You can install and try
  Spinikus without one, but browsers will only reach your apps over HTTPS through a name.

The installer takes care of everything else: it installs Docker, Git and
[Railpack](https://railpack.com) when they are missing.

## Install

On the server, run:

```sh
curl -fsSL https://github.com/cfowlerdev/spinikus/releases/latest/download/install.sh | sudo sh
```

The script:

1. downloads the latest release and checks it against its published checksum;
2. installs Docker (through `get.docker.com`), Git and Railpack if they are missing;
3. creates a `spinikus` system user, and puts it in the `docker` group so it can run
   containers;
4. writes the configuration to `/etc/spinikus/spinikus.toml` and keeps data in
   `/var/lib/spinikus`;
5. starts Spinikus as the `spinikus` systemd service.

When it finishes, it prints a link like this one:

```text
==> Spinikus is running. Create the first admin at:

    http://203.0.113.10:8080/setup?code=4f9c…

    The link holds a one-time setup code; keep it to yourself.
```

The setup code makes sure that only someone who can see the installer's output creates the
first account. It works until the first admin exists. If you lose the link, the code is in
`/var/lib/spinikus/setup-code` until then.

## Open the ports

Spinikus uses these ports. If your server or cloud provider has a firewall, open them:

| Port | What uses it                                                                    |
| ---- | ------------------------------------------------------------------------------- |
| 80   | Your apps over HTTP, and Let's Encrypt checking that you own a domain           |
| 443  | Your apps over HTTPS                                                            |
| 8080 | The dashboard and the API over plain HTTP, until you give them their own domain |
| 8443 | Agents on your other servers, if you add any                                    |

Port 8080 is only needed at first. Once the dashboard has its own domain with HTTPS (see
below), you can close it.

## Create the first admin

Open the link the installer printed. Enter your name, email and a password of at least 12
characters. This account is an **instance admin**: it can do everything, including adding
users and servers. You are signed in right away and land on the dashboard.

## Serve the dashboard over HTTPS

The dashboard starts on plain HTTP at port 8080, so your password travels unencrypted. Give
it its own domain as soon as you can:

1. Add a DNS `A` record (and `AAAA` for IPv6) for a name such as `spinikus.example.com`
   pointing at the server.
2. In the dashboard, open **Settings → General**.
3. Under **HTTPS certificates**, enter an email address. Spinikus uses it for its Let's
   Encrypt account; setting it agrees to Let's Encrypt's terms of service. Save.
4. Under **Dashboard domain**, enter `spinikus.example.com` and save.

Within a minute or so, Spinikus gets a certificate and serves the dashboard and the API at
`https://spinikus.example.com`. Sign in there from now on.

Then stop serving plain HTTP on port 8080 from outside the machine. Edit
`/etc/spinikus/spinikus.toml`, set:

```toml
[http]
listen = "127.0.0.1:8080"
```

and restart Spinikus with `sudo systemctl restart spinikus`. You can then close port 8080 in
your firewall.

## Check that it is running

```sh
systemctl status spinikus
journalctl -u spinikus -f      # its log, followed
```

Next: [Your first deployment](first-deployment.md).

## Installer options

Pass options after `sh -s --`:

```sh
curl -fsSL https://github.com/cfowlerdev/spinikus/releases/latest/download/install.sh \
    | sudo sh -s -- --version v0.1.1
```

| Option                  | What it does                                                                  |
| ----------------------- | ----------------------------------------------------------------------------- |
| `--version v<x.y.z>`    | Installs that release instead of the latest                                   |
| `--http-listen <addr>`  | Where the dashboard and API listen (default `0.0.0.0:8080`)                   |
| `--skip-docker`         | Doesn't install Docker; apps can't run until it is installed                  |
| `--skip-railpack`       | Doesn't install Railpack; repositories without a Dockerfile won't build       |
| `--uninstall`           | Removes Spinikus (see [Upgrading and uninstalling](operating/upgrading.md))   |
| `--join`, `--token`, `--ca-fingerprint` | Installs an agent on another server (see [Servers](operating/servers.md)) |
