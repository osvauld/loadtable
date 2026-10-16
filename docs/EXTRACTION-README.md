# Tinline task manager — explicit local execution

Active worktree: `/home/abe/p2p_chat/.claude/worktrees/task-manager-next`,
branch `tools/task-manager-next`. This contains the user-selected implementation
with the most additions, previously in the main dev checkout. The original
`.claude/worktrees/task-manager` v1 snapshot remains untouched. This is a standalone
Rust workspace with egui/eframe UI and SQLite/Unix-socket control plane.

## Shared native deployment (current)

`/home/abe/.local/bin/tasks` is installed on PATH, using a new shared native
Rust daemon/state at `~/.local/state/tinline-task-manager-native`. Existing task
services/states remain separate. Runtime admission no longer calls Python
buildlock; stable flock files protect legacy active jobs during migration.

```sh
tasks run --who your-agent -- cargo check --offline --locked -p p2pcore
tasks run --who your-agent -- cargo build --release -p desktop
tasks list                  # another agent can query jobs/progress/results
tasks admission             # actual native/legacy locks and pressure
tasks logs JOB_ID
tasks cancel JOB_ID         # your own work only
```

These are explicitly approved **live cwd/argv** jobs, not immutable source suites.
Use `tasks enqueue ...`, `tasks approve-command ID`, `tasks start-worker` for
separate permission steps. Read response.submitted_id for your submission; never
infer ownership from the highest listed ID. Do not nest admission helper scripts.
See `NATIVE-DEPLOYMENT.md` and `AGENT-WORKFLOW.md` for commands, real validation,
release identity, trust boundaries and limits. Code is still uncommitted.

The following section describes the original **sealed self-suite** contract,
which is separate from live-command execution.

## First immutable executable suite

| Field | Allowed value |
|---|---|
| suite | task-manager.tests |
| configuration | debug/offline |
| cost | quick |
| source | Absolute Git repository root containing tools/task-manager |
| revision | Full current 40-character Git HEAD |

`tasks catalog` also lists proposed `core.tests.compile`, `desktop.build` and
`android.assemble.debug` contracts with `executable: false` and explicit blockers.
Normal approval/execution remains gated. Android has a separate explicit
experimental build-only approval for first-APK validation; it is not production
rollout. See `ANDROID-EXECUTION-MILESTONE.md` and `AGENT-WORKFLOW.md`.

These named source-capsule catalog entries remain proposals except the explicit
Android trial route above. That does not disable the separate, explicitly
approved `local.command` live-workspace route. This sealed suite itself does not
accept arbitrary commands or device selectors.

**Approval is permission to execute trusted local Rust code with your account
rights, not a sandbox.** Tests and registry-dependency build scripts can perform
I/O. Offline prevents Cargo dependency downloads, not networking by Rust code.
Do not approve an untrusted repository. Phone/stress/benchmark/project-core suites
are not enabled as immutable catalog contracts in this milestone; explicitly
approved live commands are the separate route described above.

### Source identity

At worker pickup, verify source root/current HEAD and capture the suite's manifest,
lockfile and Rust files under src/tests, including dirty/untracked files. Two
content inventories must agree; SHA-256 covers paths, bytes and snapshot schema.
Maximum 256 files / 4 MiB. Files/directories are sealed read-only and reverified
before each admitted stage. Later source edits do not change the captured input.

The Git HEAD is provenance; the snapshot digest identifies the actual dirty
suite input. Capture occurs at pickup, **not submission/approval time**. Two reads
are not an atomic filesystem snapshot or defense against a hostile same-user
writer. The conservative source contract rejects symlinks/special files, build.rs,
.cargo, local/git/workspace dependencies, overrides and escaping target paths.
Only Rust source plus manifest/lock are captured; external include files, assets
and doctests are not supported. Registry crates/toolchain/user Cargo config remain
trusted local inputs, not a reproducible hermetic environment.

## Usage

~~~sh
export TASKS=/home/abe/.local/bin/tasks
"$TASKS"                        # opens the window; auto-starts daemon only
"$TASKS" catalog                # distinguish executable entries from proposals
"$TASKS" cache                  # read-only cache policy/free-space diagnostics
"$TASKS" cache-plan             # read-only retirement proposals; never deletes
"$TASKS" submit SPEC.json       # still only a proposal
"$TASKS" approve JOB_ID         # explicitly permits this catalog job
"$TASKS" start-worker           # background worker drains approved jobs, then exits
"$TASKS" list
"$TASKS" logs JOB_ID            # JSON response includes bounded log tail
"$TASKS" cancel JOB_ID          # queued cancellation or active cancel request
"$TASKS" worker                 # alternatively foreground worker; Ctrl-C cancels its job
"$TASKS" record SUITE CONFIG STAGE SECONDS  # manual runtime reports; worker.* reserved
~~~

Example SPEC.json (use the actual current HEAD):

~~~json
{
  "suite": "task-manager.tests",
  "owner": "your-agent",
  "source": "/home/abe/p2p_chat/.claude/worktrees/task-manager-next",
  "revision": "01c57dd2e8a21c1811dc3ccd1b226e58cf89181c",
  "configuration": "debug/offline",
  "cost": "quick"
}
~~~

