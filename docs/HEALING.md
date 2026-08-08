# Self-healing

Off by default. Turn it on and openpages will, when a deployment breaks, put
the site back and then ask the Claude CLI on your machine to draft a fix.

The order in that sentence is the whole design.

## Three rules

**1 · Recovery never waits for it.** By the time anything in `heal.rs` runs, the
rollback has already happened. If the CLI is slow, missing, rate-limited or
simply wrong, the site is already back up. Healing is an extra, never a step in
the recovery path.

**2 · It is handed a report, not a log.** A failed `npm run build` can produce
four thousand lines, of which about nine matter. `incident::distill` strips ANSI,
drops progress and spinner lines, collapses repeats, finds the first real error
marker, and takes a window around it — capped at `report_budget_chars` (2,400 by
default). That is a cost decision *and* an accuracy one: a model given four
thousand lines of progress bars finds the wrong error about as reliably as a
person does.

**3 · It cannot loop.** Every attempt is fingerprinted on the normalised summary
line, with digits collapsed, so `failed after 1200ms` and `failed after 87ms`
are the same failure. A fingerprint that has been attempted before is skipped.
Each repository also has a daily budget (`max_per_repo_per_day`, 3). A broken
build that a fix does not repair costs one attempt, not one per push forever.

## What actually happens

```
build fails
  ▼
site untouched — a failed build has nothing to promote
  ▼
guards: enabled? mode? fingerprint unseen? budget left?
  ▼
clone the repository at the failing commit, depth 30
  ▼
claude -p "<distilled report + instructions>"   in that working copy
  ▼
git status --porcelain
  ├─ clean   → recorded as "no fix", with the model's one-line reason
  └─ dirty   → branch openpages/fix-<sha>, commit, push
                 mode = pr     → gh pr create   (default)
                 mode = direct → push to the site's branch, which redeploys
```

For a **runtime** failure — a live container that stopped answering — the
watchdog rolls back first and passes the rollback target's id into the report,
so the fix is drafted against a site that is already serving again.

## What Claude is told

The distilled report, plus:

> Fix the cause, then stop. Rules:
> - Change as little as possible. Do not refactor, reformat, or tidy anything unrelated.
> - Do not touch CI config, licences, or secrets.
> - A missing dependency belongs in the manifest, not vendored in.
> - If you cannot find a safe fix, change nothing at all.

It runs in a throwaway clone under `data_dir/heal/`, with your own Claude
credentials, on your own machine. Nothing is sent anywhere else.

## Configuration

```toml
[heal]
enabled = false          # the switch that beats every other setting
mode = "pr"              # pr | direct | off
claude_bin = "claude"
claude_args = ["-p", "--output-format", "json", "--max-turns", "14",
               "--permission-mode", "acceptEdits"]
max_per_repo_per_day = 3
timeout_secs = 900
report_budget_chars = 2400
heal_self = false        # also open PRs against openpages itself
```

`claude_args` is configurable on purpose: if the CLI renames a flag, you fix it
in the config instead of waiting for an openpages release. Output is parsed
leniently — JSON `result` if it is there, the last non-empty line otherwise.

Each site can override the mode: **inherit** (default), **off**, **pr**, or
**direct**. `direct` means openpages pushes to your branch, which triggers a
deployment of its own fix. It is genuinely useful and genuinely powerful, which
is why it is per-site and never the default.

## Disclosure

Repair pull requests say, in the body, that they were opened automatically by
openpages and that the change was drafted by the Claude CLI on that machine.
Commits carry no co-author trailer — the author is `openpages` — but a
repository owner reviewing a pull request is told plainly where it came from.

## What it will not do

- It will not run when `enabled = false`, whatever a site is set to.
- It will not push to a branch unless a site explicitly chooses `direct`.
- It will not push straight to the openpages repository, ever. `heal_self`
  opens pull requests only.
- It will not retry a failure it has already attempted.
- It will not delay a rollback by so much as one request.
