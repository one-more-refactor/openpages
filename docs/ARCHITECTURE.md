# Architecture

One binary. One SQLite file. One listening socket.

Everything below is a job that binary does, and most of them are a background
task rather than a separate process, because the coordination between them is
the interesting part and putting a network boundary in the middle of it would
only make it harder to reason about.

## A push, end to end

```
GitHub push
   │  webhook (HMAC-SHA256), or a poll when there is no inbound route
   ▼
deployments row, state = queued, pinned to one commit SHA
   │  claimed by exactly one worker — an UPDATE with `state = 'queued'` as the lock
   ▼
buildpipe                                         (src/buildpipe.rs)
   ├─ git fetch --depth 1 <sha>  · on the host, so the token never enters a container
   ├─ detect framework                            (src/detect.rs)
   ├─ container: install && build                 (src/runtime.rs)
   └─ verify: non-empty, contains HTML, under the size cap
   ▼
state = verifying → proof: does it actually serve `/`?
   ▼
state = ready — built, proven, and NOT live
   │  swap scheduler, every 10s                   (src/swap.rs)
   ▼
state = live · previous becomes `superseded` (the rollback target) in one transaction
   │  drain window
   ▼
previous container stopped; previous files stay until pruned
```

The split that matters is between **ready** and **live**. A deployment can sit
in `ready` for half an hour. That is what makes "build it now, switch later"
expressible at all, and it is why `promote` touches only the database — never
the filesystem.

## Why `promote` moves no files

Swapping a site is a transaction plus a cache clear:

```sql
UPDATE deployments SET state='superseded' WHERE site_id=? AND state='live';
UPDATE deployments SET state='live', promoted_at=? WHERE id=?;
```

Requests already reading the old deployment hold an open handle to a directory
that still exists. Nothing is renamed underneath them and no process restarts.
Rollback is the same operation in reverse, which is why it is fast enough to be
the *first* thing that happens when something breaks.

## The edge

One listener serves the panel and every site. The Host header decides:

| Host                                | Serves                                        |
|-------------------------------------|-----------------------------------------------|
| `openpages.example.com` (`panel_host`) | the panel and the API                      |
| `name.pages.example.com`            | that site's live deployment                   |
| `sha--name.pages.example.com`        | that specific deployment, live or not         |
| anything else                       | 404                                            |

Host classification runs as the outermost middleware, so a request arriving at
a *site* hostname never reaches an API route — not `/api/sites`, not
`/healthz`. That is one function, `edge::classify`, and it is unit-tested,
because it is the whole authorisation boundary between "a page you published"
and "the thing that publishes pages".

Static responses carry `x-openpages-deployment: <id>`. "The swap did not
happen" and "the swap happened and the page is still wrong" are otherwise the
same symptom.

### Traffic accounting

Every request bumps two things: a per-minute counter, flushed to `traffic`
every ten seconds, and an in-flight gauge held by an RAII guard so it comes
back down even if the client disconnects mid-response. The first tells the
scheduler what "quiet" means for this site; the second tells it whether right
now *is* quiet.

## The swap policy

`swap::decide` is pure and takes no I/O, so the question "did it pick the right
moment" is answered by a unit test rather than by watching production. In
order:

1. Policy is `immediate` → swap.
2. Waited past `swap_max_wait_secs` → swap. **A site that is never quiet still
   deploys.**
3. Zero hits in the last minute and nothing in flight → swap.
4. At least 30 historical samples **for this hour of the week**, and the current
   minute is at or below the configured quantile of them, and at most one
   request in flight → swap.
5. Otherwise wait.

Step 4's restriction to the same hour-of-week is the point. A site that is
quiet at 04:00 and busy at lunch has no useful "average"; comparing Tuesday
lunch against Tuesday lunch does.

The 30-sample floor exists because a quantile over ten points is noise, and
acting on noise means swapping at a moment that only looked quiet.

## Builds

`buildpipe` has no database handle and makes no calls to the panel. It takes a
`BuildJob` struct and a channel to write log lines into. That is the only
reason a remote node can run the identical code path with nothing but a
container runtime and `git`.

- The clone happens **on the host**, so the GitHub token is never inside the
  build container.
- A single commit is fetched, not a branch.
- The build runs in a container with a bind-mounted workspace. Under rootless
  Podman, container-root is the invoking user, so the output is readable;
  under Docker, `--user` is pinned instead. Both get `HOME` pointed somewhere
  writable, because package managers otherwise fail with errors that say
  nothing about permissions.
- Every log line is redacted against the credentials handed to the build before
  it is stored or displayed.

**Verification refuses three things** that all look like success: a missing
output directory, an empty one, and one with no HTML anywhere in it. The third
catches the most common real mistake — the wrong `output_dir` — which
otherwise publishes a blank site with a green tick beside it.

## Failure

A build failure never touches the live site: a failed build has nothing to
promote. A *runtime* failure — a container that stops answering — is caught by
the watchdog, which rolls back and only then considers healing.

The distiller (`src/incident.rs`) turns the log into a summary line, a bounded
report, and a fingerprint that is stable across re-runs (digits are
normalised, so `failed after 1200ms` and `failed after 87ms` are the same
failure). Everything downstream reads the distilled form. See
[HEALING.md](HEALING.md).

## Data

SQLite in WAL mode, one file, in `data_dir`. The interesting constraints:

- `claim_job` is an `UPDATE … WHERE state='queued' … RETURNING id`. Two workers
  racing cannot both win.
- `promote` is a transaction. There is no instant with two live deployments,
  and none with zero.
- `traffic` is one row per site per minute, pruned at 30 days.
- Node tokens are stored as SHA-256 hashes; the plaintext is shown once.
- Session cookies are opaque tokens; only their hash is stored.

Back it up by copying `data_dir`. The deployments directory can be regenerated
by rebuilding; the `.db` file cannot.

## What is deliberately absent

- **No message queue.** The queue is a table with three states and a claim
  predicate. At this size, anything else is more failure modes for the same
  behaviour.
- **No WebSockets.** Build logs are one-way, so they are server-sent events,
  which reconnect on their own. Nodes long-poll over plain HTTP, which works
  through anything and can be debugged with `curl`.
- **No agent daemon protocol.** A node speaks four HTTP endpoints.
- **No secrets manager.** Environment variables are plain text in the database,
  and the README says so rather than implying otherwise.
