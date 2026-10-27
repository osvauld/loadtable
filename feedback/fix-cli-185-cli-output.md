# Trial feedback: fixing CLI output (list table, --follow, phone-test progress)

- Agent/session: fix-cli (subagent, worktree `task-manager-worktrees/fix-cli`, branch `fix/cli`), 2026-10-17
- Job ID(s): host 168 (offload, red: 2 display tests failed), 172 (offload, worker job 14: test + clippy green),
  175 (host test + clippy green), 184 (offload, worker job 22: green), 185 (final host test + clippy green).
  Isolated smoke instance (own TASK_MANAGER_STATE_DIR, not the shared ledger): jobs 1-3.
- Outcome: no issues observed with the coordinator; one friction point (isolated state path length).
- Redacted command, cwd type, resource class:
  `tasks run --run --who fix-cli -- tasks-offload --worker <lan worker> --who fix-cli --ssh-key <key> -- sh -c 'cargo test --locked -j 8 && cargo clippy ... -D warnings'` (run slot on this host, compile on the worker);
  `tasks run --who fix-cli -- sh -c 'cargo test --offline --locked -j 2 && cargo clippy --offline --locked -j 2 --all-targets -- -D warnings'` (compile slot, live worktree).
- What was exercised: real execution. Offloaded tests ran on the LAN worker; final validation ran on this host's queue.
  The new binary was smoke-tested against an isolated state dir: `run --run --follow` (three lines one second apart arrived
  on the client one second apart, before the job finished), `enqueue` from two owners in one cwd (shared-cwd warning),
  `list --active` (table plus live holders from the real flock files), `list --who`, `list --who --json`,
  `run --device S --run` (named conflict), `approve-command`/`logs` stderr status line. No phone; phone-test was
  covered by unit tests only.
- Expected: offload returns the remote result as the job's exit code; host queue admits once PSI clears.
- Observed:
  - Offload worked first time: ~35 s per run including a 1.3 MB rsync; the red run (168) propagated exit 101 and the
    remote log; the final JSON line names the remote job ID. No timeouts or resubmissions.
  - The isolated smoke job waited ~33 s on memory/I/O PSI caused by other agents' builds before admission; the log said
    why on each poll.
  - `TASK_MANAGER_STATE_DIR` under the session scratchpad failed with `path must be shorter than SUN_LEN` (socket path
    too long); I used a short dir under `/tmp/claude-1000` instead.
- Impact: none on results.
- Suggested improvement: when the state dir makes the socket path too long, say which path and the limit
  (e.g. "socket path STATE/taskd.sock is N bytes; max 107").
- Acceptance test for suggestion: with a 120-character state dir, `tasks list` names the socket path and the limit.
- Workaround requested/authorized (if any; no silent bypass): none. I stopped my own isolated daemon after the smoke test.
