# Metrics

Spinikus graphs how much CPU, memory and network each service uses, and how much each
server uses, disk included.

## A service's metrics

The service's **Metrics** tab has a graph each for:

- **CPU**, in cores: 0.5 is half of one CPU core, 2 is two whole cores.
- **Memory** in use, not counting page cache the system can take back (as `docker stats`
  counts it).
- **Network**: bytes received and sent per second.

Pick how far back to look above the graphs: 1 hour, 6 hours, 24 hours, 7 days or 30 days.
The latest value is beside each graph's title. Hover over a graph to see a moment's
values, or open **Table** under it for all of them. Times are in UTC.

The numbers are for all of the service's containers together. While a deployment replaces
another, the old and new containers both count, so a short bump at each deployment is
normal.

Anyone who can see the service can see its metrics. Databases have them too.

## A server's metrics

Instance admins see a server's metrics on its page, under **Settings → Servers**, in the
**Resources** section: CPU, memory (out of the server's memory) and network for the
whole machine, and **Disk**: the filesystem Docker keeps images, containers and volumes
on (usually `/var/lib/docker`), out of its size.

When that disk is 90% full, the server's page says so, and channels that get
`server.disk_low` are told (see [Notifications](../operating/notifications.md)). You're
told again only after it went below 85% and filled up again. Make room by removing old
images and build cache (`docker system prune`), volumes nothing uses, or by growing the
disk.

The agent reads the server's CPU, memory and network from Linux's `/proc`. On other
systems, such as a Mac running Spinikus for development, only the disk is graphed.

## How they are measured

Each server's agent looks at its containers and the machine every 15 seconds, and sends
the control plane each minute's average once a minute. If the agent can't reach the
control plane, it keeps the latest 3,000 of them (an hour's, with 50 containers) and sends
them once it can.

The control plane keeps every minute for 24 hours, and an average for each hour for 30
days. Ranges up to 24 hours are drawn from minutes, longer ones from hours, so an hour
appears in a 7-day graph within minutes of its end. Graphs average them further, to a
few hundred points: 1 minute each over an hour, 2 minutes over 6 hours, 5 minutes over 24
hours, an hour over 7 days, and 3 hours over 30 days. A gap in a graph is a time nothing
was measured: the service wasn't running, or the agent was away.

## The API

`GET /api/v1/services/{id}/metrics?range=24h` and `GET /api/v1/servers/{id}/metrics?range=7d`
(instance admins) return the points of a range (`1h`, the default, `6h`, `24h`, `7d` or
`30d`), oldest first, with `step_seconds`, the time each covers.
