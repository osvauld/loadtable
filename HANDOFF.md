# Task manager — active worktree continuation

## Standalone extraction (latest)

Authoritative repository is now `/home/abe/task-manager`, initialized locally on
main without commits or remote. Read README.md, AGENTS.md and TRIAL.md for current
instructions. Tinline copies and all runtime state/releases were preserved;
source extraction is not a service upgrade. Agents must use the facility and
provide honest session feedback in a new feedback/ file and their task report.
See EXTRACTION.md for validation/limits. Everything below is historical context,
including old active-source paths and old next-work lists.

## Location and ownership

The user selected the version with the most additions and requested worktree-based
continuation. On inspection it was already relocated to:

- Worktree: `/home/abe/p2p_chat/.claude/worktrees/task-manager-next`
- Branch: `tools/task-manager-next`
- HEAD at inspection: `01c57dd2e8a21c1811dc3ccd1b226e58cf89181c`
- Source: `tools/task-manager/` (untracked; no staging/commit authorized)

Continue only in this worktree's `tools/task-manager/`. The main checkout no
longer contains the tool. Do not recreate it there. The older v1 worktree
`.claude/worktrees/task-manager` remains untouched, including its historical
handoff and AGENTS.md change. Do not overwrite it with this version.

Initial continuation owned the task-manager directory only. The later explicitly
authorized native deployment additionally installed a new shared per-user entry
point/state/daemon and changed main AGENTS.md's build entry-point instructions.
It did not authorize edits to other worktrees, unrelated build outputs, old task
state or device operations. The main checkout's
`.gitignore` edit belongs to other work and was not changed.

## Retained features

Version-2 socket API, egui UI, SQLite ledger, catalog-restricted local execution,
source snapshots/SHA-256 sealing, approval, worker, shared build/run admission,
logs, cancellation, interruption handling and worker runtime observations.
Only `task-manager.tests` / `debug/offline` / `quick` is executable.
Approval/start policy is unchanged; selecting this implementation did not
resolve whether it should become automatic.

The previous continuation added compact columns, readable logs with a raw toggle,
ASCII instructions and documentation. Historical validation: 37 Rust tests, clean
clippy and build, plus native geometry checks at 1060/760 px. See
`EXECUTION-MILESTONE.md` for the exact scope and limitations of those checks.

## Shared native Rust coordinator — DEPLOYED (latest)

User redirected priority away from more Python-backed Android trials: implement
Rust admission and deploy the shared agent entry point. This is now done.

- Installed `/home/abe/.local/bin/tasks` -> independent cache-disk release binary,
  SHA-256 `c3a9c31dea6ace2cffe1c4b558f4e89f239f3931e5fdadd7efeefdee65d2705b`.
  Manifest `/tmp/task-manager-native-deployment.json`; preserve copied runtime
  releases under `~/build-cache/p2p_chat/tasks/releases/`.
- Shared state `~/.local/state/tinline-task-manager-native`, NOT the old default.
  On-demand native daemon remains running, no systemd install. Old daemon PID
  611058 and its state were not stopped/migrated. Own new native daemon had idle,
  guarded promotions only; all durable rows survived, no device sessions touched.
- `tasks run --who AGENT -- COMMAND` explicitly submits/approves/starts, waits and
  returns child exit code. Default compile; `--run` already-built execution;
  `--device SAFE_SERIAL` only when separately authorized. Ctrl-C detaches, does
  not cancel: query existing ID, don't blindly resubmit. Alternatives enqueue,
  approve-command ID, start-worker; enqueue returns authoritative submitted_id.
- `tasks list`, `tasks admission`, `tasks logs ID`, `tasks cancel ID` work across
  agents/checkouts. Default shared state, no TASK_MANAGER_STATE_DIR override for
  normal agents. Existing legacy jobs are observed via admission, not invented
  ledger rows/historical results. Per-job approval remains required.
- Rust module admission.rs: same stable flock inodes as legacy Python, CPU PSI/
  MemAvailable gates, compile/4-run/device slots, process-start-bound notes and
  filenames. Holder text isn't lock authority; no lock inode deletion. Native
  FIFO doesn't trust legacy PID-only notes. Worker no longer invokes buildlock.py.
