<div align="center">

# openpages

### Cloudflare Pages, on your own machine.

Link a repository, push to it, and the site is live at `name.pages.yourdomain`.
No account, no build minutes, no egress bill, no third party holding the
certificate for your own domain.

[![CI](https://img.shields.io/github/actions/workflow/status/one-more-refactor/openpages/ci.yml?branch=main&style=flat-square&logo=github-actions&logoColor=white&label=CI&color=17140f)](https://github.com/one-more-refactor/openpages/actions/workflows/ci.yml)
&nbsp;[![Release](https://img.shields.io/github/v/release/one-more-refactor/openpages?style=flat-square&logo=github&logoColor=white&color=17140f&label=Release)](https://github.com/one-more-refactor/openpages/releases/latest)
&nbsp;[![License](https://img.shields.io/badge/license-MIT-17140f?style=flat-square)](LICENSE)

![Rust](https://img.shields.io/badge/Rust-1.82+-17140f?style=flat-square&logo=rust&logoColor=white)
&nbsp;![Svelte](https://img.shields.io/badge/Svelte-5-17140f?style=flat-square&logo=svelte&logoColor=white)
&nbsp;![Runtime](https://img.shields.io/badge/runtime-rootless%20Podman%20%7C%20Docker-17140f?style=flat-square&logo=podman&logoColor=white)
&nbsp;![Binary](https://img.shields.io/badge/ships%20as-one%20binary-17140f?style=flat-square)
&nbsp;![Swap](https://img.shields.io/badge/swaps-when%20nobody's%20looking-c4321c?style=flat-square)
&nbsp;![Status](https://img.shields.io/badge/status-alpha-c4321c?style=flat-square)

</div>

---

> ## ⚠️ Alpha — finished, not polished
>
> openpages does what this page says — the deploys, swaps, self-updates and
> rollbacks below have all run on real machines. But it is a personal project,
> published as-is: **every release is an alpha**, there is no roadmap, and
> maintenance is best-effort.
>
> What that means in practice:
>
> - **Any future change may break without a migration path.** The SQLite schema,
>   the panel API and the agent protocol are not frozen; assume an upgrade can
>   mean wiping `data_dir` and re-linking your sites.
> - **It has only ever run on its author's machines.** The install path, the
>   webhook path and the swap scheduler have been exercised on a handful of real
>   deploys — not across the framework matrix the README lists, and not for long
>   enough to trust the traffic profile it learns.
> - **The self-updater replaces the binary that is serving your sites.** It
>   verifies by real boot and reverts itself, and that guard is tested — but it
>   is the piece with the most to lose. Read [`docs/UPDATES.md`](docs/UPDATES.md)
>   before enabling it on something you care about.
> - **Self-healing is off by default and stays that way** until you have read
>   [`docs/HEALING.md`](docs/HEALING.md). It opens pull requests against your
>   repository.
> - **Nobody has audited this but its author.** A build runs your repository's
>   code on your machine, and build environment variables sit in plain text in
>   SQLite — see the honesty notes below.
>
> Run it on a host you can rebuild, and keep the sites that matter somewhere else.

---

## Two commands

```bash
# 1 · on the server (asks what this machine should run, then writes the units)
curl -fsSL https://raw.githubusercontent.com/one-more-refactor/openpages/main/install.sh | sh

# 2 · give it a GitHub identity, and yourself a way in
openpages login && openpages invite
```

Point `*.pages.example.com` and `openpages.example.com` at the host, open the
panel, paste the code, pick a repository. That is the whole setup.

`openpages doctor` checks every piece and says which one is missing.

### Or try it on your laptop first

```bash
openpages install --scope user --yes --base-domain pages.localhost --public-url ''
```

No root, no DNS, nothing outside your home directory: `*.localhost` already
resolves to your own machine, so the panel is at
**http://openpages.localhost:8788** and a site called `blog` is at
`http://blog.pages.localhost:8788`. Removing three directories removes it.

## What it actually does

**Deploys, without the parts you did not ask for**
- **Detects the framework** from the lockfile and the manifest — Astro, SvelteKit,
  Next, Nuxt, Vite, Docusaurus, Eleventy, Gatsby, Hugo, Jekyll, mkdocs, plain HTML
  — and installs with a **frozen lockfile** so the build matches the commit.
- **Builds one commit, not a branch.** The exact SHA that fired the webhook, so a
  deployment labelled `a1b2c3d4` contains `a1b2c3d4` and nothing else.
- **Proofs before publishing.** A build that "succeeded" into an empty or wrong
  directory is caught and refused — that mistake otherwise ships a blank site with
  a green tick beside it.
- **Proof deployments** for every other branch, at `<commit>--<name>.pages.example.com`.
- **`_redirects` and `_headers`** work the way they do on Cloudflare, SPA fallback
  included.

**Swaps when nobody is looking**
- A built and proofed deployment is *standing by*, not live. openpages keeps a
  per-minute traffic profile for each site and puts the new one on press when the
  current minute is at or below that site's usual low **for that hour of the week**
  — not "quiet in general".
- **In-flight requests are counted, not guessed.** The previous deployment stays
  readable for a drain window, so a swap cannot cut a response in half.
- **A site that is never quiet still deploys.** The wait is bounded; when the
  deadline passes, it goes on press anyway.

**Updates itself the same way it updates your sites**
- Compiles or downloads the next version **in the background**, and on the source
  channel waits for the machine's load to drop before starting.
- **Verification is a real boot**, not a checksum: the staged binary migrates a
  throwaway database, serves on a throwaway port, and answers a request. If it
  fails, it is discarded and the running version never hears about it again.
- **Restarts through a systemd socket**, so connections queue in the kernel instead
  of being refused.
- **Reverts itself.** A version that cannot start is put back by its own second
  restart — see [`docs/UPDATES.md`](docs/UPDATES.md).

**Repairs, in that order**
- A broken deployment is **rolled back first**. Only then is a **short distilled
  report** — not the log — handed to the Claude CLI *on your machine*, which opens
  a pull request. Recovery never waits for a model, and the same failure is never
  attempted twice. Off by default. [`docs/HEALING.md`](docs/HEALING.md)

**One binary, four jobs**
- Panel, build agent, updater and healer are separate answers during install, not
  separate downloads. A build box runs the agent alone; the panel host usually runs
  everything. [`docs/NODES.md`](docs/NODES.md)

## The panel

Not a dashboard — a press sheet. Sites and deployments are ledgers, because that
is what they are, and the only status indicator is a **printer's colour bar**:
five patches, one per phase, filled as a deployment passes through them.
Vermilion means on press and means nothing else.

```
 spoon                                     4,182 hits / 24h   ▉▉▉▉▉   ON PRESS
 one-more-refactor/spoon-knife · main · astro

 notes                                        12 hits / 24h   ▉▉▉▉░   STANDING BY
 one-more-refactor/notes · main · sveltekit

 flick-web                                   904 hits / 24h   ▉▬░░░   PIED
 one-more-refactor/flick-web · main · vite     error TS2307: Cannot find module './lib/rsvp'
```

## Architecture

```
                    ┌──────────────────────────────────┐
                    │   Panel  ·  Svelte 5, embedded    │   press-sheet UI
                    └────────────────┬─────────────────┘   SSE build logs
                                     │
   Host: name.pages.example.com ─────┤
   Host: sha--name.pages.example.com │
                    ┌────────────────▼─────────────────┐
                    │   Edge         one listener       │   static + proxy
                    │   host → live deployment          │   traffic + in-flight
                    └────────────────┬─────────────────┘
                    ┌────────────────▼─────────────────┐
                    │   openpages    Rust · axum        │   SQLite (WAL)
                    │   swap scheduler · updater        │   one binary
                    │   healer · GitHub via `gh`        │
                    └────────┬──────────────┬──────────┘
                             │              │ long-poll, outbound only
              ┌──────────────▼───┐   ┌──────▼──────────────┐
              │  local agent      │   │  remote node         │
              │  podman / docker  │   │  podman / docker     │
              └───────────────────┘   └──────────────────────┘
```

Full map — data flow, the swap policy, trust boundaries:
**[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)**.

## Layout

| Path              | What                                                             |
|-------------------|------------------------------------------------------------------|
| `src/edge.rs`     | Host routing, static serving, `_redirects`/`_headers`, proxying   |
| `src/swap.rs`     | When a deployment takes the press, and rollback                   |
| `src/buildpipe.rs`| Clone → build → verify. No database, so a remote node can run it  |
| `src/update.rs`   | Staging, verification, the swap, and the self-revert guard        |
| `src/heal.rs`     | Rollback-first repair via the Claude CLI                          |
| `src/incident.rs` | Turning 4,000 lines of build log into the nine that matter        |
| `src/gh.rs`       | Every GitHub call, through the `gh` CLI                           |
| `web/`            | The panel — Svelte 5, embedded into the binary                    |

## Documentation

- **[Architecture](docs/ARCHITECTURE.md)** — start here.
- **[Deploying it](docs/DEPLOY.md)** — DNS, wildcard TLS, Caddy, the systemd units.
- **[GitHub](docs/GITHUB.md)** — why there is no GitHub App to register, and what
  that trades away.
- **[Updates](docs/UPDATES.md)** — the compile-verify-swap-revert cycle in full.
- **[Self-healing](docs/HEALING.md)** — what Claude is given, what it is not, and
  every guard around it.
- **[Nodes](docs/NODES.md)** — building on more than one machine.

## Building it yourself

```bash
cd web && bun install && bun run build && cd ..   # the panel is embedded
cargo build --release
sudo ./target/release/openpages install
```

Development: `cargo run -- serve` and `cd web && bun run dev` (the dev server
proxies `/api` to `:8788`). `cargo test` covers the swap policy, the log
distiller, path traversal, the update guard and job claiming — the parts where
being wrong is expensive and quiet.

## Honesty notes

- **Panel sign-in is a one-time code minted on the server**, not a GitHub OAuth
  redirect. Registering an OAuth app is exactly the setup step openpages exists to
  avoid; the trade is that adding a browser means running `openpages invite` on the
  machine. Fine for one operator, wrong for a team — [`docs/GITHUB.md`](docs/GITHUB.md)
  covers what a GitHub App would change.
- **Build environment variables are stored in plain text** in the SQLite file.
  openpages is not a secrets manager and does not act like one.
- **A build runs your repository's code** in a container on your machine, with
  network access. Rootless Podman is the default for that reason. Do not point it
  at repositories you would not run locally.
- **Rolling back a container deployment needs its image to still exist.** Static
  deployments have no such caveat, which is a real argument for static.
- **Linux only.** It is built around systemd sockets and Linux container runtimes.

## License

MIT — see [LICENSE](LICENSE).
