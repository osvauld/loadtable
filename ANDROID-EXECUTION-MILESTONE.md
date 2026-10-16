# Android experimental worker — historical Python-backed x86 trial passed

**Current priority/status:** native Rust coordinator is deployed; see
`NATIVE-DEPLOYMENT.md`. The x86_64 trial below completed successfully under the
previous Python admission path: APK 181,425,159 bytes, SHA-256
`e27a1bc161aec7ef096e5da0c63141e2d742b58b4217666a070c25c81d75b451`.
All native/Gradle/ZIP-JNI steps passed, receipt rechecked, private daemon stopped.
No native-admitted APK/dual-ABI validation or installation was done. The user
explicitly prioritized the shared Rust coordinator instead of further Python
rollout. The live/blocked notes below are historical, not an active submission.

## Live continuation (supersedes earlier disk blocker)

User authorized continuation. Cache disk now has **51 GiB available**; no cleanup
was necessary or performed. Read-only source plan and x86_64 host recheck passed.
First real build-only job is explicitly approved and worker started, waiting for
shared compile admission behind active work (do not bypass or duplicate):

- Source `/home/abe/p2p_chat`, HEAD `cc09ac18047efc2f7742de3ac430f46400142f41`.
- Snapshot **749 files / 13,887,778 bytes**, digest
  `0333305275bc940bce0178ac65684032ac8fd61818908bd79851a761504d8542`.
- Private state `/home/abe/build-cache/tm-fixtures/android-real-7hpp0966/state`,
  job **1**, configuration `debug/x86_64/rust-debug/offline`.
- One-shot controller `/tmp/task-manager-android-first-trial.py`, PID **108149**;
  progress `/tmp/task-manager-android-first-trial.json`, `.log`.
  No automatic retry or dual-ABI submission. Check live processes/state before
  retrying; a controller error can leave its own daemon/worker alive intentionally.
- Main checkout has another agent's uncommitted cache-shim/setup/buildlock work.
  It was NOT overwritten/copied/deployed. Current machine Cargo configuration
  still selects installed sccache; the owned manager's trusted admission script
  retains the previously reviewed policy. Host/config change while queued will
  refuse work, not silently switch captured policy. Review any such refusal
  before a new approval. No service restart/device operation is authorized.
- Other active desktop/receipt builds and device lock were left alone.

A queued job is NOT a passing build. Inspect logs and terminal state, recheck APK
receipt and packaging first. Only then submit the explicit dual-ABI trial; normal
catalog stays gated pending actual acceptance and host-policy review.

The sections below retain the earlier implementation/validation record.

## Current status

The managed Android build-only worker path is implemented. Normal catalog approval
is STILL gated (`ANDROID_ENABLED=false`). A separate **explicit per-job**
`approve-android-trial JOB_ID` action permits validation without silently enabling
all Android proposals. Worker start is still separate. There is no environment
variable that opts jobs in, no auto-approved backlog, and no automatic retry.

**No real APK build has been validated.** Cache space fell from 14 GiB to 8.3 GiB
while other agents were building. The worker requires 20 GiB free on its build/cache
filesystem; do not bypass that reserve, launch indefinitely blocked duplicate
workers or delete other agents' caches. Existing emulator/device sessions were
left alone. No install, launch, release/bundle/publish or data-wipe step exists.

Active source: `.claude/worktrees/task-manager-next/tools/task-manager/`.
Reviewed dev at `990c0d66c53c9af494fe306d03167cbd28da5b3a`; integration checkout and
other worktrees were not edited. Owned buildlock/AGENTS still match dev.

## Implementation

- `src/android_build.rs`: private managed lane roots under
  `~/build-cache/p2p_chat/tasks/android-lanes/ABI/CHECKOUT_PATH_HASH/`.
  Different source worktrees never share Cargo targets. Jobs from one checkout
  serialize on an exclusive stable descriptor lease across capture/build/result
  publication/crash cleanup. Compiler/dependency caches remain shared.
- Android ownership is distinct (`android-debug`); populated unmarked caches are
  refused rather than adopted. Existing self-test ownership/planner semantics
  remain unchanged. Android is NOT eligible for self-test retirement proposals.
