# Nodes

A node is a machine that builds and serves. The panel host always has one,
called `local`. Add more when builds are heavy enough to bother the panel, or
when a site should be served from somewhere else.

## Enrol one

In the panel, under **Nodes**, name the machine and mint a token. It is shown
once; only its hash is stored. Then, on the other machine:

```bash
curl -fsSL https://raw.githubusercontent.com/one-more-refactor/openpages/main/install.sh | sh
# choose: build agent, not the panel
```

or directly:

```bash
openpages agent \
  --panel https://openpages.example.com \
  --token n_ab12cd.<secret> \
  --edge-url http://10.0.0.5:8789
```

It appears in the panel within a minute.

## How it talks

**Control traffic is outbound only.** The node long-polls `GET /api/node/job`,
which simply does not return until there is work or twenty-five seconds pass.
Logs and results are posted back. Nothing listens for the panel to connect, so
a node behind NAT, on a laptop, or on another continent needs nothing
forwarded.

**Edge traffic is inbound**, and this is the one thing a node does need to be
reachable for. When a visitor asks for a site whose live deployment was built
on a node, the panel proxies to that node's `edge_url` with a header saying
exactly what to serve:

```
x-openpages-target: static:blog/412
x-openpages-target: container:38211
```

The node has no database, so it is told rather than asked to look up. A request
to a node's edge without that header is refused — it is not a general proxy.

Give `--edge-url` an address the panel can reach: a LAN address, a tailnet
address, a private VPC address. If the node is genuinely unreachable, keep its
sites on `local` and use the node for building only.

## Failure

A node that dies mid-build leaves a deployment in `building`. Results are
retried with backoff for about two minutes before the node gives up — losing a
log line is survivable, losing the result is not.

A node that goes away entirely shows its last-seen time in the panel. Sites
pinned to it stop serving, because their files are on a machine that is gone.
Rebuild them on another node by changing the site's node and deploying.

## Choosing where a site builds

Each site is pinned to one node, chosen when it is created. A site's
deployments are always built *and served* by the same node, because the
artifact only exists there.

Running the panel with no local agent at all:

```bash
openpages serve --no-agent
```

which the installer writes into the unit for you if you answer "no" to the
build agent question. The panel then only routes and schedules; every build
goes to a node.
