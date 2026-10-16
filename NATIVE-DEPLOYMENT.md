# Shared native Rust coordinator

## Agent entry point

`/home/abe/.local/bin/tasks` is installed on this host. Default shared state:
`/home/abe/.local/state/tinline-task-manager-native`. The CLI starts that daemon
on demand. No systemd service was installed. The previous v1/v2 state and daemon
are separate and were not migrated, stopped or replaced.

```sh
# Foreground convenience: explicitly submit + approve THIS live command + start
# the bounded worker pool; prints job ID, waits, returns requested exit status.
tasks run --who your-agent -- cargo check --offline --locked -p p2pcore
tasks run --who your-agent -- cargo build --release -p desktop
# Already-built tests/benchmarks (not a compile):
tasks run --who your-agent --run -- /absolute/path/to/test-binary
# Only for separately authorized hardware work:
# tasks run --who your-agent --device SAFE_SERIAL -- /absolute/path/to/command ...

# Another agent, another terminal, any checkout:
tasks list                  # durable owners/specs/phases/results/runtime observations
tasks admission             # read-only native resource/pressure/holder/waiter JSON
tasks logs JOB_ID            # bounded retained output tail
tasks cancel JOB_ID          # cancel only your own work

# Deliberately separate submission, approval and worker-start:
tasks enqueue --who your-agent -- cargo check --offline --locked -p desktop
# Use response.submitted_id, NOT the largest ID in response.jobs: concurrent
# clients can submit while your request is being answered.
tasks approve-command JOB_ID
tasks start-worker
```

Do not set `TASK_MANAGER_STATE_DIR` for ordinary shared work. Explicit overrides
are for isolated tests/instances; they do not query the shared ledger. Normal
XDG state-directory conventions still apply; the canonical deployment above uses
HOME with no XDG_STATE_HOME override. `execution_enabled` in the old v2 response
means **worker currently running**, not whether the daemon supports native jobs.

`tasks run` does not auto-approve other proposals. Explicit worker start drains
already-approved jobs, with at most five in-flight executions. Ctrl-C detaches
the client, NOT the durable job. Query the printed ID before retrying; cancel
that ID to stop its owned process group. There is no blind mutation replay,
automatic failed-job retry, or automatic replay after interruption.

## What moved to Rust

`src/admission.rs` replaces runtime calls to Python buildlock:

- One compile slot; four run slots; exclusive, validated device serial slots.
  A device command takes its device slot AND a run slot.
- Linux MemAvailable >= 4 GiB and CPU PSI some avg10 <= 40 for compile/run.
  Missing/malformed pressure refuses work instead of silently admitting it.
- Stable `flock` files under `/tmp/claude-UID` match existing Python names
  (`p2p-build.lock`, `p2p-run-N.lock`, `p2p-device-SERIAL.lock`). No lock inode is
  removed/replaced to force admission. Existing Python jobs remain protected.
- Native waiter ordering is cooperative FIFO by request-note order, NOT a global
  FIFO guarantee against legacy clients. Legacy PID-only notes are observations,
  not ordering authority: reused/stale PIDs must not block native admission.
  Native notes AND wait filenames bind process start ticks, avoiding stale
  SIGKILL-note collisions after PID reuse. Status probes the actual lock, not stale
  holder text; it does not clean up other clients' metadata or create missing roots.
- Admission descriptors survive command exec/owner cleanup; existing dedicated
  group supervision/watchdogs handle cancellation and parent/worker loss. Only
  owned groups are signalled. Target/worker descriptor leases remain in place.
- A single explicit worker invocation claims up to five jobs with independent
  SQLite connections; compile/run admission remains global. Infrastructure error
  stops new claims but lets other already-approved jobs finish. Signals cancel
  owned active work. Idle workers exit; the query daemon remains.

No Python buildlock subprocess exists in the worker -> supervisor -> admission
-> command path. Python may still be the explicitly requested build tool, e.g.
the libaom builder; this is not Python admission.

## Live command contract versus immutable suites

`local.command` is a NEW, separately authorized contract. `approve-command` is
required; normal `approve` and Android-trial approval cannot authorize it. argv,
owner, resource class and absolute cwd are persisted in the ledger. `revision`
is explicitly `live-workspace`, execution.snapshot is absent. A zero exit status
means only that the requested command succeeded, NOT an inferred test suite,
source snapshot, verified artifact or APK result. Completion/history/cancel
precedence commit atomically. Failed/cancelled commands don't teach success.

These commands run with the local user's rights and can write to the LIVE cwd.
They are not sandboxed. Keep each checkout stable while its Cargo compile/run
pair executes. Plain Cargo test invocations split into compile/no-run and requested
run stages, as in the legacy helper; a mutable workspace can cause Cargo to
rebuild during run, so don't edit or concurrently reuse that checkout. Other
commands use their declared resource class. Commands must remain foreground;
background/daemonized descendants can retain admission descriptors after the
requested parent returns. There is no universal command classification/sandbox.

No arbitrary caller environment is silently persisted/copied from agent shells.
Use explicit non-secret `env NAME=value ...` argv inputs when needed (e.g. VPX_DIR,
VPX_STATIC_REQUIRED). Never put credentials in argv or override machine wrapper,
Cargo-target or Gradle-home policy. Machine Cargo configuration supplies sccache.
The backend supplies canonical shared Gradle home if no override exists. Raw
Git/Rust workspaces get missing per-checkout target/Android output links on the
cache disk; existing directories/links are not migrated or deleted. New mappings
use canonical checkout path hashes rather than potentially colliding basenames.

