# Trial feedback: nested Cargo workspace target placement (source fix)

- Agent/session: tm-placement (worktree `task-manager-worktrees/osvauld-foundation`, branch `feature/osvauld-foundation`, unborn HEAD)
- Job ID(s):
  - 51: failed before admission, coordinator-side
  - 57: failed at the cache-disk gate
  - 69: red run
  - 70: green filtered run
  - 72: full test suite
  - 73: clippy
- Outcome: the fix is implemented and validated in source. Along the way there was one blocker (cache-disk reserve, cleared externally) and one coordinator defect observation (job 51).
- Redacted command, cwd type, resource class: `tasks run --who tm-placement -- cargo test|clippy --offline --locked -j 2 ...`; cwd = worktree root (root Cargo workspace); compile
- What was exercised: real execution through the installed shared coordinator. The tests use synthetic tempdir Git checkouts and a private cache root. This is NOT a deployment; the installed runtime is unchanged.
- Expected: a filtered red run against the old root-only behaviour, then green, then a clean full suite and clippy.
- Observed:
  - **Job 51:** the client printed `Resource temporarily unavailable (os error 11)` and exited while the job was WaitingBuild. The job log showed PSI admission waits, then `supervisor: stopping owned stage group (cancel/worker gone)`, `native admission cancelled or owner disappeared`, `worker error: database is locked`. Final phase `failed`, detail `database is locked`. I did not cancel it. Jobs 48 and 55 (other owners) became `interrupted` in the same window. I resubmitted only after 51 was terminal.
  - **Job 57:** admitted, then `insufficient disk space: /home/abe/build-cache: 18.4 GiB available, 20.0 GiB reserve`, exit 2. A bounded 20-minute poll saw no recovery. No cleanup or gate change was attempted. The blocker was later cleared externally (about 98 GiB free).
  - **Job 69 (red):** `cargo_workspace_root` was temporarily stubbed to return `None`, giving the old root-only behaviour. Result: 3 passed, 5 failed, 0 ignored (74 filtered out).
    - Failed: `nested_independent_workspace_gets_its_own_cache_target_for_any_argv`, `nested_workspace_below_a_non_cargo_git_root_is_placed`, `populated_existing_nested_target_is_preserved_and_reported`, `excluded_or_standalone_package_is_its_own_workspace`, `same_nested_layout_in_different_checkouts_is_isolated`.
    - Passed as expected: root unchanged, member package, and the nothing-placed cases.
    - The stub was removed afterwards.
  - **Job 70 (green, filtered):** 8 passed, 0 failed, 0 ignored (74 filtered out).
  - **Job 72 (full `cargo test --offline --locked -j 2`):** 103 passed, 0 failed, 0 ignored, broken down as lib 82, bin 4, contracts 7, reconnect 4, snapshot 5, socket 1, doctests 0.
  - **Job 73:** `cargo clippy --offline --locked -j 2 --all-targets -- -D warnings` exit 0.
  - The disk gate runs in `place_outputs` whenever a Cargo root is found, even when its links already exist, so a low cache disk blocks every Cargo job in such checkouts.
- Impact: before the fix, the nested independent workspace (`experiments/osvauld-foundation`) built into a local ~1.2 GiB `target/` outside the cache. After deployment, a nested workspace without `target` gets a cache-backed link. Existing populated dirs stay local and are reported in the job log.
- Source change:
  - **`src/local_command.rs`:**
    - `place_outputs` returns `Result<Placement>` (`linked`, `preserved`, `detail()`).
    - Root links are unchanged (same keys and paths).
    - The cwd's own Cargo workspace root, when below and different from the Git root, gets its own `target` link, keyed by SHA-256 of its canonical path. The Git root does not need a Cargo.toml.
    - Workspace-root model: nearest manifest; its `[workspace]`; else `package.workspace`; else the nearest non-excluding ancestor `[workspace]`; else the package itself. The search is bounded to the Git root. An unparseable manifest means no nested placement.
    - Existing entries are never replaced; non-symlink entries are reported in `preserved`. An `AlreadyExists` race is tolerated.
    - No environment is set (RUSTC_WRAPPER, CARGO_TARGET_DIR and GRADLE_USER_HOME untouched).
    - Placement is keyed on the job's cwd only, so direct Cargo argv and `sh -c` argv behave the same.
  - **`src/worker.rs` (one approved change):** `Placement::detail()` is prefixed to the existing `live cwd=... argv=...` job-log line. It is empty when nothing was placed or preserved.
- Suggested improvement:
  1. Deploy this source only through the normal idle-window promotion (not done here).
  2. Investigate job 51's `database is locked` worker error. A busy SQLite should back off and retry, not fail a waiting job.
  3. Consider running the cache-disk gate only when a new link must be created, or document that it applies to every Cargo job.
- Acceptance test:
  - Jobs 70, 72 and 73 above, as source evidence.
  - After an authorized deployment: a job with cwd `experiments/osvauld-foundation` whose `target` is a populated real dir leaves it untouched (same inode) and logs `existing local output preserved, not cache-backed: .../target`.
  - A fresh nested independent workspace without `target` gets a symlink into `~/build-cache/p2p_chat/tasks/checkouts/<sha256(canonical path)>/target`, for both a direct `cargo` and an `sh -c '...'` argv.
  - The root workspace's link target is unchanged, and two checkouts never share a target.
- Workaround requested/authorized: build-cache headroom was restored externally by the coordinator. The worker.rs log line was explicitly approved. No bypass, direct cargo or cleanup by me.

Limitations:
- Cargo's real discovery also walks above the Git root and validates `members` globs. This model stops at the Git root and ignores `members`, so a mis-declared workspace still fails in Cargo as before.
- A workspace selected only through `--manifest-path` is not placed, because placement is keyed on the cwd.
- An existing populated local `target` is never migrated.
- The deployed runtime is unchanged, so none of this is in effect yet.
