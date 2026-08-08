# openpages documentation

- **[Architecture](ARCHITECTURE.md)** — how a push becomes a live page, and what
  each piece is responsible for. Read this one first.
- **[Deploying it](DEPLOY.md)** — DNS, wildcard certificates, Caddy, the systemd
  units, backups.
- **[GitHub](GITHUB.md)** — openpages talks to GitHub through the `gh` CLI. What
  that buys, what it costs, and how to change it.
- **[Updates](UPDATES.md)** — compile quietly, verify by booting, swap when quiet,
  revert if wrong.
- **[Self-healing](HEALING.md)** — the rollback-first repair loop, everything the
  Claude CLI is given, and every guard around it.
- **[Nodes](NODES.md)** — building and serving on more than one machine.

Two commands away from a working install: see the [README](../README.md).
Something not working: `openpages doctor`.
