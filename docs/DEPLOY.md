# Deploying openpages

## What the machine needs

- Linux with systemd
- **podman** (recommended, rootless) or **docker**
- **git**
- **[gh](https://cli.github.com)** — see [GITHUB.md](GITHUB.md)
- Optionally the **claude** CLI, if you want self-healing

`openpages doctor` checks all of it and names whatever is missing.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/one-more-refactor/openpages/main/install.sh | sh
```

You are asked four questions — panel, build agent, automatic updates,
self-healing — and then told exactly which files will be written. To see them
without writing anything:

```bash
openpages install --dry-run
```

Non-interactive:

```bash
sudo openpages install --yes \
  --components panel,agent,updater \
  --base-domain pages.example.com \
  --panel-host openpages.example.com \
  --public-url https://openpages.example.com
```

## Rootless, in your home directory

For a laptop, a trial, or a machine where you would rather not touch `/etc`:

```bash
openpages install --scope user --yes --base-domain pages.localhost --public-url ''
```

Everything lands under `~/.config/openpages`, `~/.local/lib/openpages` and
`~/.local/share/openpages`, the units are `systemctl --user` units, and no
account is created. `*.localhost` resolves to your own machine without any DNS
at all, so the panel is at `http://openpages.localhost:8788` straight away.

Self-updates work the same way — the updater restarts the user unit rather
than the system one. Uninstall by deleting those three directories.

## DNS

Two records, both pointing at the host:

```
openpages.example.com.   A   203.0.113.10
*.pages.example.com.     A   203.0.113.10
```

The wildcard is not optional if you want proof deployments: they live at
`<commit>--<name>.pages.example.com`.

## TLS

openpages speaks plain HTTP and expects a reverse proxy in front. A wildcard
certificate needs a DNS-01 challenge, so Caddy needs your provider's module:

```bash
xcaddy build --with github.com/caddy-dns/cloudflare
```

See [`deploy/Caddyfile.example`](../deploy/Caddyfile.example). The one rule
that matters: **do not rewrite the Host header.** It is how openpages knows
which site a request is for.

## The units

`openpages.socket` owns the listening socket; `openpages.service` is the
process. The split is what makes an update invisible — see
[UPDATES.md](UPDATES.md).

```bash
systemctl status openpages
journalctl -u openpages -f
```

The service runs as the `openpages` system user with `NoNewPrivileges`,
`ProtectSystem=full` and `ProtectHome=read-only`. Rootless podman under a
system unit needs a lingering login session for `/run/user/<uid>` to exist; the
installer enables it.

## Configuration

`/etc/openpages/config.toml`. Everything has a working default; an empty file
is valid. The fields worth knowing:

```toml
[server]
listen = "0.0.0.0:8788"
base_domain = "pages.example.com"       # sites live at <name>.<this>
panel_host = "openpages.example.com"    # beats any site with the same name
public_url = "https://openpages.example.com"   # unset ⇒ poll instead of webhooks
data_dir = "/var/lib/openpages"

[build]
runtime = "auto"                        # auto | podman | docker
max_concurrent = 2
timeout_secs = 1800
max_artifact_mb = 2048
keep_deployments = 20                   # per site, before old files are pruned

[swap]
policy = "quiet"                        # quiet | immediate
max_wait_secs = 1800                    # a never-quiet site still deploys
drain_secs = 20
quiet_quantile = 0.25
```

Changes need a restart: `systemctl restart openpages`.

## First sign-in

```bash
openpages login     # GitHub identity for this machine
openpages invite    # a code for the browser, valid 15 minutes
```

The very first code is also printed to the journal on first start:

```bash
journalctl -u openpages -n 40 | grep 'sign-in code'
```

## Backups

Everything that cannot be regenerated is in one directory:

```bash
systemctl stop openpages
tar czf openpages-$(date +%F).tgz -C /var/lib openpages
systemctl start openpages
```

`openpages.db` is the important part — sites, settings, history, sessions. The
`deployments/` directory is rebuildable by deploying again, so if you want a
smaller backup, exclude it.

Copying the `.db` while the service is running works because of WAL, but stop
it if you can. It takes a second.

## Moving to another machine

Copy `data_dir` and `/etc/openpages/config.toml`, install openpages there, and
repoint DNS. Webhook URLs follow `public_url`; if that changes, remove and
re-add each site so the hooks are rewritten, or turn webhooks off and let it
poll.

## Things that cost time here, so they need not cost yours

- **`PrivateTmp=yes` breaks rootless Podman.** Podman keeps per-user state under
  the real `/tmp`, so a private one makes `podman info` fail — with an error
  that never mentions `/tmp`. The generated units set it only on a machine that
  does not build.
- **openpages starts even with no container runtime.** It says so loudly in the
  log, on the overview, and under Press, and every build fails with the reason.
  A panel that crash-loops is a panel you cannot open to find out why.
- **A `--scope user` install is found by the CLI automatically.** Config
  resolution is `--config`, `$OPENPAGES_CONFIG`, `./openpages.toml`,
  `~/.config/openpages/config.toml`, `/etc/openpages/config.toml`. Without that
  fourth entry the service works while `openpages invite` reaches for
  `/var/lib` and fails naming the wrong file.

## When something is wrong

```bash
openpages doctor              # every dependency, one line each
systemctl --user status openpages   # or without --user for a system install
journalctl -u openpages -f
curl -s localhost:8788/healthz
curl -sI -H 'Host: name.pages.example.com' localhost:8788/ | grep x-openpages
```

That last one prints the deployment id actually serving the request, which
separates "the swap did not happen" from "the swap happened and the page is
still wrong".
