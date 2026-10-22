# macOS adaptation — development checkpoint

This is an incremental backend adaptation, not a second implementation of the task manager.
**The headless `tasks` application now compiles and links on macOS. Local runtime remains
explicitly disabled.** The CLI accepts offline catalog/setup inspection (and fixture UI inspection
when built with UI); other commands reject before connecting to a daemon, creating state or
submitting jobs. No macOS daemon, GUI, scheduler, recovery or local execution acceptance is
claimed.

## Composed shared supervisor checkpoint (experimental)

`crates/platform/src/supervisor.rs` now composes both lifetimes in a separate guardian:
controller -> guardian -> watched wrapper -> command/descendants. It forwards READY only
after the wrapper watcher is armed, then forwards one controller START after a fresh
channel check. Both channels remain open through owned TERM/KILL/reaping. The caller
still supplies the private wrapper group, authoritative leases and independent output
drainers; this is not a new worker, admission backend or public execution command.

The event loop incrementally reads the fixed-size host-local report, rather than blocking
on a partial frame. Its outcome separates command report, controller/wrapper/protocol
reason, protocol error and cleanup result. `CommandReported` deliberately does NOT mean
passed. A zero exit cannot overwrite a cleanup error; controller loss observed during
cleanup remains interruption. Native holder checks and ledger reconciliation still have
to establish completion. There is no respawn/retry path in this primitive.

Eight new native behaviors exercise the composed path: controller loss before grant,
during blocked command pre-exec and during execution; guardian SIGKILL during execution;
normal command status and spawn error reporting; partial-report/controller-loss handling;
and early-wrapper exit. Guardian loss leaves no result and the watched group still releases
its lease. The last two use deliberately incomplete protocol peers, not real armed watchers. The
early-exit case reproduced empty retained-zombie group EPERM and preserves it as an error,
with no command report/completed outcome. No permission/ownership error is suppressed.
All relevant subprocess fixtures still check inherited-lease release, stable lock inode,
unrelated-group survival and non-entry where appropriate.

Selected checkpoint: **189 passed, zero failed/ignored**, the prior 181 selection plus
these eight. The expanded 20-entry workload/composition suite passed three repeated runs.
Headless/default-feature checks, platform all-target Clippy, headless library/CLI/example/
boundary Clippy with warnings denied and owned-file formatting/diff checks passed. No
fixture processes remained; logs stay outside Git. Linux validation remains deferred.

**Still blocked:** native empty-group/holder observation, descriptor allowlists, real
admission integration, durable interrupted/uncertain ledger transitions and restart/no-replay
acceptance. The guards are now composed here but are NOT invoked by the worker. No SQLite,
installed-runtime, policy, service, device or key/config changes occurred. The memory-policy
blocker and fail-closed Mac runtime refusal remain. This is bootstrap/source validation,
not task-submission experience or evidence of full crash-safe execution.

## Workload-side guardian-loss checkpoint (experimental)

`crates/platform/src/workload_guard.rs` now supplies the other side of supervision:
run it in a dedicated, freshly exec'd group leader, not in the daemon or a library
thread. Its watcher is armed before READY; a single START grant permits the command
in the same group. Guardian EOF/protocol failure kills the wrapper's **own live group**,
not a remembered numeric PGID. The peer endpoint must belong exclusively to the guardian;
control descriptors remain close-on-exec. The wrapper forces null command stdin and
its own process group, overriding accidental inherit/new-group builder settings.
The small bounded report is host-local, not a new deployed peer/ledger protocol.

The wrapper remains alive and armed **after reporting the command's exit or spawn/wait
error**. The guardian must record that report, clean its privately owned wrapper group,
then reap; a report is not job completion. This avoids abandoning resistant descendants
if the guardian dies between command exit and its final sweep. A caught no-op SIGTERM
handler keeps the watcher alive during guardian TERM grace; unlike SIG_IGN, exec resets
that handler to default for the actual command. Signal dispositions are process-global:
this API is restricted to a fresh, single-purpose wrapper and refuses non-leaders and
non-default startup SIGTERM. It never returns after arming.