- Explicit pool caps five claimed executions, so run work can overlap compile;
  per-state worker/cleanup lease, owned groups/watchdogs, no automatic replay.
- local_command.rs is a SEPARATE live cwd/argv contract (no source snapshot or
  inferred artifact/test result). Explicit approve-command authorization, durable
  failure/cancel/exit/timings, atomic completion. Raw core/desktop jobs now work;
  immutable core/desktop catalog contracts and Android normal approval still gated.
  Keep workspaces stable; arbitrary caller env isn't persisted. Use non-secret
  `env NAME=value ...` when needed, never cache overrides. No nested helpers.
- Final **90 Rust tests passed, 0 failed/ignored**, clippy/build clean, then ALL
  final native CLI/parallel-agent/Cargo/failure/cancel tests, worker crash/lease/
  tamper E2E and actual captured manager self-suite passed. Latest compiler checks
  ran THROUGH shared Rust as job 6: /tmp/task-manager-native-identity-checks.log.
  Real suite summary /tmp/task-manager-native-regressions-complete.json (all 0),
  self-suite /tmp/task-worker-self-suite-4q30yrf1.
- Shared deployed Agent A/B smoke passed (native-only owned process tree), final
  durable job 7; /tmp/task-manager-native-shared-smoke.json. Actual shared native
  `cargo check --offline --locked -p p2pcore` job 2 and desktop job 3 both PASSED.
  These are compiler checks, NOT runtime/network/device/release integration.
- No active owned validation/controller remains. Earlier failing idle fd fixture
  and clippy logs retained, fixed, not ignored. No cache deletion, phone install,
  data wipe, staging, commit, merge or push. Code remains untracked/uncommitted.

Scope addition authorized by deployment: main `/home/abe/p2p_chat/AGENTS.md` now
routes new direct agent builds through tasks, warns about live cwd/secrets/nested
helpers, preserves old Python compatibility. Other main .gitignore/contact-sharing
work untouched; no other worktree edited. Active worktree's legacy scripts/AGENTS
were not synchronized to new dev, because Rust runtime no longer needs them.
See `NATIVE-DEPLOYMENT.md` + rewritten `AGENT-WORKFLOW.md` for current instructions.
Next: agent adoption/query/maintenance polish; avoid restarting services or
changing deployed binary while jobs/cleanup are active. Native typed Android and
both-ABI validation are later; do not confuse earlier Python x86 success with it.

## First real Android trial (previous)

Completed before the native-Rust pivot: PASSED, APK 181,425,159 bytes,
SHA-256 `e27a1bc161aec7ef096e5da0c63141e2d742b58b4217666a070c25c81d75b451`,
receipt independently rechecked, owned private daemon stopped. This was Python
admission, NOT native/dual-ABI validation. The following was its initial live note.

User authorized continuation; cache disk recovered to **51 GiB free** without any
cleanup by this agent. First x86_64 build-only trial is approved and worker started
through `tasks` internal admission, currently queued behind other actors. Private
state `/home/abe/build-cache/tm-fixtures/android-real-7hpp0966/state`, job **1**;
controller PID **108149**, `/tmp/task-manager-android-first-trial.py`, progress
`/tmp/task-manager-android-first-trial.json` / `.log`. Do NOT duplicate submission.
Source HEAD cc09ac1; captured 749 files / 13,887,778 bytes, digest
`0333305275bc940bce0178ac65684032ac8fd61818908bd79851a761504d8542`.
Actual x86 plan/host recheck passed. No APK outcome yet. Inspect terminal job and
receipt before both-ABI trial; no automatic second submission/retry is running.

Main's new uncommitted cache shim/buildlock/setup work belongs to another agent;
not overwritten/copied. Current global Cargo wrapper remains installed sccache;
owned admission uses its existing reviewed policy. If global config changes
while queued, existing host identity guards refuse work; diagnose before retry.
See `ANDROID-EXECUTION-MILESTONE.md` live section. No devices, old task service,
other worktrees or active build outputs touched. Normal catalog still gated.