The window has the same submit, **Approve trusted source**, **Start local worker**,
Cancel and Details/logs actions. Approval and worker start are separate. Starting
the daemon/window never approves the old backlog. Worker stops after two idle
seconds; approve later jobs then start it again. No background executor is
silently restarted or enabled.

### Android preparation (not execution)

`tasks android-plan SOURCE FULL_HEAD [ABIS]` audits the current Rust/Android source
and installed toolchain markers, then prints a gated build-only pipeline. Default
ABI is x86_64; arm64-v8a and x86_64,arm64-v8a are also typed options. It never
starts a daemon/build/download or touches a device.

`tasks android-capture SOURCE FULL_HEAD NEW_DESTINATION` copies and verifies a
bounded, read-only source capsule outside the source checkout. It includes native,
Kotlin, generated icon inputs and assets, but excludes build outputs/local SDK
properties. This is a separate contract from the executable self-suite snapshot.
Neither preparation command enables job execution. Managed Android lanes and
APK results now exist behind explicit `approve-android-trial JOB_ID`; normal
catalog approval is still disabled. An earlier real x86_64 build passed under
Python admission after space recovered; native-admitted APK and dual-ABI
validation remain future work after the native coordinator rollout. See
`ANDROID-EXECUTION-MILESTONE.md` for trial commands, host trust/identity limits
and validation. `ANDROID-MILESTONE.md` records earlier source-preparation proof.

### Existing daemon upgrade

Socket protocol is now version 2. An older live daemon may still own your default
state socket; a newly built client reports a version error rather than replacing
it. Inspect its active work and restart it deliberately, or use a separate state
directory. Nothing here kills or wipes an existing daemon/database for you.

For an isolated session (no PATH installation needed):

~~~sh
export TASK_MANAGER_STATE_DIR="$HOME/.local/state/tinline-task-manager-v2"
"$TASKS"
~~~

Normal native state defaults to XDG_STATE_HOME/tinline-task-manager-native or
~/.local/state/tinline-task-manager-native. The old namespace is not migrated. State must be a private real directory, not a
symlink. Daemon creation makes it mode 0700. Do not select a directory containing
unrelated data. TASK_MANAGER_TARGET_DIR may select an absolute **dedicated**
artifact cache outside the captured source. The default reusable target is
`~/build-cache/p2p_chat/tasks/targets/self-tests`, shared between cooperating
workers using an exclusive target lease. Existing `state/artifacts` directories
are not migrated or deleted. Do not point it at legacy target directories used
by agents/builds that do not honor this lease.

### Sealed self-suite cache and disk policy (not the live-command contract)

- After environment sanitization the worker explicitly sets `RUSTC_WRAPPER` to
  the discovered absolute sccache executable, `SCCACHE_DIR=~/build-cache/sccache`
  and `SCCACHE_CACHE_SIZE=30G`. This retains the self-suite's older owner-defined
  cache settings; new live commands inherit machine Cargo configuration instead.
  Missing sccache is recorded as an uncached build; nothing is installed for you.
- Disposable validation builds use `CARGO_INCREMENTAL=0` and no dev/test debug
  information. Active development builds outside this worker are unchanged.
- Different states reuse one default self-test target instead of creating a new
  permanent target per agent. The target lease spans build, run and owned-group
  cleanup and is inherited through the native admission boundary. It only
  protects cooperating workers; direct Cargo/legacy builds must not use it.
- Before capture/admission the worker waits (cancellably) for **20 GiB available**
  on target/sccache filesystems and **256 MiB** on the state filesystem. The
  state reserve is smaller because state may be on tmpfs; it does not hold the
  default build outputs. Waiting reasons appear in phase detail and job logs.
- Disk availability is checked again after compile-slot admission. If it falls
  below reserve while queued, the job fails before spawning Cargo. This is a
  point-in-time gate, not a filesystem quota or reserved allocation; other
  programs can still consume space during compilation.
- `tasks cache` reports policy/free space without starting a daemon or creating
  targets. The sccache size is a requested server configuration, not a budget
  for Cargo targets; an already-running sccache server may retain its initial
  configuration. No cache hits are invented or treated as test results.
- Self-test targets have an **8 GiB allocated-space admission ceiling**, checked
  under the lease before capture and after compile admission. Incomplete scans
  refuse work. This is not a hard quota: a build can grow beyond it; the next job
  fails before Cargo and requires review/new submission rather than auto-cleanup.
- `tasks cache-plan` protects active, unknown, malformed or unfinished targets.
  Workers initialize provenance only for empty selected targets; populated legacy
  caches are never auto-adopted. Clean idle managed targets become review
  candidates when over budget or unused for 7 days. Plans use bounded metadata
  scans and never follow static symlinks, inventory active targets, or recursively
  scan legacy roots. Missing/incomplete data is not treated as zero usage.
- No deletion/apply operation exists. Existing caches remain untouched. Plans do
  not authorize deletion, include user results/logs/snapshots, or prove an atomic
  filesystem snapshot. Preserve `.tasks-target.lock`, owner and usage metadata;
  do not run `cargo clean` against another job's leased target. See
  `RETENTION-MILESTONE.md` for exact scope/limits and the future cleanup contract.

