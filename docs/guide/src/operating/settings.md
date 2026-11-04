# Settings

Instance admins manage the instance under **Settings**. Everyone else sees only their own
**Account** and **API tokens** there.

| Tab           | What it holds                                                                  |
| ------------- | ------------------------------------------------------------------------------ |
| **General**   | The settings below                                                             |
| **Servers**   | Servers and their agents (see [Servers](servers.md))                           |
| **Storage**   | Storage destinations for backups (see [Backups and restores](../data/backups.md)) |
| **Backups**   | Backups of Spinikus itself (see [Backing up Spinikus](instance-backups.md))   |
| **Notifications** | Where messages about failures go (see [Notifications](notifications.md)) |
| **Users**     | Users (see [Users, roles and API tokens](access.md))                           |
| **Audit log** | Every change, by whom                                                          |
| **Account**   | Your name and password                                                         |
| **API tokens**| Your API tokens                                                                |

## General

| Setting                 | Default                    | Meaning                                                                  |
| ----------------------- | -------------------------- | ------------------------------------------------------------------------ |
| **Base domain** (Generated hostnames) | `localhost`  | Apps' generated hostnames go under it: `<service>-<environment>-<project>.<base domain>`. Empty turns generated hostnames off. |
| **Domain** (Dashboard domain) | none                 | Serves the dashboard and API at this hostname, over HTTPS, through the control plane server's proxy |
| **Email** (HTTPS certificates) | none                | The Let's Encrypt account's contact; setting it agrees to the CA's terms and turns HTTPS on |
| **ACME directory**      | Let's Encrypt              | Another CA's ACME directory URL, such as Let's Encrypt's staging one for testing |
| **Releases kept** (Old releases) | 5                 | How many earlier deployments of each service keep their image, to roll back to (at most 100) |

Changes apply right away: hostnames are rerouted and certificates ordered within moments.

The settings can also be read and changed through the API (`GET`/`PUT /api/v1/settings/{key}`,
with the keys `base_domain`, `dashboard_domain`, `acme_email`, `acme_directory_url` and
`releases_kept`).
