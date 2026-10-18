# Task manager — shared workload coordinator (trial)

Standalone Rust/egui + SQLite/Unix-socket coordinator for local agent workloads.
**Authoritative source: `/home/abe/task-manager`**, separate from Tinline.
Read [TRIAL.md](TRIAL.md): agents must use the facility and provide honest feedback.
The deployed [safety/CLI update](SAFETY-UPDATE.md) caps default Cargo jobs at two,
adds memory/I/O-pressure gates, prints final run summaries and makes logs plain
text by default (`tasks logs ID --json` for scripts). Low host headroom queues
work intentionally; never bypass admission to force a test through.

```sh
# In the checkout you intend to build:
tasks run --who my-agent -- cargo check --offline --locked -p PACKAGE
tasks list
tasks admission
tasks logs JOB_ID
tasks cancel JOB_ID                 # your own job only
tasks list --active                 # compact table of unfinished jobs + live lock holders
tasks list --who my-agent --limit 5 # compact table; add --json for the same filter as JSON
tasks run --who my-agent --follow -- cargo test --offline --locked -j 2   # stream the log live
```

Plain `tasks list` (no options) still prints the full JSON response so existing
scripts keep working; any option switches to a compact table (`--json` keeps JSON,
filtered). The table abbreviates long `NAME=value` env arguments such as PATH for
display only; stored and executed argv are unchanged. `tasks run` prints the
resolved cwd and command on its `submitted job N;` line and warns (without
blocking) when other owners have unfinished jobs in the same cwd. In JSON replies
`execution_enabled` means "a worker process was alive at reply time", not "this
job ran"; `logs`/`approve-*` print the job's phase on stderr to make that clear.

Installed command: `/home/abe/.local/bin/tasks`. Shared on-demand daemon/state:
`~/.local/state/tinline-task-manager-native`. Extraction does not change the
running release, protocol, lock names, state or job history. No systemd install.
Leave TASK_MANAGER_STATE_DIR unset for normal shared work; XDG state overrides
still apply. Legacy jobs interoperate via the same actual flock files but aren't
imported into this job ledger. No automatic job replay or backlog approval.

`tasks run` explicitly submits/approves/starts/waits and returns child exit status.
Ctrl-C detaches, not cancels. For separate approval use enqueue, the returned
submitted_id, approve-command ID, start-worker. Commands execute in a **live cwd**
with the local user's rights, not a source snapshot/sandbox. Default compile slot;
--run is for already-built commands; --device requires separate device permission;
--offload is only for a `tasks-offload`/`offload-portable-desktop.sh` client
(own slots, no CPU PSI gate; scripts/README-offload.md).
Don't nest internally admitted helpers or override machine cache policy.

## Phone and emulator targets

`phone` = arm64-v8a only, `emulator` = x86_64 only; either name works wherever an
Android ABI/config is accepted and is stored as the existing canonical config.

```sh
tasks devices            # read-only adb view: kind, ready/unauthorized, ABI, lock free/busy
tasks phone-test /path/to/app-checkout [--device SERIAL] [--follow | --full-log | --tail N]
```

While waiting, `phone-test` prints each Android build step as it starts and, at
the end, only the last 40 log lines (`--tail N` to change, `--full-log` for the
previous full retained log, `--follow` to stream the whole log live).

`phone-test` builds the arm64 APK through the typed Android trial (running it is
the per-job trial approval), then a separate job holding only that device's lock
runs `adb install -r` (keeps app data) and opens the launcher activity. It never
uninstalls, downgrades or clears data. Running it is explicit permission to
install/launch on THAT device only. Without `--device` it needs exactly one ready
phone. Wireless `host:port` serials use the existing `host_port` lock names. The
window's Devices panel shows the same view and a "Test on phone" button.

Never-use devices: the user's personal phone `ZD22257P6G` is on a built-in
denylist; `run`/`enqueue --device`, approval, execution, device locks,
`phone-test` and any argv naming it are refused on every host. Configuration can
only add serials (`TASKS_DENIED_DEVICES`, `~/.config/tasks/denied-devices`).
`--device` jobs get `ANDROID_SERIAL` set to their device, and the worker starts
the adb server for them without lock descriptors. A test phone on the LAN worker:
deploy/worker/README.md "Test phone on the worker".

## Peer link (daemon to daemon over iroh; not deployed)

