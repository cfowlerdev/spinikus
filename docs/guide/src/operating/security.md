# Security

The repository's [security document](https://github.com/cfowlerdev/spinikus/blob/main/docs/security.md)
describes the trust model, what protects what, and the risks Spinikus accepts for now. In
short, for running an instance:

- **Serve the dashboard over HTTPS** on its own domain, and bind `[http] listen` to
  `127.0.0.1:8080` (see [Installing Spinikus](../installation.md#serve-the-dashboard-over-https)).
- **Developers run code on your servers.** Builds and apps run whatever their repository
  says, with network access. Give the developer role only to people you would trust with a
  shell on the servers.
- **The `spinikus` user can run containers**, which makes it as powerful as root on its
  server. Keep the servers for Spinikus.
- **Secrets are encrypted** under the master key, which lives outside the database. Back up
  the key with the database ([Backing up Spinikus](instance-backups.md)), and keep the
  instance backups' passphrase safe.
- **Apps reach your private network** like any process on the server, cloud metadata
  endpoints included. Restrict that with the host's firewall if it matters.
- **Use scoped, expiring API tokens** for automation, one per pipeline.
- **Check the audit log** (Settings → Audit log) for changes you don't expect.
- **Keep Spinikus up to date** (see [Upgrading](upgrading.md)), and the servers' operating
  systems and Docker too.

Report vulnerabilities privately to the maintainers rather than in a public issue.
