# Updating openpages

openpages updates itself on the same terms it gives your sites: build it out of
the way, prove it works, take the press when nobody is looking, and be able to
undo it.

## The cycle

```
check          latest release tag vs this version                    (hourly)
  ▼
stage          release channel: download + verify the sha256
               source channel:  wait for load to drop, then
                                cargo test && cargo build --release, nice -n 19
  ▼
verify         run the staged binary: `--version`, then `selftest` —
               which migrates a throwaway database, starts the whole
               server on a throwaway port, and answers a real request
  ▼
[ any failure above → discard, record it, keep running. The running
  version is never touched by a version that did not pass. ]
  ▼
quiet          wait until no requests are in flight, bounded by swap.max_wait_secs
  ▼
apply          write the pending marker
               atomically relink /opt/openpages/bin/current → the new binary
               systemctl --no-block restart openpages.service
  ▼
confirm        the new version runs for health_window_secs → marker cleared
```

## Why the restart is not an outage

The service is socket-activated. `openpages.socket` owns the listening socket
and stays up across the restart, so connections arriving during the swap queue
in the kernel's accept backlog instead of being refused. The process is
replaced; the socket is not.

## Why "it compiles" is not the check

Compilation proves syntax. It says nothing about a migration that fails on a
real database, a route that panics on registration, or a config field that was
renamed. So verification is a **boot**: the staged binary runs `openpages
selftest`, which does the real startup path against a scratch data directory
and then makes an HTTP request to itself. The same command runs in CI before a
release is published, so a binary that cannot boot never becomes a download.

## Reverting a version that cannot start

Verification happens before the swap, so this should not occur. It is handled
anyway, because "should not occur" is not a recovery plan.

Before relinking, openpages writes `pending-update.json` recording the previous
binary. The systemd unit runs:

```ini
ExecStartPre=/opt/openpages/bin/current update guard
```

Each start increments an attempt counter in that marker. On the third attempt
the guard relinks `current` back to the previous binary, deletes the marker,
and prints what it did to the journal. systemd's own `Restart=always` then
brings the service up on the version that worked.

So: two failed starts, and the machine has fixed itself. This is
`update::tests::the_guard_reverts_only_after_repeated_failures_to_start`, and
it is tested precisely because it is the code that runs when nothing else does.

If the healer is enabled with `heal.heal_self = true`, a failed *build* of a new
openpages version also opens a pull request against the openpages repository.
That mode is always `pr`, never `direct` — nothing pushes straight to the tool
that runs everything.

## Channels

```toml
[update]
channel = "release"   # download the published binary, checksum verified
# channel = "source"  # compile it here
```

**`release`** is the default. The asset name is `openpages-<arch>-linux`, pinned
in both the updater and the release workflow, and verified against the
published `.sha256`. If a release ships without a checksum, openpages warns and
proceeds — say so out loud rather than silently skipping the check.

**`source`** compiles on the machine. It is the reason `load_threshold` exists:
nothing starts until the one-minute load average per core is below it (0.5 by
default), the build runs at `nice -n 19` on half the cores, and it runs
`cargo test` before `cargo build`. A version that does not pass its own tests
is never staged.

## Controls

```toml
[update]
enabled = true              # off = never check
auto_apply = true           # false = stage and verify, then wait for you
check_interval_secs = 3600
health_window_secs = 120    # how long a new version must survive to be confirmed
install_dir = "/opt/openpages/bin"
service = "openpages.service"
```

With `auto_apply = false`, a verified version appears in the panel under
**Press** as *standing by* and swaps when you press the button.

From the command line:

```bash
openpages update check
openpages update status     # the last twenty events, with reasons
```