Real native tests SIGKILL the guardian before grant, during a deliberately blocked
async-signal-safe command pre-exec hook, during execution, after command exit, and during
TERM grace. They check non-entry where appropriate, inherited-lease release, stable lock
inodes and unrelated-group survival. Normal cleanup preserves command exit 7 separately
from wrapper termination; spawn errors and malformed/truncated reports are not success.
Eleven behavior tests plus one re-exec fixture entry pass, including three repeated runs.

Native tests exposed two relevant details: self-directed group SIGKILL can return on
Darwin before delivery, so the wrapper waits for it instead of racing it with `_exit`;
and signalling an already-empty retained-zombie group can return EPERM. That existing
`OwnedChildGroup` error is NOT suppressed or treated as cleanup permission. Known-no-command
startup-refusal fixtures use a private direct-child wait instead of requesting a group
sweep. General empty-group cleanup/error classification remains an integration blocker
requiring native observation, not an EPERM allowance. Control writes explicitly use
MSG_NOSIGNAL so a report-write failure cannot SIGPIPE only the wrapper and strand children.

Latest selected validation: **181 passed, zero failed/ignored** (the previous 169 selection
plus these 12). Headless/default-feature checks, platform all-target Clippy, headless
library/CLI/example/boundary Clippy and owned-file formatting/diff checks passed. No fixture
processes remained. Linux validation is deferred at the user's request; no Linux native
execution or full worker E2E is implied. Logs remain outside Git.

**Still not execution acceptance:** the two guards are not composed into the worker;
admission, descriptor allowlists, native holder observation, interrupted/uncertain ledger
outcomes and restart/no-replay acceptance remain. This does not contain escaped sessions,
credential changes, simultaneous guardian+wrapper death, every kernel fork/kill interleaving
or hard-stuck wrapper exec/kernel tasks. Grant revocation is not atomic with exec. Lease
retention may correctly block capacity rather than guarantee bounded cleanup. No service,
key/config, reserve threshold or runtime guard was changed; Mac execution stays disabled.

## Latest dev synchronization: checkout gate and pull-back

Merged `origin/dev` through `0e3da48` into `dev-macos` without conflicts (merge
`054a1b4`). This retains checkout-aware forwarding before offload-slot acquisition,
active-list remote-job/wait annotations, and pressure-aware pull-back rerouting.
The Mac admission candidate, direct-child ownership primitive and runtime guards remain.

Native selected validation passed 162 tests: the prior 117-case selection, 25 rerouting
model/ledger tests, 4 checkout-gate tests, 1 confirmed-cancel rule test and 15 CLI display
tests. Zero failed/ignored. Headless/default-feature checks, platform all-target Clippy
and headless library/CLI/example/boundary Clippy passed. Initial headless Clippy found
that the incoming CLI change left the old table wrapper used only by MCP/tests; it is
now compiled only for those callers. No blanket dead-code allowance was added.

These model/ledger tests do not establish real peer cancellation/reconnect/no-duplicate
acceptance. Linux-only metrics tests, live Linux worker regression and actual remote
pull-back remain unrun. No service/config/key changes or runtime activation occurred.

## Earlier dev synchronization

Merged `origin/dev` through `8244b7b` into `dev-macos` (merge `80367c2`), retaining
our admission candidate and direct-child ownership work. Incoming shared changes include
Cargo jobs policy/busy lowering, stuck-job detection, whole-log grep, coarse ETA matching
and test-target progress. No source conflicts or runtime-guard changes were required.

Native Mac validation passed 117 selected tests: the previous 98-case selection plus
7 policy tests, 3 admission cap/stuck-window/busy-lowering tests, 1 requested-jobs test,
5 worker stuck-rule/log-retention tests and 3 CLI grep tests. Zero failed/ignored.
Headless/default-feature checks, platform all-target Clippy and headless
library/CLI/example/boundary Clippy passed with warnings denied where applicable.