## Android experimental execution implementation (previous)

Implemented leased writable lanes, per-checkout/ABI Cargo outputs, fresh JNI/APK
outputs, native cache isolation, installed/offline toolchain execution, durable
APK/provenance receipts and atomic build-only completion. New explicit per-job
`approve-android-trial JOB_ID` enables validation only; normal catalog approval
remains gated and worker start remains separate. No silent environment opt-in.
See `ANDROID-EXECUTION-MILESTONE.md` for trust limits, commands and exact evidence.

Current blocker is real: cache disk fell from 14 GiB to **8.3 GiB free**, below
worker reserve 20 GiB. No APK/NDK/Gradle build or captured self-suite/E2E was launched
through this executor. Do not bypass reserves or delete legacy/active caches.
Other agents' emulator sessions and active builds were left alone. Restore space
via a separately reviewed/authorized plan, review host policy, then run x86_64
trial first, both ABIs second. Do not enable the normal catalog prematurely.

Final checks: **78 Rust tests passed (0 failed/ignored)**, clippy clean, binary
built; `/tmp/task-manager-android-executor-complete.log` exit 0. Four Python clock
tests, formatting and whitespace checks passed. No owned validation remains
queued. Actual read-only dual-ABI host audit passed (13 identities, Rust 1.99.0,
installed stdlibs); `/tmp/task-manager-android-host-audit.json`. It is not APK
integration proof. Initial check hit an existing cache-test fork-inherited
fd race; idle assertions now have bounded retries, active checks and production
locking unchanged. Live isolated approval/disk-wait/cancel smoke passed:
`/tmp/task-manager-android-admission-smoke-final.json`; no snapshot/stages/history
were created, owned worker/daemon exited. Latest wait: 8.2 GiB available / 20 GiB
reserve. Read-only cache-plan found one retained recent self target and 36 legacy
owner-unknown roots; none authorized for deletion. It is NOT an APK build pass. Integration dev
at initial inspection: `990c0d66c53c9af494fe306d03167cbd28da5b3a`; final dev
HEAD `cc09ac18047efc2f7742de3ac430f46400142f41` adds only docs/TODO.md. No merge,
reset, staging or commit occurred.

## Android preparation continuation (previous)

User authorized Android build-only continuation. Implemented separate bounded
workspace source capsules (`workspace_snapshot.rs`), read-only typed pipeline /
toolchain/native preflight (`android.rs`), and `tasks android-plan` /
`tasks android-capture`. Android execution is STILL gated: writable leased lanes,
external-input policy and atomic APK/build-only completion are not implemented.
See `ANDROID-MILESTONE.md` for exact scope, commands, evidence and next steps.

Actual dev `e43be4e` was audited/captured: 745 files, 13,833,401 bytes, libaom 3.15.2
tarball verified, all installation markers present, no build/device/daemon action.
Capsule: `/home/abe/build-cache/tm-fixtures/android-capsule-b2r3srwg` (receipt beside
sealed source). The owned worktree buildlock/AGENTS now match dev exactly; its
.gitignore also incorporates dev's Android symlink fix. No merge/reset occurred.
Current development binary is `target/debug/tasks` through this checkout's
canonical target symlink, not the old continuation-target binary.

Validation helpers now separate observed execution timeout from queue timeout
(default unlimited), without retrying jobs. Four Python clock models passed.
Final checks: **66 Rust tests passed (0 failed/ignored), clippy clean, build
passed**; `/tmp/task-manager-android-final-checks.log` exit 0. The one conditional
sequencer then completed real worker E2E and captured self-suite (all 66 tests)
with exit 0. `/tmp/task-manager-android-regressions.json` is `status: passed`;
no owned validation remains queued. Logs/state/exact commands are in
`ANDROID-MILESTONE.md`. An old test's transient fork-inherited lease expectation
and one clippy style issue were corrected before the final complete pass; no
failed test was ignored. Production lease protection was not relaxed.

## Retention/budget continuation (previous)

