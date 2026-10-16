# Android foundation: source capsules, preflight and independent wait clocks

**Historical preparation slice.** Managed execution lanes, build-only completion
and explicit experimental approval were subsequently implemented. See
`ANDROID-EXECUTION-MILESTONE.md` for CURRENT status, disk blocker and trial policy;
normal catalog execution is still gated. Remaining-work sections below record
what was outstanding at the end of this earlier slice.

## Status / scope

Implemented preparation, NOT Android job execution. The Android catalog entry is
still `executable: false`; approval policy and production worker dispatch have not
been relaxed. No APK was built/installed by this slice. Do not mistake a plan or
source capsule for a successful Android build.

Active source: `.claude/worktrees/task-manager-next/tools/task-manager/`.

## Reconcile with dev, without merging/resetting

Reviewed dev `ae764ee` (Gradle output/cache placement), `39c6f69` (AppImage/release
CI), then `e43be4e` (ignore Android build symlinks). In the owned worktree only:

- `scripts/buildlock.py` and `AGENTS.md` now match dev's versions exactly.
- `.gitignore` incorporates the committed Android build-link ignore fix, without
  copying the integration checkout's unrelated uncommitted edits.
- Canonical per-checkout `target/` is used for bootstrap checks; no manual
  `CARGO_TARGET_DIR`, wrapper or Gradle-home environment override was added.
- Buildlock created missing Android output symlinks in this owned checkout. No
  existing directories were migrated or removed. Other checkouts/device sessions
  were left alone. No branch switch, merge, stage, commit or install occurred.

## New local preparation commands

```sh
TASKS=/home/abe/p2p_chat/.claude/worktrees/task-manager-next/target/debug/tasks
SOURCE=/home/abe/p2p_chat
REV=$(git -C "$SOURCE" rev-parse HEAD)
"$TASKS" android-plan "$SOURCE" "$REV"              # default x86_64
"$TASKS" android-plan "$SOURCE" "$REV" arm64-v8a
"$TASKS" android-plan "$SOURCE" "$REV" x86_64,arm64-v8a
# Explicitly create a NEW capsule outside the source checkout; parent must exist:
"$TASKS" android-capture "$SOURCE" "$REV" /owned/parent/new-capsule
```

Neither command starts a daemon or invokes submitted source, Cargo, Gradle, adb,
network fetches or device tools. Capture writes only a new destination and never
replaces an existing directory/symlink. Partial new destinations are retained on
failure. Receipt JSON is printed and belongs outside the capsule.

### Source contract (`src/workspace_snapshot.rs`)

- Separate from the existing standalone self-suite snapshot contract, which has
  NOT been widened or silently repurposed.
- Copies root Cargo manifest/lock, optional Rust toolchain descriptors, all files
  under `crates/`, `android/` and `scripts/`, including dirty/untracked assets,
  native C, generated icon INPUTS, Gradle wrapper and Kotlin sources.
- Excludes generated Cargo targets, Android build/.gradle/.kotlin/.cxx output,
  Python bytecode and local SDK properties. Static source symlinks/special files
  are rejected; excluded output links are never followed. Private `.env*`, `.jks`
  and `.keystore` files are rejected. This is not a general secret scanner.
- Rejects unreviewed root `.cargo` configuration, escaping/missing manifest path
  references and missing explicit workspace-member manifests (globs unsupported).
- Limits: 5,000 files, 128 MiB total, 16 MiB/file, 20,000 traversed entries, depth
  64. These are source limits, not target-cache quotas.
- Two inventories + HEAD checks detect ordinary concurrent edits; SHA-256 binds
  sorted paths, content, length and executable bits. Gradlew remains executable;
  copied files/directories are sealed read-only, then exact verification rejects
  added outputs/metadata. This is not an atomic filesystem snapshot or sandbox.
- This closes the known current source roots, NOT arbitrary build-script reads,
  system toolchains, global Gradle/Cargo config, dependency caches or native
  installs. Those external inputs still require identity/policy checks before
  worker execution. Future build-input paths need an explicit contract update.

### Preflight / pipeline contract (`src/android.rs`)

Typed ABIs only: x86_64, arm64-v8a, or both. It plans per-ABI pinned libaom steps,
then ONE Gradle assembly. Gradle already runs cargo-ndk and host UniFFI generation;
calling those again would duplicate work. The proposed first configuration uses
Rust debug, debug dev-tools, HD experiment off, `--offline`, `--no-daemon` and
`:app:assembleDebug`. No install/release/bundle/publish action is represented.

Preflight hashes bounded installed NDK/SDK/JDK/wrapper marker files and verifies
the cached libaom tarball against the captured source pin. Missing/corrupt inputs
are reported, never downloaded/deleted/repaired. Markers are partial identities,
not hashes of whole toolchains. A verified tarball alone never enables execution.
Gradle offline mode is not a network sandbox; wrapper/native-source availability
and dependency policy must be checked before enabling the runner.

## Validation

- Python wait-clock models: **4 passed**, 0 skipped; command:
  `PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tools/task-manager/tests -p test_wait_budget.py -v`.