Coverage limitation: incoming ETA/test-target unit tests live in the Linux-only metrics
test module; attempted native filters matched zero tests and are NOT counted above.
The incoming stuck detector's live group-CPU sampler also uses `/proc`, so its actual
sampling and native Linux runtime regressions remain unvalidated here. Pure stuck-rule
tests do run. Mac execution remains disabled and the conservative-memory blocker persists.

## Shared primitives implemented

`crates/platform` (`task-manager-platform`):

- `MachineSampler` retains a `sysinfo` collector for CPU, memory, swap, core count and
  hostname. No background thread or process/environment enumeration. CPU is unknown until
  two sufficiently separated samples exist. Values are observational: sysinfo's available
  memory has OS-specific semantics, not a promise of Linux MemAvailable equivalence.
- `process::{identity, alive}` supplies one process-identity API. Linux preserves
  `/proc/PID/stat` start ticks. macOS uses `proc_pidinfo(PROC_PIDTBSDINFO)` with full-buffer,
  PID, timestamp-range and overflow checks. Its token is the microsecond start timestamp.
  Zombie state or a native ESRCH result is not live. sysinfo's seconds-resolution process start time is deliberately
  NOT used to authorize cancellation. Identity tokens are host-local.
- `process::signal_owned_group` uses `nix`, requires a matching native identity and a group
  leader, and preserves ESRCH handling. Like the existing implementation, it requires an
  owned stage child whose leader remains unreaped through escalation; identity validation
  plus killpg is not atomic and not a substitute for that supervisor contract.
- `src/worker.rs` delegates its identity/aliveness/group-signal helpers to this crate.
  Worker startup and internal stage/admission/command entry points reject unsupported local
  runtime before accessing state. Parent-death and inherited-FD hooks are native operations;
  macOS returns ENOTSUP rather than skipping protections.
- `thread` selects Linux's existing per-thread rusage/nice/idle-I/O calls or macOS's native
  thread CPU clock and thread background priority. `fs::file_key` handles native dev_t types.
  These small primitives remove Linux syscall/type dependencies from shared health code.
- Linux daemon auto-start now uses the same inherited-FD scrub hook as detached device
  helpers: close_range where available, with the existing helper's fcntl fallback otherwise.
  This replaces the old daemon-specific best-effort close_range-only call. Linux runtime
  validation of this consolidation remains outstanding.

Shared library modules are no longer hidden wholesale behind Linux guards. Admission
thresholds, policy, queues, flock leases, cache planning, snapshots, health data and Unix
transport compile together. `src/admission/native.rs` owns live pressure acquisition;
Linux's PSI behavior and error messages are retained, while macOS pressure acquisition
returns Unsupported. `src/runtime_support.rs` distinguishes compilation from runtime
acceptance and is checked before CLI, daemon and worker mutations. The existing Linux
monitoring readers still need macOS integration; compiling them is not a native data feed.

`crates/peer`:

- `nix::errno::Errno::clear` replaces direct Linux errno access around `readdir`.
- One small target-selected rename helper uses Linux `renameat2(RENAME_NOREPLACE)` or
  macOS `renameatx_np(RENAME_EXCL)`. Both retain atomic no-overwrite publication. Other
  targets explicitly return Unsupported, not a check-then-rename fallback.
- The rest of source-transfer logic stays shared.

Existing Cargo `ui` / `mcp` features remain independent of the compilation target. Do not
add manually selectable linux/macos features that can disagree with the actual target.
The existing ledger, wire protocol, approvals, slot filenames and identity JSON field
names have not changed. The Linux default Cargo cap remains two; `-j 4` below is an
operator-approved local macOS bootstrap, not a change to deployed Linux policy.

## First integration with dev

Merged `origin/dev` at `f110c1b` into `dev-macos`. Its canonical serialized
`ProcessIdentity`, native memory/disk/child-usage APIs, capabilities, setup app, relay
configuration and tests are retained. Our signalling/pre-exec helpers are in
`process_control.rs`, while observational sysinfo readings are in `telemetry.rs`.
Redundant native identity implementations were removed, not maintained as competing APIs.