`tasks run --peer NAME [--run | --device SERIAL] [--fetch REL]... -- CMD` forwards a command
to another machine's daemon: this checkout is synced content-addressed (only missing blobs),
the peer runs it under ITS admission, approvals and device denylist, its log streams into the
local job and `tasks run` exits with the peer's exit code. Locally the job holds an offload
slot. A lost link is UNCERTAIN (re-attached by remote id, never resubmitted); cancel works
both ways; both ledgers record the link. Disabled unless `~/.config/tasks/peer.toml` exists
(`tasks peer init --bind LAN_IP:PORT`, then add `[[peer]]` entries with the other side's
`tasks peer id`); `tasks peer list` shows it, `tasks peer status-watch NAME` prints a peer's
live snapshots (usable as `TASKS_UI_REMOTE_CMD`). Design, config, failure semantics and what
remains before `tasks-offload` can go: [docs/design/peer-link.md](docs/design/peer-link.md);
LAN check: `scripts/peer-lan-e2e.sh`.

## Window

`tasks` (or `tasks ui`) opens a live dashboard of both machines: machine cards,
then Running / Queued / Recent tables with the agent (owner up to `@`) on every
row; click a row for details. The job form and devices are on their own tabs.
The worker card reads snapshots from the command in `TASKS_UI_REMOTE_CMD` (or the
first line of `~/.config/tasks/ui-remote-cmd`), e.g. `lan-peer status-watch …`;
without it the card says "not configured". `tasks ui --fixtures DIR` replays
snapshot files (e.g. `tests/fixtures/snapshot/ui-busy`) and contacts nothing.
Contract: docs/design/cluster-snapshot.md.

## Development

This is an independent Cargo workspace. No app checkout or workspace dependency
is required to compile the coordinator. The local repository starts uncommitted,
with no remote; creating commits/publishing needs explicit authorization.

```sh
cd /home/abe/task-manager
tasks list; tasks admission
tasks run --who coordinator-tests -- cargo test --offline --locked -j 2
tasks run --who coordinator-clippy -- cargo clippy --offline --locked -j 2 --all-targets -- -D warnings
tasks run --who coordinator-build -- cargo build --offline --locked -j 2
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -p test_wait_budget.py -v
# This orchestrates its OWN workers/admission; no outer tasks run:
python3 tests/native_commands.py "$PWD/target/debug/tasks"
python3 tests/worker_e2e.py "$PWD/target/debug/tasks" --timeout 300 --queue-timeout 0
```

Missing per-checkout target links are placed on the cache disk by the installed
manager. Don't share a target directory with another checkout, delete outputs,
replace the pinned runtime or restart a busy daemon to test a source change.
Use isolated state only for deliberate test instances.

## Extraction boundaries and retained compatibility

Source copied from Tinline worktree `tools/task-manager-next`, base commit
`01c57dd2e8a21c1811dc3ccd1b226e58cf89181c`, with uncommitted coordinator additions.
No source history was invented, and no old worktree/copy/build/state was deleted.
The old copies are historical; new development belongs here.

Package/library name `tinline-task-manager`, protocol/state/cache namespaces and
Tinline-specific immutable self-suite/Android adapters are intentionally retained.
Generic live command execution works outside Tinline. Those adapters still use
Tinline's tools/task-manager source layout: extraction does not magically make
the sealed self-suite accept a root-level package, nor give this unborn Git
repository a commit HEAD. Root-level immutable self-capture, a repository chooser
instead of the GUI's legacy source default, and configurable adapter namespaces
are follow-up work, not validated features. Use live commands for this repo now.

The old docs copied alongside source are historical evidence with Tinline paths.
[docs/EXTRACTION-README.md](docs/EXTRACTION-README.md) preserves the previous README;
NATIVE-DEPLOYMENT.md describes the installed pre-extraction release. Prefer this
README/TRIAL.md for current source location and policy. The old real x86_64 APK
pass is NOT native/dual-ABI/device acceptance. No install/publish/data wipe is
part of this trial.

## Limits and feedback

One compile slot, four run slots, exclusive device slots; at most five in-flight
worker jobs, plus four offload-client slots with their own four-job budget.
Linux pressure/memory gates, cooperative native FIFO, stable leases,
owned process-group cancellation/recovery, bounded logs and atomic completion.
Job processes inherit their slot lease; a daemon that escapes the job keeps it.
When RUSTC_WRAPPER is sccache the worker starts its server outside the job (no
lease descriptors) first. Any other leaked holder is named (pid, command,
originating job) in `tasks status`/`admission`/the waiting log; it is never
stolen or killed: the slot frees when that process exits.
No sandbox, hard quotas, global legacy FIFO, universal command classification,
automatic cleanup, legacy history import or zero-downtime maintenance promise.

After meaningful use, summarize feedback in your task report AND create your own
[feedback](feedback/README.md) file using its template. Include job IDs and an
acceptance test for suggested improvements. "No issues observed" is valid; no
fabricated timings/coverage. Never include secrets or full private logs. See
AGENTS.md for shared-checkout, validation and authorization rules.