- Admission uses the existing conservative 8 GiB allocated lane threshold plus
  complete bounded inventories. This includes retained working trees/native/
  Cargo/project outputs. It is not a hard quota or fleet budget; a running build
  may exceed it and subsequent work is refused. No cleanup/apply API was added.
- `workspace_snapshot::materialize` creates a NEW writable, independently copied
  tree from an exact verified capsule, preserving executable bits. No hardlinks
  to immutable source. Inputs are rechecked around execution; output links and
  inherited lease device/inode are verified separately.
- Fixed mappings: `tree/target` -> lane target, `tree/android/build` -> Android
  output, `tree/android/app/build` -> a **fresh per-tree** output directory. This
  satisfies UniFFI's fixed repo/target lookup and prevents stale APK/JNI reuse.
  Project caches are lane-local; global Gradle home stays the machine canonical
  cache. Internal admission runs outside any Git checkout, not the caller's cwd.
- Native cache identity binds the captured builder script and recorded NDK paths/
  identities. The verified shared tarball is copied into a lane-local namespace;
  mismatched archives are refused, never repaired/deleted. Shared native installs
  are not modified. Native builder and Gradle run under one internal compile
  admission; `build_android.py` is NOT nested/wrapped.
- Only numeric, already installed Rust toolchains from captured
  `rust-toolchain.toml` are accepted; requested Android stdlib targets must exist.
  No rustup install. Host identity records Rust/Cargo/cargo-ndk, JDK, NDK compiler/
  linker, SDK markers and Gradle launcher. Global Cargo/Gradle configuration is
  fingerprinted, never copied/logged as credential values; new/unreadable config
  or changed identities refuse work. Global Gradle init scripts require review.
- Already installed Gradle 9.8.1-bin is invoked directly; no wrapper bootstrap
  download. Cargo and Gradle use offline mode; SDK/Java auto-download is disabled.
  Backend cache settings are inherited, not redirected. No CARGO_TARGET_DIR or
  alternate Gradle-home override; Cargo/Gradle workers limited to two.
- Fixed stages: host version observations -> per-ABI libaom -> Gradle assemble
  (which runs cargo-ndk and UniFFI) -> APK checks -> durable publication. Substep
  logs/timings are retained in `android-steps.json`. Failures/cancellation do not
  teach successful history. No Cargo/test harness execution is invented.
- APK checks bound archive count/uncompressed size, verify ZIP CRCs, reject
  duplicate/traversal entries and require exactly the requested native ABIs with
  matching ELF machine IDs for libp2pcore. These are packaging checks, NOT proof
  that an APK installs, a call works or Android runtime behavior is correct.
- Durable `jobs/ID/apk/app-debug.apk` and `provenance.json` are copied/new-created
  and synced while retaining the lane lease. Receipt binds size, APK SHA-256,
  provenance SHA-256, source digest and configuration. Atomic build-only success
  records only `worker.build` / wait-build history, never `worker.run`. A cancel
  racing with completion keeps results but wins status and learns no success.
- Socket additions are additive protocol-v2 fields/action; older records decode
  without trial/artifact fields. An old daemon must not be restarted or replaced
  implicitly; use a new private state for trials. Older clients/daemons do not gain
  Android permission through the new command.

## Trust / limits

Approval authorizes trusted local build scripts and host configuration with this
user's rights, NOT sandboxed untrusted source. Offline flags are NOT a network or
device-access sandbox against arbitrary build/plugin code. Recorded markers/
executables are partial host identities, not a hermetic capture of complete SDK,
Rust sysroots, plugin caches or shared libraries. Host policy review and real APK
validation are still required before normal catalog activation. Source snapshots
and cooperative descriptors are not defenses against a hostile same-user writer.
See `ANDROID-MILESTONE.md` for capsule scope/limits and first preparation evidence.

## Trial workflow — only after disk space is restored and source/host reviewed

Use the active worktree's `target/debug/tasks`. Do not install/replace a service.

