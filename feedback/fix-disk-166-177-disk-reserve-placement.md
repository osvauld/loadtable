# Trial feedback: disk reserve, placement and fresh-worktree fixes (source only)

- Agent/session: `fix-disk` subagent, worktree `task-manager-worktrees/fix-disk`, branch `fix/disk` (from `feature/osvauld-foundation` 3f2f17a).
- Job ID(s):
  - Host queue: 166 (offload, compile error), 167 (offload test, green), 169 (offload red run), 171 (offload clippy), 174 (host `cargo test`), 177 (host clippy + build).
  - LAN worker (remote IDs): 10 (compile error), 11 (test), 13 (red), 15 (clippy).
- Outcome: no coordinator defect observed. Some friction (PSI waits, and the socket-path length limit in an isolated test).
- Redacted command, cwd type, resource class:
  - `tasks run --run --who fix-disk -- tasks-offload --worker <lan-worker> --who fix-disk --ssh-key <key> -- cargo test|clippy --locked -j 8`, live worktree cwd, run class on this host.
  - `tasks run --who fix-disk -- cargo test --offline --locked -j 2` and `sh -c 'cargo clippy ... && cargo build ...'`, live worktree cwd, compile class.
- What was exercised:
  - Real execution through the installed shared queue and the LAN worker.
  - Two isolated test instances of the NEW source binary, using a private state dir under `~/build-cache/tm-fixtures` and synthetic git fixtures:
    - with `TASKS_MIN_CACHE_FREE_GIB=65536`: disk wait, then cancel, plus an existing-link job;
    - with a worker PATH lacking git: `/usr/bin/true` in a non-Git dir.
  - The installed runtime was not replaced. No daemon restart.
- Expected: red run, then green tests and clippy on both hosts; the isolated instances show the new wait and admission fields.
- Observed:
  - Job 169 (red): I restored the old "check the cache whenever an output root exists" rule in `OutputPlan::reserve_paths`. Result: 3 of 16 placement tests failed (existing links + low disk; preserved output on its own filesystem; nothing placed outside Git). Then I restored the source.
  - Jobs 167/11 and 174: 107 lib + 5 bin + 7 + 4 + 5 + 1 tests passed, 0 failed, 0 ignored. Jobs 171/15 and 177: clippy `-D warnings` clean; build OK.
  - Isolated instance A, with the reserve set above the free space:
    - `tasks admission` showed `disk_blocker: "cache disk below reserve, jobs needing new cache output wait before admission: /home/abe/build-cache: 82.1 GiB available, 65536.0 GiB reserve"`, plus `cache_disk` and `state_disk`.
    - The job needing a new link stayed `preparing` with detail `Waiting for disk reserve: ...`, and no link was created.
    - At the same time, a job whose `target` link already existed passed.
    - `tasks cancel` gave `cancelled` with "Cancelled while waiting for disk reserve". No shared admission was ever taken.
  - Isolated instance B (no git): the job passed. Its log says `output placement: git not found on the worker PATH and no .git entry above the cwd; placement skipped`.
  - Friction:
    - A state dir under the session scratchpad failed with `path must be shorter than SUN_LEN` (the Unix socket path limit). I moved only the state dir to `~/build-cache/tm-fixtures`, which `tests/native_commands.py` also uses. Those fixture dirs were removed afterwards.
    - Host jobs waited several minutes on memory/I/O/CPU PSI. Each wait was logged clearly.
  - Offloading to the LAN worker worked well: a cold compile took about 54 s, and later runs took 10-17 s. That kept host load low, and the final JSON line made the result easy to read. A failed remote compile is reported as exit 101 on both the remote and the local job.
- Impact: none of this is deployed. These are source fixes awaiting review and an idle-window promotion.
- Suggested improvement:
  1. The "proceeds once space appears" part has no end-to-end test. The worker resolves thresholds once per process, so a test instance cannot lower its reserve live. A test-only space-probe override (env or file) would make that path testable end to end.
  2. The isolated-instance socket path limit could be named in README "Development". The error could also suggest a shorter `TASK_MANAGER_STATE_DIR`.
- Acceptance test for suggestion: (1) an isolated instance with a probe override starts a waiting job after the override is cleared; (2) a state dir longer than `SUN_LEN` fails with a message naming `TASK_MANAGER_STATE_DIR`.
- Workaround requested/authorized: none. No gate was lowered, no cache was cleaned, the installed runtime was not changed.