Native integration exposed two previously unexecuted cases: the running kernel returned
40 VM-statistic words while libc describes a larger revision; the reader now validates the
returned prefix through the last field it actually uses, never reads an unreported field,
and rejects short/oversized replies. Darwin libproc also returned ESRCH for an unreaped
zombie. Its test now captures identity while the child is held alive, permits that native
ESRCH observation, proves it is not live, and always reaps the child. This does NOT prove
macOS descendant cleanup when its group leader is a zombie; runtime stays disabled.

## Read-only macOS compile-admission candidate

`src/admission/macos.rs` now implements a separately named, read-only candidate policy:

- Native `kern.memorystatus_vm_pressure_level`: only recognized normal state may pass;
  warning/critical block, and unknown values, failed reads or short replies fail closed.
- Keep the configured memory, cache-disk and state-disk reserves unchanged. Require the
  conservative Darwin memory source, valid bounded counters and measurable disk paths.
- Proposed one-minute runnable-load ceiling: 0.75 per logical CPU, inclusive. This is a
  lagging scheduler-demand signal, NOT CPU utilization or PSI, and has not been tuned
  against real builds. Linux's run-slot PSI override does not relax it.
- Do not claim CPU/I/O stall measurements. Emit no made-up PSI fields or averages.
- A successful resource evaluation is NOT admission: every report says
  `runtime_enabled: false`. No slot acquisition, worker enablement, approvals, daemon
  startup or state creation is added. The original Unsupported pressure backend and
  CLI/internal runtime guards are unchanged.

Read-only probe (supply actual absolute paths when evaluating a planned cache layout):

```sh
cargo run --locked --no-default-features --example macos_admission -j 4 -- \
  /tmp/tasks-macos-candidate-readonly/cache /tmp/tasks-macos-candidate-readonly/state
```

The probe resolves absent disk paths to their existing ancestors without creating them.
One real sample reported normal memory pressure, 10 logical CPUs, load 2.40, approximately
429 MiB of conservative free/purgeable headroom and approximately 114 GiB free on the
probed filesystem. It correctly returned `available memory below reserve` against the
unchanged 8 GiB floor. The headroom estimate intentionally excludes reclaimable file
cache; this is NOT a claim of only 429 MiB of all potentially usable RAM. Policy suitability
for this 16 GiB Mac must be reviewed before activation; do not silently lower the reserve,
substitute sysinfo telemetry or equate normal pressure with sufficient headroom.

Candidate validation: 13 new unit/native-read tests, 5 CLI/fixture boundary tests and
9 existing Linux admission fixture tests passed (27 selected tests; zero failed/ignored
in the final runs). Warning/critical/unknown-pressure, malformed counters and reserve/load
boundaries are synthetic tests; actual pressure/load were observed, not stress-induced.
Headless library/example/boundary Clippy passed with warnings denied, and default-feature
`cargo check` passed. An initial boundary run exposed colliding parallel timestamp-only
temp names; the fixture now includes an atomic sequence and has a parallel-creation test.
Linux native runtime and worker/lease/recovery acceptance remain unrun.

## Direct-child group ownership and lease cleanup

`crates/platform/src/owned_group.rs` adds shared Linux/macOS `OwnedChildGroup`,
currently **not wired into the worker**. It spawns a dedicated group, privately retains
its `Child`, and uses `waitid(WNOWAIT)` to pin the leader's PID/PGID until cleanup.
It does not depend on libproc exposing a zombie's start timestamp. No reaping handle
or `try_wait` is exposed. TERM escalates to KILL after the grace period or leader exit;
normal completion also kills remaining group members before reaping. Ordinary Drop
attempts cleanup but provides no SIGKILL/crash protection, containment or hard timeout.

