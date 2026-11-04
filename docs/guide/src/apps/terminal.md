# Terminal

A terminal opens a shell, or runs a command, inside a service's running container, from the
dashboard or the CLI, without SSH to the server. Use it to look around a misbehaving app,
check its files and environment, or run a one-off command such as a database console.

Only the environment's **admins** may open one (see [Roles](../operating/access.md#roles)),
since it runs anything as the container's user. Each one opened is recorded in the audit log
as `service.terminal`.

## From the dashboard

Open the service's **Terminal** tab, or the **Terminal** button next to **Restart** on the
service's page. It opens a shell in the container of the live deployment: `bash` if the
image has it, else `sh`.

To open one in another deployment's container, such as one still being verified, use the
**Terminal** link on that deployment's page.

The status above the terminal says **Connected** once it is open, and how it ended, such as
**Exited with code 0** when you type `exit`. **Open again** starts a new one. Leaving the page
closes it.

## From the CLI

```sh
spinikus shell                        # a shell in the linked service's container
spinikus shell shop/production/api    # another service's
spinikus shell -- psql -U app         # run a command instead of the shell
spinikus shell --deployment dep_01J…  # another deployment's container
```

From a terminal, `shell` puts yours in raw mode, so keys such as Ctrl+C reach the command,
and passes on changes to the window's size. It exits with the command's exit status.

Input can come from a pipe too. It is sent as if typed, followed by Ctrl+D, which ends a
shell. The command always runs with a terminal, so its output includes the input echoed:

```sh
echo 'ls /app' | spinikus shell
```

## What to know

- **Only running containers.** A stopped service has to be started first. A deployment whose health check failed has its container
  stopped; read its **Container logs** instead (see [Logs](logs.md)). A container that keeps
  crashing may not stay up long enough to open one.
- **Changes are temporary.** Files you change are lost when the container is replaced by the
  next deployment or a rollback. Fix the cause in the code or the settings, and deploy.
  Data in [volumes](volumes.md) stays.
- **Closing a terminal ends its shell**, as Ctrl+D would. A program still running in the
  foreground, such as a long script, keeps going until it exits or the container is
  replaced. Stop it before closing.
- **It runs as the container's user**, with the container's variables, secrets included.