## Execution and safety

~~~text
queued proposal → approved → preparing → waiting_build → building
                                     → waiting_run → running → passed / failed
active job + cancellation → cancelled (after owned-group cleanup)
claimed job + lost worker/supervisor → interrupted (never automatically retried)
~~~

- Linux-only local worker; one worker/cleanup lease per state directory.
- Native Rust admission performs real CPU/memory and compile/run/device locking,
  compatible with legacy flock files. No Python buildlock runtime call remains.
  One explicit worker pool has up to five in-flight jobs. Agents invoke `tasks`;
  do not acquire an outer admission lock around managed executor jobs.
- The following harness/artifact guarantees apply to the sealed self-suite only.
  Live commands have the distinct trust/result contract in NATIVE-DEPLOYMENT.md.
- Build: cargo test --locked --offline --lib --bins --tests --no-run with Cargo
  JSON artifact records. Run: verified SHA-256 test harness executables directly
  under a run-only slot. No Cargo compilation occurs in the run-only stage.
- Results run fresh harnesses; they are never reused as successful test results.
  Reusable leased Cargo targets and shared compilation caching are allowed.
- Cancellation is a durable request. A supervisor signals only its child group,
  with PID/start-time/group checks, TERM then KILL. Supervisor/command retains the
  worker lease during cleanup. A watcher detects worker/supervisor loss. Tests
  cover single-process crashes; hostile programs escaping groups, simultaneous
  multi-level kills and power failure are not guaranteed cleanup scenarios.
- Restart reconciliation occurs only with the exclusive worker lease. Interrupted
  jobs retain logs/artifacts and require new submission, never an automatic rerun.
- Logs capped at 8 MiB/job; socket tail capped at 32 KiB. No automatic deletion,
  artifact pruning, worktree retirement or cleanup is performed.
- SQLite completion and successful timing learning commit together. Failed,
  cancelled and interrupted jobs do not teach successful runtime history.

Build/run runtimes and admission waits are separate. The window shows real phases,
elapsed wall time, stage/result details and log tails. At ordinary window widths,
secondary job metadata is on the suite hover and runtime spread is on the estimate
hover. Logs default to readable compiler diagnostics and build/test summaries;
**Raw Cargo JSON** exposes the original tail without changing retained logs.
It prefers worker-observed
runtime history over manual reports, excludes wait histories from estimated run
duration and never invents percentages or scheduled completion times. Estimates
are local observed totals for recorded stages, not a guaranteed finish ETA; cache,
machine load/toolchain and source changes can affect them. No per-test push progress
or distributed scheduling is implemented.

Use the installed pinned release as the shared manager, not transient binaries
inside Cargo job outputs. Native runtime no longer depends on an integration
script path. The worker/cleanup lease excludes recursive workers on the same
state; neither snapshots nor that lease sandbox trusted same-user code.

## Development checks

From the active worktree. The installed release is an independent copy, so a
rebuild does not overwrite a binary being executed by shared workers:

~~~sh
cd /home/abe/p2p_chat/.claude/worktrees/task-manager-next
# target/ is this checkout's canonical cache-disk symlink, not another worktree's.
tasks run --who task-manager-tests -- \
  cargo test --locked --offline --manifest-path tools/task-manager/Cargo.toml --target-dir target
tasks run --who task-manager-clippy -- \
  cargo clippy --locked --offline --manifest-path tools/task-manager/Cargo.toml --target-dir target --all-targets -- -D warnings
tasks run --who task-manager-build -- \
  cargo build --locked --offline --manifest-path tools/task-manager/Cargo.toml --target-dir target
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tools/task-manager/tests -p test_wait_budget.py -v
~~~

The following opt-in checks launch workers whose stages take shared admission
**themselves**. Do not wrap either script in buildlock (that would nest admission):

~~~sh
python3 tools/task-manager/tests/worker_e2e.py "$PWD/target/debug/tasks" --timeout 300 --queue-timeout 0
python3 tools/task-manager/tests/self_suite.py "$PWD/target/debug/tasks" "$PWD" --timeout 300 --queue-timeout 0
~~~

E2E uses tiny trusted synthetic Rust sources and synthetic Git refs without making
commits. It validates pass/fail, inert backlog, real compile admission, target contention,
queued and running cancellation, worker/supervisor crash cleanup and lease release,
durable results and source
tamper rejection. Self-suite runs the actual captured task-manager Rust tests.
These Python files are opt-in orchestration, not a Python backend or default
nested Cargo tests. All fixtures/logs remain outside Git; no devices involved.
`--timeout` counts observed execution only; `--queue-timeout` is independent
(default 0 = unlimited). Neither helper retries/submits a duplicate on timeout.

See `RETENTION-MILESTONE.md` for target budget/planning validation and limits,
`CACHE-MILESTONE.md` for the earlier cache/lease validation,
`AGENT-WORKFLOW.md` for agent entry points, and `EXECUTION-MILESTONE.md` for earlier
execution validation. `TAKEOVER.md` is the historical control-plane handoff.
