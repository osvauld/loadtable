# Worker-owned cache/admission — first slice

Active source: `.claude/worktrees/task-manager-next/tools/task-manager/`, branch
`tools/task-manager-next`. No global scripts or old worktrees were edited; no
existing daemon was restarted or active build interrupted. Validation used owned
targets/fixtures and the shared sccache. No legacy cache was deleted/migrated and
no staging/commit/install occurred.

## Implemented

- `tasks cache`: read-only policy/free-space report; no daemon startup, target
  creation or cleanup.
- Worker configures sccache after clearing inherited command environment. Fixed
  shared cache `~/build-cache/sccache`, requested limit `30G`, discovered absolute
  wrapper path; missing sccache is explicit in per-job policy/logs.
- Disposable validation disables incremental output and dev/test debug info.
- Default self-test target is one reusable directory,
  `~/build-cache/p2p_chat/tasks/targets/self-tests`, not one directory per state.
- Target lease spans capture/build/run and owned cleanup, including across states
  selecting the same canonical directory. `.tasks-target.lock` is never unlinked.
- Worker waits cancellably for target ownership and disk reserves. Build/cache
  filesystems need 20 GiB available; state needs 256 MiB (state may live on tmpfs).
  Phase detail and logs expose the blocking reason. No compile slot is held while
  waiting for target/disk. Disk availability is rechecked after build admission.
- Completion now calls the existing atomic `Store::complete_success` transaction,
  rather than separately teaching successful timings before finishing a job.
- Catalog includes disabled core compile, desktop build and Android APK proposals,
  with fixed proposed commands, resource/permission descriptions and blockers.
  Only the standalone self-suite is executable. See `AGENT-WORKFLOW.md`.

### Inherited lease channels

Internal `__stage` / `__command` reserve stdin for the worker lease and stdout
for the target lease. They do not emit user output on stdout. Standard file
descriptors survive Python's normal `subprocess` close-fds boundary; a private fd
would be lost before the admitted command. All command output uses explicit
bounded job logs (Cargo/harness output is captured by the command runner).
Supervisors retain both leases while terminating their owned process group.

Do not add internal stdout logging without preserving this lifetime contract.
The lock is cooperative, not protection against a same-user writer ignoring it.

## Validation commands and scope

Compile/test/clippy/build passed through the shared queue (do not duplicate a
waiting/running entry after a timeout):

```sh
cd /home/abe/p2p_chat/.claude/worktrees/task-manager-next
export CARGO_TARGET_DIR=/home/abe/build-cache/p2p_chat/task-manager-continuation-target
python3 /home/abe/p2p_chat/scripts/buildlock.py --who pi-task-manager-cache -- sh -c \
  'cargo test --locked --offline --manifest-path tools/task-manager/Cargo.toml &&
   cargo clippy --locked --offline --manifest-path tools/task-manager/Cargo.toml --all-targets -- -D warnings &&
   cargo build --locked --offline --manifest-path tools/task-manager/Cargo.toml'
```

**43 Rust tests passed, 0 failed, 0 ignored**: 22 library, 4 binary, 7 contracts,
4 reconnect, 5 snapshot, 1 socket. Doc-tests contained 0 tests. Clippy passed with
warnings denied and the binary build passed. Initial log:
`/tmp/task-manager-cache-checks.log`. A final rerun after formatting also passed:
`/tmp/task-manager-cache-final-checks.log` (same command, `--who
pi-task-manager-cache-final`).

New deterministic checks cover the reserve boundary, non-creating disk queries,
exclusive target leases/clone lifetime, symlink refusal, inherited leases across
a real Python subprocess boundary, and sanitized compilation-cache policy.
These do not prove successful integration of unimplemented Tinline workloads.

Opt-in worker E2E (run directly, NOT wrapped in buildlock; stages take admission):

```sh
python3 tools/task-manager/tests/worker_e2e.py "$CARGO_TARGET_DIR/debug/tasks"
```

The E2E now uses a unique cache-disk fixture rather than building on /tmp tmpfs,
checks cancellable target contention, persists cache policy, and checks target
lease release after worker/supervisor loss. The real low-space test uses
`/dev/shm` (requires less than 20 GiB available) and checks that no target lease
or build is started, then cancels the wait. Crash-fixture stale holder notes are
removed only for recorded exact supervisor identities/dead holder PIDs, while
holding the corresponding real slot; other agents' notes/locks are untouched.
Initial E2E passed: `/tmp/task-manager-cache-e2e.log`, fixtures
`/home/abe/build-cache/tm-fixtures/task-worker-e2e-pphxffhv`.
Final extended E2E **passed all checks**:
`/tmp/task-manager-cache-final-e2e.log`; fixtures
`/home/abe/build-cache/tm-fixtures/task-worker-e2e-ydtwa7ro`.

Actual captured self-suite also passed through the worker, with all 43 tests
freshly executed (0 failed/ignored). Command:

```sh
python3 tools/task-manager/tests/self_suite.py "$CARGO_TARGET_DIR/debug/tasks" \
  /home/abe/p2p_chat/.claude/worktrees/task-manager-next \
  --target-dir /home/abe/build-cache/p2p_chat/task-manager-worker-validation-target \
  --timeout 900
```

Log `/tmp/task-manager-cache-self-suite.log`; state/snapshot/results retained at
`/tmp/task-worker-self-suite-oqmsp2kl`. Build runtime 44.40 s; fresh harnesses 0.85 s
(these are observations, not speedup or future ETA guarantees). Captured the live
worker-spawned Cargo environment in `/tmp/task-manager-cache-observed-env.log`:
absolute sccache wrapper, shared cache path/30G request, incremental=0, dev/test
debug=0. This verifies the sanitizer does not discard the worker's cache policy.

`tasks cache` and `tasks catalog` JSON output were manually inspected. Cache
reporting with a nonexistent state path did not create the state directory.

## Subsequent slice

`RETENTION-MILESTONE.md` now records the target admission ceiling and read-only
ownership-aware retirement planner. The remaining list below describes the
limitations at completion of this earlier slice; no deletion is enabled.

## Remaining / not promised (first slice)

- The free-space gate is not a quota/reservation and cannot prevent unrelated
  programs from consuming disk during a build. Failure after queue admission
  requires a new submission; it is not silently retried.
- The sccache limit is a requested server setting. Existing servers can retain
  their original configuration; no server is killed/restarted by this code.
- No target size budget, pruning, artifact retirement, cache-result reuse or
  deletion is enabled. Existing legacy caches have unknown ownership/leases.
- Core/desktop need full workspace, build-script and native-input capture and
  build-only result contracts. Android additionally needs generated inputs,
  SDK/NDK identity, dependency/network decisions and target-path relocation.
- No real core/desktop/Android execution, device action, native-window interaction,
  cache hit-rate/speedup guarantee or power-failure cleanup is validated here.
- Approval and worker-start policy remain explicit; automatic execution is not
  enabled just because new proposals are visible in the catalog.
