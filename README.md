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
```

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
--run is for already-built commands; --device requires separate device permission.
Don't nest internally admitted helpers or override machine cache policy.

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
worker jobs. Linux pressure/memory gates, cooperative native FIFO, stable leases,
owned process-group cancellation/recovery, bounded logs and atomic completion.
No sandbox, hard quotas, global legacy FIFO, universal command classification,
automatic cleanup, legacy history import or zero-downtime maintenance promise.

After meaningful use, summarize feedback in your task report AND create your own
[feedback](feedback/README.md) file using its template. Include job IDs and an
acceptance test for suggested improvements. "No issues observed" is valid; no
fabricated timings/coverage. Never include secrets or full private logs. See
AGENTS.md for shared-checkout, validation and authorization rules.