Ownership requires exclusive child reaping and a stable default SIGCHLD disposition.
Spawn rejects non-default handlers, SIG_IGN and SA_NOCLDWAIT; lost direct-child ownership
(ECHILD) is an error before signalling, never permission to act on a numeric group.
No independent process-wide reaper or later signal-disposition changes are permitted.
Trusted pre-exec hooks must not undo the requested new group. Processes escaping into
other sessions are not covered and must not cause their lease to be stolen.

`crates/platform/tests/owned_groups.rs` runs actual subprocesses and flocks. Seven
behavioral tests plus one subprocess-fixture entry passed: TERM-exiting leader with
resistant descendant, already-exited leader, grace-period escalation for a resistant
leader, normal completion, ordinary Drop, auto-reaping rejection and lost ownership.
Lease cases retain a deliberately inheritable FD, observe exclusion, prove release
after group cleanup and verify the stable slot inode and an unrelated group's survival.
Spawn tests are serialized because inheritable FDs are process-global; fixtures expire
after 15 seconds even if their test parent disappears. No adb/sccache/daemon was launched.

The selected regression run passed 98 tests: platform library 24, direct groups 8
(including the fixture entry), peer library 10, setup library 17, macOS boundaries 5,
candidate admission 13, Linux admission fixtures 9, command policy 8, runtime guard 1,
Apple artifacts 3. Zero failed/ignored; other root tests were filtered. Platform
all-target Clippy passed with warnings denied, as did the default-feature metadata
check. These are native macOS results, not Linux runtime regression or worker E2E.

Still required: protect the spawn/pre-exec window and descendants after abrupt
supervisor death, preserve inherited leases through all cleanup paths, implement
non-invasive native holder observation and detached-helper descriptor isolation, then
integrate the ownership handle into the worker. Existing parent-death/FD hooks remain
ENOTSUP on macOS and runtime entry points remain blocked. The prior conservative-memory
policy blocker is unchanged.

## Shared controller-loss guard (not worker integration)

`crates/platform/src/guarded_group.rs` adds a shared Unix guard for a **separate guardian
process**. The controller owns the sole peer of a close-on-exec Unix socket. The guardian
waits for an explicit startup byte before spawning a dedicated `OwnedChildGroup`; EOF
before startup means nothing is spawned, and EOF after spawn triggers group cleanup.
Unexpected protocol/I/O errors propagate rather than reporting success; an existing group
receives best-effort Drop cleanup. An inheritable guardian endpoint is rejected.

The controller's peer endpoint must never be duplicated/passed to the workload; doing so
would mask controller loss. Callers must provide explicit workload stdio, retain exclusive
child-reaping ownership and drain any captured output concurrently. The internal byte
protocol is a primitive's test/caller contract, not a deployed peer or ledger protocol.

Real native fixtures launch distinct controller, guardian, workload and resistant descendant
processes. Tests SIGKILL the controller before startup grant and during execution, then
prove non-entry or inherited-lease release, unchanged slot inode and unrelated-group
survival. Normal leader exit also preserves exit 7 while descendants are cleaned up.
Buffered grant plus EOF, malformed grant and inheritable endpoint checks fail closed.
Six behavior tests plus one re-exec fixture entry pass; fixtures self-expire after 15 s.

**Remaining holes:** killing the guardian itself bypasses its cleanup. A controller may
also disappear after the final pre-spawn check or while synchronous spawn/exec is stuck;
this guard cannot atomically prevent subsequent command entry. It is not an admission
backend, arbitrary-descendant containment, hard timeout or ledger recovery mechanism.
Workload-side startup/liveness protection and failure-injection tests must close these
windows before worker integration. No claim of complete crash-safe execution is made.

The selected native checkpoint passed 169 tests (prior 162-case selection plus these 7),
zero failed/ignored. Headless/default-feature compile checks, platform all-target Clippy,
headless library/CLI/example/boundary Clippy and owned-file formatting/diff checks passed.
Initial Clippy exposed fixture Child handles needing explicit reaping guards; fixed with
RAII fixture ownership, not warning suppression. Linux native runtime and full worker E2E
remain unrun. No fixture subprocesses remained after the check. Mac runtime guards,
ENOTSUP pre-exec hooks, deployed state and the conservative memory-policy blocker are
unchanged. No service was installed or restarted.