```sh
export TASKS=/home/abe/p2p_chat/.claude/worktrees/task-manager-next/target/debug/tasks
export TASK_MANAGER_STATE_DIR="$HOME/.local/state/tinline-tasks-android-trial-v2"
# Write SPEC.json outside Git with CURRENT full HEAD of the selected source:
# suite: android.assemble.debug; owner: your-agent
# source: /home/abe/p2p_chat; revision: git -C SOURCE rev-parse HEAD
# configuration: debug/x86_64/rust-debug/offline; cost: cpu_simulation
"$TASKS" android-host /home/abe/p2p_chat x86_64,arm64-v8a  # read-only installed-host audit
"$TASKS" submit /absolute/path/to/SPEC.json
"$TASKS" approve-android-trial JOB_ID  # explicitly trusts local source AND host config
"$TASKS" start-worker                # separate permission to start; no outer lock
"$TASKS" logs JOB_ID
"$TASKS" list
# If admission stays blocked, cancel THIS job rather than duplicating it:
"$TASKS" cancel JOB_ID
```

First real validation must be x86_64. After it passes, submit a NEW explicit trial
with configuration `debug/x86_64,arm64-v8a/rust-debug/offline` for both ABIs.
`debug/arm64-v8a/rust-debug/offline` is also typed; arbitrary ABI/shell/profile
strings are rejected. Do not enable normal catalog approval until both validations
and host/input policy review pass. Installation and publishing are separate policy.

## Evidence / checks

- Initial incremental checks: 56 library tests passed; one pre-existing cache test
  observed a fork-inherited descriptor during an idle assertion. Like the earlier
  retention fixture fix, idle-only expectations now retry for at most 3 seconds.
  Active/exclusive assertions remain immediate; no production lock behavior was
  relaxed. No failed test was ignored. Log
  `/tmp/task-manager-android-executor-checks.log` (exit 101).
- Corrected checks passed **78 Rust tests, 0 failed/ignored**, clippy `-D warnings`
  and build, then repeated after cwd isolation. Latest frozen verification:
  `/tmp/task-manager-android-executor-complete.log` ended `VALIDATION_EXIT=0`:
  **78 passed, 0 failed/ignored**, clippy clean, binary built. Python independent
  wait-clock tests also passed **4/4**; Rust formatting/whitespace checks passed.
  Command, from the active worktree:
  `python3 scripts/buildlock.py --who pi-task-manager-android-executor-complete -- sh -c 'cargo test --locked --offline --manifest-path tools/task-manager/Cargo.toml --target-dir target && cargo clippy --locked --offline --manifest-path tools/task-manager/Cargo.toml --target-dir target --all-targets -- -D warnings && cargo build --locked --offline --manifest-path tools/task-manager/Cargo.toml --target-dir target'`.
- New tests are bounded filesystem/ZIP/SQLite models: copy isolation, executable
  modes, stable lease/mappings, fresh outputs, foreign-cache refusal, corruption
  refusal, durable publication, ABI/ELF checks, host-change checks, typed trial
  permission, atomic success/cancel and no invented test history. They do NOT
  prove libaom/NDK/Gradle production integration.
- Live isolated admission smoke PASSED: normal approval refused; explicit trial
  permission persisted; real worker reached disk-reserve wait with no snapshot,
  stages, Gradle/Cargo invocation or runtime learning; cancellation completed and
  owned worker/daemon exited. Repeated against the FINAL binary with exit 0:
  `/tmp/task-manager-android-admission-smoke-final.json` and `.log`, state beneath
  `/tmp/tasks-android-admission-4it8f7m2`. Observed **8.2 GiB available / 20 GiB
  reserve**. This is NOT an APK build pass.
- Actual read-only `tasks android-host /home/abe/p2p_chat x86_64,arm64-v8a` passed:
  13 installed host/config identities, Rust 1.99.0 and both Android stdlibs present;
  identity recheck passed, no tool commands/download/daemon/state creation.
  `/tmp/task-manager-android-host-audit.json`. This is partial input readiness,
  not proof native compilation or APK assembly succeeds.
- Read-only cache plan completed, deletion disabled: one recent retained self-test
  target and 36 owner-unknown legacy roots; no automatic deletion candidates.
  `/tmp/task-manager-android-executor-cache-plan.json`. This does NOT authorize
  cleaning legacy roots or claim those roots are inactive.
- Full worker E2E and captured self-suite passed in the previous preparation
  slice, but have NOT been rerun for this executor slice: 20 GiB reserve currently
  prevents their worker builds. No duplicate/infinite-wait regressions launched.
- No real x86_64/dual-ABI APK, emulator/phone integration, application runtime,
  performance/release profile or signing/publishing validation was performed.
