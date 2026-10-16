# Target budgets and read-only retirement planning

Active source: `.claude/worktrees/task-manager-next/tools/task-manager/`, branch
`tools/task-manager-next`. This is the second cache slice; `CACHE-MILESTONE.md`
records the earlier sccache/lease/disk-admission validation.

## Implemented

- `tasks cache-plan`: local JSON dry-run, no daemon startup, directory/lock
  creation or deletion. It briefly probes existing target leases. Old cache roots
  are listed as owner-unknown without recursively walking them.
- Self-test targets have an **8 GiB allocated-space admission ceiling**, checked
  while leased before source capture and again after compile-slot admission.
  Incomplete/unreadable inventories refuse new work rather than assuming zero
  usage. A rejected job fails with a reason and requires a new submission after
  review; it does not clean anything or silently retry. This is not a quota:
  an admitted build can grow past the threshold, blocking the next job.
- Metadata-only inventories deduplicate internal hardlinks, count allocated blocks
  (not sparse-file apparent length), intentionally skip symlinks and other
  filesystems, and fail closed on unsupported/unreadable entries. Limits: 50,000
  entries, depth 64, roughly 2 seconds/target checked between operations. Plans
  inspect at most 16 targets, with a roughly 5-second overall inspection budget;
  root enumeration is capped at 256 entries. Kernel I/O latency is not bounded.
- Cooperating workers initialize ownership ONLY for a selected empty target
  (apart from its stable lease file). Populated legacy targets are never adopted.
  Ownership binds schema, manager/family, canonical path, user, directory device/
  inode and stable lease device/inode. Copying a marker or replacing a lease does
  not confer ownership. These directories must be reserved for disposable build
  outputs, never user data/state/source. A marker is not per-file proof of origin;
  review is still required. Same-user hostile edits are not a security boundary.
- Managed use records start/finish under the target lease. Worker death leaves
  unfinished usage; those targets are protected until a clean managed use or
  explicit review. Metadata-write failure leaves conservative protection.
- Plans protect active targets (no inventory), unknown/invalid provenance,
  unfinished usage, incomplete inventories, symlinks, special files and external
  hardlinks. Recent clean targets are retained. Only clean, idle, manager-owned
  targets at/over budget or unused for 7 days become **review candidates**.
- Candidate byte counts exclude the stable lease and owner/usage controls, and
  are estimates, not guaranteed freed space. No job results, logs, snapshots,
  shared sccache or legacy root is included in a removal operation. There IS NO
  removal/apply operation in this milestone.

## Validation

From the active worktree, through the current shared build queue:

```sh
export CARGO_TARGET_DIR=/home/abe/build-cache/p2p_chat/task-manager-continuation-target
python3 /home/abe/p2p_chat/scripts/buildlock.py --who pi-task-manager-retention-final -- sh -c \
  'cargo test --locked --offline --manifest-path tools/task-manager/Cargo.toml &&
   cargo clippy --locked --offline --manifest-path tools/task-manager/Cargo.toml --all-targets -- -D warnings &&
   cargo build --locked --offline --manifest-path tools/task-manager/Cargo.toml'
```

**53 Rust tests passed, 0 failed, 0 ignored** (32 library, 4 binary, 7 contracts,
4 reconnect, 5 snapshot, 1 socket; doc-tests contained 0 tests). Clippy with
warnings denied and the binary build passed. Log:
`/tmp/task-manager-retention-final-checks.log` ended with `VALIDATION_EXIT=0`.
One initial run passed its then-52 tests but clippy rejected an unused import;
it was removed.
A subsequent foreground queue wait timed out before compilation, and its process
exit was confirmed before the single background queue entry was created. Do not
launch duplicate jobs while the existing entry waits/runs.

New checks cover missing/legacy non-adoption, clean/active/recent lifecycle,
read-only candidate planning and stable inode preservation, unfinished usage,
copied ownership/replaced leases, hardlink accounting, no symlink traversal,
bounded/incomplete inventories, special files and malformed/symlinked control
files (including nonblocking FIFO refusal), and non-recursive legacy discovery.
Age/budget boundary checks are deterministic models, not proof of native workload
integration or performance.

Opt-in real worker E2E (NOT inside an outer buildlock):

```sh
python3 tools/task-manager/tests/worker_e2e.py "$CARGO_TARGET_DIR/debug/tasks" --timeout 900
```

Extended scenarios verify new target ownership, protected active/unfinished usage,
and refusal of an unmeasurable target budget before building, without removing
previous results/artifacts. No multi-GiB disk-filling fixture is used. Results
not completed: extended E2E timed out after 900 seconds waiting for shared
compile admission on its first build (`job 2`, `waiting_build`). Log:
`/tmp/task-manager-retention-e2e.log`. The orchestrator and its first build stage
were no longer running at the subsequent inspection. No duplicate was launched.
This is admission-blocked/inconclusive, not a passing worker E2E result. Actual
captured self-suite validation also remains pending for this slice.

Live `tasks cache-plan` output was inspected at
`/tmp/task-manager-retention-plan.json`: deletion disabled, default target missing,
31 legacy roots kept owner-unknown, and no state/target directory created.

While the first E2E job waited for the shared compile slot, a live probe also
verified `protected_active`, `inventory: null` and unfinished managed usage. Probe
output: `/tmp/task-manager-retention-active-plan.json`; current E2E state:
`/home/abe/build-cache/tm-fixtures/task-worker-e2e-b86nv6xq/state`. This proves the
queued-worker lease/provenance path, not completion of the remaining E2E checks.

## Subsequent completed regression

The Android-foundation continuation re-ran and passed the extended worker E2E,
including the budget/FIFO, active-target and unfinished-usage scenarios, with
independent admission clocks. It also passed the actual captured 66-test self-suite.
See `ANDROID-MILESTONE.md`; logs `/tmp/task-manager-android-worker_e2e.log` and
`/tmp/task-manager-android-self_suite.log`. This supersedes pending validation
above, not the historical record of the initial admission timeout. No Android APK
execution was enabled by these task-manager regressions.

## Rollout / remaining

- No cleanup has been applied to existing caches. Legacy targets and older
  task-manager validation caches remain owner-unknown even if they have a lock.
- Admission limits are per-target, not a global fleet budget or filesystem quota.
  The stable default avoids one new permanent target per state; manual overrides
  can still grow aggregate usage. Sccache's requested server cap is separate.
- A future authorized cleanup must re-acquire the same stable lease, revalidate
  ownership/usage and a fresh complete inventory, preserve control inodes and
  user results, then remove only approved disposable contents. Do not treat an
  old plan JSON as authorization or as an atomic filesystem snapshot.
- Inventories are cooperative, not proof against hostile same-user filesystem
  races, escaping processes, power failures or mount changes. No deletion API
  exists to amplify those limitations here.
- Full workspace/native-input capture and build-only result contracts remain
  blockers for core/desktop; Android also needs Gradle/generated inputs, SDK/NDK,
  network/dependency policy and target relocation. Those proposals stay disabled.
- No service installation, live daemon replacement, GUI/device operation, branch
  switch, staging, commit or merge was performed.