Do NOT nest `scripts/buildlock.py`, `scripts/build_android.py`, or managed worker
executors inside another compile admission. Known direct helper arguments are
rejected; arbitrary user shell/build scripts cannot be proven free of nested
admission. Legacy helpers remain compatible but their jobs aren't imported into
this ledger; `tasks admission` observes their current locks, not historical logs
or outcomes. No cleanup, hard disk/memory quota, sandbox or fleet guarantee was
added. Raw live commands have no immutable-suite target-retirement contract.

The existing sealed self-suite and experimental Android suite remain separate.
Android normal catalog approval stays gated. The prior real x86_64 APK passed
under the old Python admission path; native-admitted Android/dual-ABI APK validation
has NOT been performed. Installation/publishing/device-data operations are NOT
part of this deployment.

## Evidence

- Bootstrap through the existing queue: **90 Rust tests passed, 0 failed/ignored**,
  clippy warnings-denied and build passed; `/tmp/task-manager-native-deploy-checks.log`.
- Real native tests, each acquiring admission internally (NO outer queue):
  Final deployed-source run `/tmp/task-manager-native-regressions-complete.json`,
  all three exit 0 (earlier final/hardened runs also passed):
  - `tests/native_commands.py`: independent Agent-B client queries Agent-A's two
    concurrent native run jobs, logs/results; real tiny Cargo compile/run; failure,
    cancellation, no false history; `tasks run` returns actual exit code 9.
  - `tests/worker_e2e.py`: approval/backlog, source sealing, daemon result recovery,
    target/disk/budget refusals, cancellation, worker/supervisor SIGKILL, lease
    release and tamper checks all passed.
  - `tests/self_suite.py`: actual captured manager source compiled and all harnesses
    passed; not a model-only claim. `/tmp/task-worker-self-suite-4q30yrf1`.
- Initial native test run hit a transient fork-inherited fd in an IDLE expectation;
  bounded idle-only retry fixed the fixture. Active exclusivity checks stayed
  immediate. Initial clippy found a collapsible if; fixed, no warnings suppressed.
  Historical exit-101 logs remain, not relabelled as passing.
- First deployed acceptance `/tmp/task-manager-native-shared-smoke.json` passed:
  independent clients on DEFAULT shared state, running/log/result queries, inspected
  real owned process tree contains __stage -> __admit -> __command -> shell and no
  Python/buildlock. Job 1 passed. Old daemon PID 611058 left alone.
- Actual deployed `cargo check --offline --locked -p p2pcore` (job 2) and
  `cargo check --offline --locked -p desktop` (job 3) PASSED through native shared
  admission. These are compiler checks, NOT release builds, protocol/network tests,
  application launch or phone integration. Logs `/tmp/task-manager-native-core-check.log`
  and `/tmp/task-manager-native-desktop-check.log`; query durable job rows.
- A follow-up queue hardening excludes legacy PID-only notes from FIFO ordering.
  It was built/tested THROUGH THE DEPLOYED Rust coordinator as job 4:
  `/tmp/task-manager-native-queue-hardening.log`, all 90 tests, clippy/build exit 0.
  A final process-start-bound waiter filename fix was validated through shared
  job 6, `/tmp/task-manager-native-identity-checks.log`, all 90 tests/clippy/build
  exit 0. Final CLI/E2E/captured-self suites in
  `/tmp/task-manager-native-regressions-complete.json` all passed.
- Deployment manifest: `/tmp/task-manager-native-deployment.json`; immutable copied
  runtime releases under `~/build-cache/p2p_chat/tasks/releases/SHA256/` (not active
  Cargo targets). Preserve these assets/results; no automatic release cleanup.
  Repository code remains uncommitted; no staging, commits, merges or pushes.

Final release was promoted only while its own native worker/cleanup lock was
idle and all shared jobs terminal. Only the newly deployed native daemon was
updated; the original daemon/device sessions remained untouched. All prior job
rows survived both controlled native-only promotions. FINAL binary SHA-256:
`c3a9c31dea6ace2cffe1c4b558f4e89f239f3931e5fdadd7efeefdee65d2705b`.
Final source fingerprint:
`f8195376e2ab5348d7f872152fe270360d8020a8332842d386153d6debad6d84`.
Final deployed two-client/process-tree smoke passed, durable job **7**;
`/tmp/task-manager-native-shared-smoke-final.log` and
`/tmp/task-manager-native-shared-smoke.json`. No active owned validation remains.
The main checkout's AGENTS.md now routes new direct work through the Rust command;
other agents' worktrees and already-loaded instructions weren't forcibly changed.

Next: coordinate remaining agent adoption and add richer UI/query/maintenance
controls. Native-admitted typed Android/dual-ABI APK checks remain separate.
Do not confuse current legacy-lock observations with fully migrated agent traffic.
Keep live cwd stable, use explicit non-secret build inputs, and don't infer sealed
artifact guarantees from raw-command success. Large job-history pagination,
zero-downtime maintenance and hard quotas are not implemented.
