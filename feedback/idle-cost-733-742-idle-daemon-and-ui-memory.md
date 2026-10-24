# Trial feedback: idle daemon CPU and window memory (fix/idle-cost)

- Agent/session: idle-cost (fix/idle-cost worktree, /home/abe/task-manager-worktrees/idle-cost)
- Job ID(s): local 733 (bench of the installed release, run slot, pass), 735-738 (cargo check
  -j 2; 735 my compile error), 734 -> worker 264 (baseline release build + bench, pass),
  739 -> worker 265 (clippy pass, release build + bench pass, `cargo test` failed in 2 peer
  tests), 740 -> worker 266 (full suite --no-fail-fast + `--test peer` serial, both pass),
  742 -> worker 268 (clippy, ignored memory measurement, UI tests, pass).
- Outcome: friction (one flaky pair of tests; stale agent wait loops), otherwise worked.
- Redacted command, cwd type, resource class: `tasks run --who idle-cost -- cargo check
  --offline --locked -j 2 --all-targets` (compile); `tasks run --run -- python3 -I
  scripts/idle_bench.py --bin <installed release>` (run); `tasks run --offload -- tasks-offload
  ... -- sh -c 'cargo clippy/test/build --locked -j 6 ...; echo exit=$?'` (offload).
- What was exercised: real local check/run jobs and real worker offloads; read-only /proc
  sampling of the user's daemon and window (no signals); isolated daemons on synthetic history.
- Expected: offloaded `cargo test --locked -j 6` passes on unchanged peer code.
- Observed: worker 265 failed `tests/peer.rs` `mcp_anywhere_goes_where_the_compile_slot_is_free`
  and `controller_sees_worker_disk_alerts_and_libclang_failure_marks_the_peer`: placement chose
  the other machine because the worker's own I/O/memory PSI was high while the suite ran
  ("blocked: I/O PSI full avg10 above 10%"). The same code passed in 266 (all targets, and the
  peer target with --test-threads=1). Separately: three other agents' shell loops
  `until tasks logs N 2>/dev/null | grep -qE "^job N (passed|...)"; do sleep 15..30; done`
  (about 20 h old, jobs 521/528/533) can never end, because the `job N passed` line goes to
  stderr; each iteration was a full-listing request (~0.17 s daemon CPU) - most of the laptop
  daemon's idle CPU (2.16 CPU s/min sampled over 35 min).
- Impact: a red offload for unrelated reasons costs a 10 min rerun; stale loops burn CPU forever.
- Suggested improvement: (1) peer placement tests should pin the PSI inputs (fixture pressure)
  instead of reading the host's; (2) `tasks logs ID` should end its stdout with the
  `job N <phase> (exit X)` line (or document `tasks run`/`tasks list --json` for waiting), and
  health could flag a client polling the same finished job for hours.
- Acceptance test: (1) `cargo test --test peer` passes while `stress-ng --io 4` runs on the
  host; (2) `tasks logs N 2>/dev/null | tail -1` prints `job N passed (exit 0)` for a passed job.
- Workaround requested/authorized: none; I did not touch the stale loops (other agents' work).
