# Trial feedback: crate-total fix, offloaded cargo test/clippy via --peer nixos

- Agent/session: crate-total
- Job ID(s): local 4556 (lib tests, 2 failures of mine), 4559, 4561 (lib tests, only 4 admission failures left), 4562 (bin tests, 2 stale UI strings), 4564 (bin tests passed), 4566 (clippy passed)
- Outcome: friction (first job waited ~29 min on worker admission); otherwise no issues observed
- Redacted command, cwd type, resource class: `tasks run --peer nixos --who crate-total -- cargo test|clippy --offline --locked -j 2 ...`, own worktree, offload
- What was exercised (real execution vs model/read-only observation): real forwarded runs on the worker. The new progress env is NOT active in those runs (the worker runs the installed release), so no live N/M was observed.
- Expected: job 4556 to start within a few minutes.
- Observed: job 4556 logged repeated "Native admission waiting: available memory below 1 GiB" / "CPU PSI some avg10 above 40%" for ~25 min before compiling. `tasks run` hit the 10 min tool timeout and moved to the background; `tasks list --who X --brief` then `tasks logs ID` was enough to follow it.
- Impact: slow iteration only.
- Suggested improvement: none from this use.
- Acceptance test for suggestion: N/A
- Workaround requested/authorized (if any; no silent bypass): none; waited.

Four admission tests fail on the worker (admission::offload_tests::offload_pool_has_its_own_lock_files, admission::tests::{pressure_is_rechecked_after_lock_and_recovers_without_job_replay, rust_lock_excludes_python_and_python_excludes_rust, status_is_read_only_and_does_not_trust_stale_holder_notes}); they test lock files/pressure, code this change does not touch, and look environmental (e.g. three compile lock files exist there). Not checked against an unmodified baseline.
