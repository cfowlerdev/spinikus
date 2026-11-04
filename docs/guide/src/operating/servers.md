# Servers

The server Spinikus is installed on runs your services until you add others. Add servers to
spread services across machines, or to keep some apart, such as databases on their own
machine. Servers are managed by instance admins.

Each added server runs a Spinikus **agent**. Agents connect to the control plane (the
server Spinikus was installed on) on port 8443 over mutual TLS: the control plane never
connects to them, so they need no open port for it, only ports 80 and 443 for their apps'
traffic.

## Adding a server

1. Make sure the new server can reach the control plane on port 8443 (open it in the
   control plane's firewall).
2. In the dashboard, open **Settings → Servers**, add a server and give it a name.
3. On the server's page, choose **Create a join token**. Spinikus shows a command like:

   ```sh
   curl -fsSL https://github.com/cfowlerdev/spinikus/releases/download/v0.1.1/install.sh \
       | sudo sh -s -- --join https://spinikus.example.com:8443 \
         --token spj_… --ca-fingerprint sha256:…
   ```

4. Run it on the new server. It installs Docker, Git, Railpack and the agent (as the
   `spinikus-agent` systemd service), and enrolls the agent.

The token works once, within an hour. The fingerprint lets the agent check that it is
talking to your control plane before it trusts it. If the new server reaches the control
plane at another address than the one in the command, change it before running it.

Within seconds, the server's page shows it **connected**, with facts about the machine.

## Running services on a server

Each service runs on one server, its **placement**. Choose it when creating a service or a
database (the **Server** field, shown once there is more than one server), or later in the
service's **Settings** under **Placement**. A placement change applies from the next
deployment.

Point the DNS records of a service's domains at the server it runs on. Each server's proxy
serves only its own services.

Volumes' data stays on the server that wrote it: a service moved to another server starts
there with empty volumes.

## Resources

A server's page shows what the whole machine uses (CPU, memory, network and disk) over
time, and warns when its disk is nearly full. See [Metrics](../apps/metrics.md).

## When a server goes away

If an agent loses its connection, its server shows **disconnected** (a server whose agent
never connected shows **pending**). Containers already running there keep running and
serving: the agent and the proxy don't need the control plane for that. Deployments on that
server fail as `agent_lost` once it has been away for 2 minutes. The agent reconnects by
itself when it can.

To stop using a server for good, use **Revoke server** on its page. Its agent is
disconnected and can never connect again, and its join tokens stop working. Containers
already running there keep running, so move its services first, then uninstall Spinikus on
it (see [Upgrading and uninstalling](upgrading.md)).

## The agent's files

On an added server, the agent keeps its identity in `/var/lib/spinikus/agent`. Its
configuration, `/etc/spinikus/spinikus.toml`, holds only the data directory, the log level
and the proxy's ports; the control plane manages everything else. Its log:

```sh
journalctl -u spinikus-agent -f
```
