# Agent workload entry point — deployed native Rust coordinator

**Current source/trial policy:** authoritative standalone repository is
`/home/abe/task-manager`. Read README.md and TRIAL.md; required session feedback
goes in your task report and a new feedback/ file. The source-layout and deployment
history below predates extraction; it does not make the old Tinline copy active.

Use **agent -> tasks -> native Rust worker/admission -> Cargo/Gradle/tools ->
durable results**. `/home/abe/.local/bin/tasks` is installed on PATH. Default shared
state is `/home/abe/.local/state/tinline-task-manager-native`; the CLI starts that
query daemon on demand. Do not set TASK_MANAGER_STATE_DIR for ordinary work.
Older task-manager instances/states are separate and were left untouched.

## Normal agent workflow

```sh
tasks admission                         # actual locks/pressure, including legacy clients
tasks list                              # durable submitted jobs/owners/results
tasks run --who your-agent -- cargo check --offline --locked -p p2pcore
tasks run --who your-agent -- cargo build --release -p desktop
# Run-only: use already-built binaries, not compiles:
tasks run --who your-agent --run -- /absolute/path/to/test-binary
# Requires separate device authorization; never infer permission from the flag:
# tasks run --who your-agent --device SAFE_SERIAL -- /path/to/command ...

# Other agents can query while you run, from any checkout:
tasks list
tasks logs JOB_ID
tasks cancel JOB_ID                      # cancel ONLY your own job
```

`tasks run` is explicit permission to submit/approve/start that requested command;
it prints the durable ID, waits and returns its exit code. Ctrl-C only detaches
the client. Never duplicate a slow/timed-out submission: query its ID first.
The worker drains individually approved jobs, never implicitly approves proposals
or replays interrupted/failed work. Up to five executions can be in flight;
resource admission allows one compile, four run jobs and exclusive device locks,
with machine pressure/memory gates. Idle workers exit, query daemon stays alive.

For separate permission steps:

```sh
tasks enqueue --who your-agent -- cargo check --offline --locked -p desktop
# Capture JSON response.submitted_id — NOT the largest ID in response.jobs.
tasks approve-command JOB_ID             # trust requested argv and live cwd
tasks start-worker
```

These are **trusted local LIVE workspace commands**, NOT immutable source suites.
argv/cwd/owner/resource/results/logs are durable, but source files/environment and
outputs are not sealed. Keep the checkout stable across Cargo test compile/run
stages. Do not pass secrets in argv. Environment is not copied from each agent's
shell; use explicit non-secret `env VPX_DIR=... VPX_STATIC_REQUIRED=1 ...` inputs
when needed, never override machine cache policy. No sandbox, universal command
classification, cgroup quota or guaranteed prevention of arbitrary script I/O.

Do NOT wrap managed worker jobs or scripts which acquire their own admission in
another compile lock. In particular `scripts/buildlock.py` and
`scripts/build_android.py` must not be nested in `tasks run`; use direct Cargo/
Gradle/native-builder commands or the typed Android trial. Existing legacy helpers
still share the SAME flock files, so active old jobs remain protected. Their jobs
are observable through `tasks admission`, not imported into the durable ledger.

## Immutable/typed suites are separate contracts

`task-manager.tests` / `debug/offline` / `quick` captures a bounded sealed snapshot
at pickup (not approval), compiles it, and runs exact produced test harnesses.
This special route uses `tasks submit SPEC.json`, normal `tasks approve ID`, then
`tasks start-worker`. It is not a full-repository source-capture promise.

`core.tests.compile` and `desktop.build` in `tasks catalog` are still proposals for
future SOURCE-CAPSULE/VERIFIED-ARTIFACT contracts. That does NOT prevent today's
explicit live `tasks run -- cargo ...` jobs. Don't describe a live command as a
verified immutable artifact build. Its status means requested process exit only.

Android has a separate experimental build-only source-capsule route:
`tasks android-plan SOURCE FULL_HEAD [ABIS]`, `tasks android-host SOURCE [ABIS]`,
`tasks android-capture SOURCE FULL_HEAD NEW_DESTINATION`, then
`approve-android-trial ID` + explicit worker start. Normal Android catalog approval
remains disabled. One real x86_64 APK passed under the earlier Python admission
backend; native-admitted APK/dual-ABI validation remains separate future work.
No installation/launch/wipe/release/publish/device permission is granted by these
build-only contracts. Do not silently resolve network/signing/security policy.

## Deployment and evidence

See `NATIVE-DEPLOYMENT.md` for the deployed release manifest, current state, exact
checks, limitations and compatibility details. It includes real independent
Agent A/B queries, concurrent run slots, live Cargo compile/run, failed/cancelled
jobs, crash/recovery, captured self-suite and actual shared core/desktop compiler
checks. Rust models alone are not production integration evidence.

Do not stage/commit, install on a phone, wipe data, replace other services, prune
caches or delete other worktrees without explicit authorization. Query active
jobs before any build/service work. The read-only cache planner is not deletion
permission; no cleanup/apply API exists.