## Validation on ARM64 macOS

```sh
# Direct Cargo bootstrap explicitly approved because no installed tasks exists here.
# Do not generalize this exception to hosts with an installed coordinator.
cargo check --locked --no-default-features -j 4
cargo test --locked --no-default-features --test macos_boundaries -j 4
cargo test --locked --no-default-features --lib --no-run -j 4
cargo test --locked --no-default-features --lib admission::ops_tests:: -j 4
cargo test --locked --no-default-features --lib local_command::tests:: -j 4
cargo test --locked --no-default-features --lib runtime_support::tests:: -j 4
cargo test --locked -p task-manager-platform -p task-manager-peer --lib -j 4
cargo clippy --locked --no-default-features -p tinline-task-manager --lib --bin tasks -j 4 -- -D warnings
```

Observed after merging dev: headless check and link passed; the default UI/MCP-feature
metadata check also passed (not GUI linking/launch acceptance). Selected executions passed
76 tests: 4 macOS CLI/backend boundary tests, 9 shared admission fixture tests,
8 command-policy tests, 1 runtime-support test, 24 platform tests, 10 peer library tests,
17 setup library tests and 3 Apple-artifact integration tests. Zero failed/ignored in the
final runs; other main-library tests were filtered, NOT validated. Headless application
and boundary-test Clippy passed with warnings denied; platform/peer/setup all-targets
Clippy also passed with default features disabled.

Tests exercise native machine/thread readings, native process identity, mismatched identity
refusal, non-leader signal refusal, signalling/reaping a synthetic isolated sleep child,
synthetic Linux stat/PSI parsing, readdir rewind/errno behavior, and no-replace publication
preserving existing directories and dangling symlinks. CLI tests prove runtime commands
refuse without state creation or executing a synthetic touch command. No external peer,
phone or deployed state is used. Linux native execution and full supervisor/lease regression
coverage have NOT been run. Default-feature GUI code passed a Cargo metadata check;
a linked GUI binary and native window have not been validated.

## Next bounded work packages

1. **Admission integration.** The read-only native candidate above exists. Review the
   conservative-memory floor and proposed load ceiling for this host before activation.
   Adapt shared gate/status interfaces to distinct native samples (no fabricated PSI),
   with fresh checks before/after lease acquisition and fail-closed errors. Keep the
   Unsupported acquisition result until worker/lease safety is accepted; do not weaken
   Linux gates. Files: `src/admission.rs`, `src/admission/native.rs`, `src/cache.rs`.
2. **Monitoring/health integration.** Feed macOS snapshots from `MachineSampler`; preserve
   the existing snapshot schema (`pressure: null` when unavailable). Replace Linux-only
   health thread priority/accounting and separate Linux lock parsing from native holder
   discovery. Unknown holder/lock state must not become a false "free". Coordinate shared
   platform API edits separately. Files: `src/metrics/`, `src/health/`.
3. **Execution and lease safety.** Direct-child group cleanup is now available and tested,
   but not integrated. Implement macOS parent-death/supervisor behavior and read-only
   lock/descriptor inspection. Test daemon/worker crashes, escaped descendants,
   cancellation escalation, inherited leases and PID reuse before enabling local jobs.
   Do not simply remove prctl or use racy lock probes as a replacement for holder scans.
   Files: `src/worker.rs`, `src/lease_audit.rs`, admission holder backend.
4. **CLI/daemon/UI integration.** Wire the shared modules with target-selected primitives;
   explicitly reject unported Linux-only adapters. Validate isolated headless execution
   before the default-feature GUI build. Leave deployment and installed state untouched.

Parallel contributors should use distinct worktrees and agree exact file ownership;
`src/lib.rs`, Cargo manifests/lockfile and shared API definitions need one integration owner.
No subagent execution facility was available during this checkpoint.
