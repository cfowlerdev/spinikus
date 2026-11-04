# Users, roles and API tokens

## Users

Instance admins manage users under **Settings → Users**: add one with an email, a name and
an initial password (share it with them however you share passwords; they can change it),
edit their details, reset their password, and disable or enable them. A disabled user can't
sign in, their sessions end, and their API tokens stop working.

Everyone changes their own password under **Settings → Account**. Changing it signs you out
everywhere else.

There is no self sign-up: only instance admins add users.

## Roles

An **instance admin** may do everything, everywhere, including managing users, servers,
settings and storage. Make someone an instance admin by editing their user.

Everyone else has a **role** in projects or environments, given by that project's or
environment's admins on its **Members** tab (add a member by their email):

| Role          | May                                                                                     |
| ------------- | --------------------------------------------------------------------------------------- |
| **Viewer**    | See services, deployments, logs and variables (not secret values)                       |
| **Developer** | Also create and change services, variables, volumes and domains; deploy, roll back, restart, stop and start; back databases up |
| **Admin**     | Also delete services and volumes; reveal secrets; open terminals in containers; restore backups; manage backup schedules and members. A project's admins also manage its environments and turn previews on |

A role in a project covers all its environments; a role in an environment covers only that
one. When someone has both, the higher role counts. Only instance admins create projects.

What a user may not see doesn't exist for them: it is missing from lists, and opening it
says "not found". The dashboard shows only the buttons for what you may do.

**Developers run code on your servers**: builds and apps run whatever the repository says.
Give the developer role only to people you would trust with a shell on the servers.

## API tokens

The CLI, scripts and CI authenticate with **API tokens**. `spinikus login` makes one for
the CLI. Make others under **Settings → API tokens**, or with the CLI:

```sh
spinikus token create deploy-bot --role developer --project shop --expires 30
spinikus token list
spinikus token revoke tok_01J…
```

A token has:

- a **role**, which caps what it may do: a viewer token can't deploy, even if you could;
- optionally a **scope**, one project or environment it is limited to;
- an **expiry**, 90 days by default, or none.

A token is never wider than the token or session that made it. The token's secret is shown
only once; Spinikus stores only a hash of it. Instance admins can revoke anyone's tokens
from the user's page.

For CI, give each pipeline its own token, scoped to what it deploys:

```sh
export SPINIKUS_URL=https://spinikus.example.com
export SPINIKUS_TOKEN=spk_…
spinikus deploy shop/production/web
```

## The audit log

Every change is recorded: who did it (a user, a webhook or Spinikus itself), what, when, and
from which address. Instance admins see everything under **Settings → Audit log**; a
project's admins see the project's entries on its **Audit log** tab. Entries name the fields
that changed, never their values.

## Sign-in protection

After 5 wrong passwords for one email from one address within 15 minutes, or 50 from one
address for any emails, that address must wait out the 15 minutes. Sessions last 14 days.
