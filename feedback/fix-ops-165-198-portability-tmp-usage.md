# Trial feedback: lock dir/PSI portability, tmp-usage, closed stdout, run CPU limit

- Agent/session: `fix-ops`, worktree `task-manager-worktrees/fix-ops`, branch `fix/ops`
  (based on 3f2f17a).
- Job ID(s):
  - Host queue: 165 (red, offload wrapper), 173, 178, 181, 190 (offload wrappers,
    `--run`), 182, 192, 196 (`cargo test --offline --locked -j 2`), 186, 193, 198
    (`cargo clippy ... -D warnings`).
  - LAN worker: 9 (red), 17 (tests), 18 (clippy, failed on my code), 21 (clippy),
    24 (tests + clippy).
- Outcome: friction (minor). No coordinator defect, apart from the client EPIPE
  behaviour below.
- Redacted command, cwd type, resource class:
  - `tasks run --run --who fix-ops -- tasks-offload --worker WORKER --who fix-ops --ssh-key KEY -- cargo test --locked -j 8`
    (host run slot, worker compile/run).
  - `tasks run --who fix-ops -- cargo test|clippy --offline --locked -j 2` (host
    compile slot).
  - All from the live worktree.
- What was exercised: real execution on both queues. I ran the new `tasks admission`
  and `tasks tmp-usage` read-only on this host. I ran the ignored closed-pipe E2E
  directly, outside `tasks run`. It used its own daemon, state and isolated
  `TASKS_LOCK_DIR`, and its worker took admission in that private directory.
- Expected: red compile failure, then green; the worker offload to behave like a
  local run.
- Observed:
  - Red: 22 compile errors on the missing API (165/9). Every later test run
    passed: 116 lib tests plus the integration targets, one ignored E2E.
  - Offload worked. Each worker job finished in well under a minute after sync.
    The host wrapper job waited only briefly in `waiting_run`.
  - The final summary lines from both queues made the results easy to read.
  - Host compile jobs logged `Native admission waiting: CPU PSI ... / memory PSI ...`
    while other agents compiled, then proceeded.
  - Read-only checks:
    - `tasks admission` on this host reports `lock_dir.source=default`
      (`/tmp/claude-1000`).
    - `tasks tmp-usage` scanned 328686 entries in 8.4 s. It found 15.6 GiB used in
      the RAM-backed /tmp, with 8.0 GiB in one agent scratch directory.
  - The first `tmp-usage` version took about 28 s of CPU because it hashed job
    ancestors per group. Fixed with a precomputed index before committing.
- Impact: low. The worker offload removes most host load for test iteration.
- Suggested improvement:
  - Not mine (CLI owner): in `wait_job`, ignore EPIPE when printing the log tail
    and summary, and still exit with the job's code. Today a closed pipe can
    make the client panic with exit 101, which looks like a Cargo failure.
- Acceptance test for suggestion: in `tests/closed_stdout.rs`, the fake daemon
  returns `phase=passed` and a log of about 200 KB. A reader closes after one line.
  Assert the client exits 0 (it exits 101 today) and still sends no request after
  Logs.
- Workaround requested/authorized: none. No gate was lowered. The isolated E2E
  used its own lock directory and did not hold shared slots.