- Bootstrap through canonical shared admission/target:
  `python3 scripts/buildlock.py --who pi-task-manager-android-final -- sh -c 'cargo test --locked --offline --manifest-path tools/task-manager/Cargo.toml --target-dir target && cargo clippy --locked --offline --manifest-path tools/task-manager/Cargo.toml --target-dir target --all-targets -- -D warnings && cargo build --locked --offline --manifest-path tools/task-manager/Cargo.toml --target-dir target'`.
  **66 Rust tests passed, 0 failed/ignored**, clippy warnings-denied clean, build
  passed. `/tmp/task-manager-android-final-checks.log` ended `VALIDATION_EXIT=0`.
  Counts: 45 library, 4 binary, 7 contracts, 4 reconnect, 5 snapshot, 1 socket;
  doc-tests contained 0 tests.
- The single conditional sequencer completed both real worker regressions with
  exit 0 after bootstrap success (no outer buildlock, separate admission clocks).
  Final summary `/tmp/task-manager-android-regressions.json` is `status: passed`;
  source/binary fingerprints remained stable. No owned validation remains queued.
  Full worker E2E: `/tmp/task-manager-android-worker_e2e.log`, retained fixtures
  `/home/abe/build-cache/tm-fixtures/task-worker-e2e-r5_aq442`. Includes real low
  disk/budget refusal, active/unfinished target protection, cancellation/crashes,
  lease release, durable results and snapshot-tamper refusal.
- Actual captured self-suite also passed all **66 Rust tests** through the worker:
  `/tmp/task-manager-android-self_suite.log`, state
  `/tmp/task-worker-self-suite-deb2v_6t`, snapshot 23 files / 320,033 bytes.
  Observed build 82.97 s, harness execution 0.91 s (not speedup guarantees).
  Commands: `python3 tools/task-manager/tests/worker_e2e.py "$PWD/target/debug/tasks" --timeout 300 --queue-timeout 0`
  then `python3 tools/task-manager/tests/self_suite.py "$PWD/target/debug/tasks" "$PWD" --timeout 600 --queue-timeout 0`,
  from the active worktree.
- Initial bootstrap stopped after 44 library tests passed and one existing
  retention test observed a transient inherited lease after another test forked.
  Idle-fixture expectations now retry only `protected_active` for up to 3 seconds;
  deliberate active-lease assertions do not retry. Production lease/planning
  behavior remains conservative and unchanged. A subsequent all-66 pass exposed
  one clippy unused-unit expression in the new collector; it was removed, then
  the full checks and worker regressions above passed. The failed sequencer stopped
  before launching integration checks; corrections were retried only after its
  processes exited. No failed test was ignored.
- Actual dev preparation passed at `e43be4e86bdbe8b7fe06c6214814a0cd13324e56`:
  **745 files / 13,833,401 bytes**; pinned libaom 3.15.2 tarball verified; all four
  installation markers present; no daemon/state created. Plan log:
  `/tmp/task-manager-android-dev-plan.log`.
- Actual dev capture copied, verified and sealed the same source inventory at
  `/home/abe/build-cache/tm-fixtures/android-capsule-b2r3srwg/source`, receipt next
  to it. Digest `d7e65a9d31770dd90d4fdf8eb5672df4a8e93778d41cae29f47e7806ca5c6ee0`.
  Log: `/tmp/task-manager-android-capture.log`. No APK/Gradle/native compilation
  was performed; this validates preparation, NOT production build integration.

## Wait vs execution timeout

`tests/worker_e2e.py` and `tests/self_suite.py` now use independent observed clocks:

- `--timeout` counts observed building/running time only.
- `--queue-timeout` limits preparation/admission separately; default 0 is unlimited.
- No restart/duplicate submission occurs on timeout. Test-owned process cleanup
  remains unchanged; no foreign process is signalled.
- These clocks are sampled validation helpers, not a new worker timeout policy;
  durable stage observations remain authoritative. Unit models do not prove live
  long-running Android integration. The prior admission-blocked E2E remains a
  historical inconclusive attempt; the new completed worker E2E above passed.

## Next execution slice — required before catalog activation

1. Materialize a new writable build tree from a verified capsule, keeping the
   immutable receipt/source. Prepare and lease canonical Cargo/JNI/UniFFI/Gradle
   project output mappings. UniFFI reads `repo/target`; an environment-only target
   override is not sufficient. Do not share worktree target directories.
2. Define Android/native lane ownership and budgets, separate from the self-test
   family/8 GiB policy. Shared Gradle/compiler caches are not exclusively leased
   or subject to this target-retirement planner.
3. Record effective Rust/JDK/SDK/NDK/Gradle executable identities, selected ABI/
   profile, native archive/header identity and offline dependency/wrapper policy.
   Review user/global config without copying release signing credentials.
4. Use internal buildlock once per heavy stage, not an outer lock around
   `build_android.py` (which already locks internally). Wire cancellation/crash
   cleanup and inherited output leases through all stages, including artifact copy.
5. Add atomic build-only completion: preserve fresh APK in durable job results,
   SHA-256/size/source/toolchain metadata; do not invent a successful test run or
   runtime observation. Current completion requires build+run and cannot be
   reused unchanged.
6. Validate x86_64 build-only first, then both ABIs. Only then enable the catalog
   entry. Installation, launch, data wipe and release publishing remain separate,
   explicitly authorized policies.