The user requested continuation. `tasks cache-plan` and the self-test target
admission ceiling are implemented in `src/retention.rs`; see
`RETENTION-MILESTONE.md` for provenance, lifecycle, scan bounds, exact checks,
pending/final validation and remaining rollout blockers. No deletion/apply API
was added, no legacy cache was adopted, and core/desktop/Android remain disabled.
Bootstrap validation completed: 53 Rust tests passed (0 failed/ignored), clippy
clean, binary built; `/tmp/task-manager-retention-final-checks.log` exit 0.
Extended worker E2E timed out after 900 seconds before its first compile was
admitted (Android held the compile slot); it is inconclusive, not passed. Its
orchestrator/build stage were no longer running on reinspection. No rerun was
launched. Log `/tmp/task-manager-retention-e2e.log`; state/live probe details in
`RETENTION-MILESTONE.md`. Actual captured self-suite for this slice is pending.

Reinspection of dev: `ae764ee` adds Gradle cache/output placement to buildlock;
`39c6f69` adds Linux AppImage packaging/release CI. The active worktree still has
HEAD `01c57dd` and older buildlock/Android wiring. Current dev's AGENTS says not to
override wrapper/target/Gradle-home policy or share worktree targets. Reconcile
that policy explicitly with worker-owned leased targets before Android execution.
Existing Android build directories were real directories, not migrated symlinks;
leave the active build untouched. Android's bindgen reads a fixed repo/target
path, so a target environment override alone does not implement relocation.

## Previous cache/admission continuation

The user asked to start routing agent workloads through the app, with worker-owned
cache policy, target leases, disk admission and reviewed workload definitions.
See `CACHE-MILESTONE.md` for current implementation, exact validation commands,
status and limitations; `AGENT-WORKFLOW.md` documents the incremental rollout.
Only the self-suite executes. Disabled proposed workloads are not production
integration. Fresh validation: 43 Rust tests passed (none failed/ignored), clean
clippy, binary build, extended worker E2E (including real low-disk blocking), and
the actual captured self-suite through the worker all passed. Live Cargo env
confirmed worker-owned sccache/incremental/debug configuration. Exact commands,
logs and scope are in `CACHE-MILESTONE.md`.
Do not delete caches or install/restart services without approval.

## Previous worktree continuation

- Updated README paths for this worktree; binaries must be rebuilt here because
  compile-time source defaults and trusted buildlock paths embed their checkout.
- Added a headless egui regression for essential job headings/actions at 760,
  1060 and 1180 px. It checks clipping and action wrapping over settled grid frames
  without opening a native window or issuing daemon requests.
- Validation will use the shared build queue, not bypass the active package build.
  Log: `/tmp/task-manager-next-checks.log`. Check the queue/process before retrying.
  Completed: 38 Rust tests passed (0 failed/ignored), clippy clean, binary build
  passed; `/tmp/task-manager-next-checks.log` ended with `VALIDATION_EXIT=0`.

## Live-state caution

The old default daemon PID 611058 was still running at this inspection, from
`task-manager-takeover-target/debug/tasks`. Do not kill it or wipe its state.
Use a disposable private `TASK_MANAGER_STATE_DIR` for later live validation.
No daemon, database, worktree branch or user data was modified here.

## Next work

1. Coordinate adoption of the deployed shared Rust command. Verify callers use
   the default shared state rather than old private v2 sessions. Legacy traffic
   remains observable through admission but is not historical ledger data.
2. Improve UI/query/maintenance ergonomics and history pagination. Preserve
   per-job permission, no blind mutation replay, and owned-group cleanup. No
   universal sandbox, cgroup/hard disk quota or zero-downtime maintenance exists.
3. Native typed Android/dual-ABI validation and immutable core/desktop verified
   artifact contracts remain later work. Raw live core/desktop compiler checks
   already passed; don't conflate them with release/network/device acceptance.
4. Cleanup remains separately authorized: preserve controls, active work, copied
   runtime releases and results. No deletion/apply API. No phone install, release,
   publishing or app-data operation authorized. Estimates are not guaranteed ETAs.
