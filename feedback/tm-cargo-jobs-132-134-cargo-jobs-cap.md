# Trial feedback: configurable Cargo jobs cap (TASKS_MAX_CARGO_JOBS)

- Agent/session: tm-cargo-jobs (subagent, osvauld-foundation worktree)
- Job ID(s): 132 (red test), 133 (green test), 134 (clippy)
- Outcome: no issues observed
- Redacted command, cwd type, resource class:
  `tasks run --who tm-cargo-jobs -- cargo test --offline --locked -j 2` (x2) and
  `... cargo clippy --offline --locked --all-targets -j 2 -- -D warnings`;
  live Git worktree; compile.
- What was exercised (real execution vs model/read-only observation): real
  execution through the installed queue. 132 failed to compile as intended
  (new tests referenced missing items); 133 passed (93 lib tests plus the
  binary/integration test targets, 0 ignored); 134 passed. The new cap was
  covered by unit tests only, not by a daemon/worker started with the
  variable set.
- Expected: queued, admitted, logged, and the exit status reported correctly.
- Observed: as expected. Job 134 first waited briefly on "memory PSI full
  avg10 above 0.5%" and was then admitted without any action from me.
- Impact: none.
- Suggested improvement: none from this use.
- Acceptance test for suggestion: N/A
- Workaround requested/authorized: none
