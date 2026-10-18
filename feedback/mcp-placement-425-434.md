# Trial feedback: MCP placement over the peer link (jobs 425–434)

- Agent/session: mcp-placement (worktree /home/abe/task-manager-worktrees/mcp-placement,
  branch feature/mcp-placement from 792c73b)
- Job ID(s): 425 (cargo check --workspace --all-targets, passed), 429 and 434 (clippy
  --workspace --all-targets -D warnings, passed), 430 (cargo test --workspace, failed: one
  wrong expectation in my new apply_jobs test; cargo stopped at the lib binary), 431 (cargo
  test --workspace --no-fail-fast, failed: 3 tests in tests/peer.rs, see below), 432 (cargo
  test --workspace --no-fail-fast, passed), 433 (tests/peer.rs mcp_anywhere_goes with
  --nocapture for a sample exchange, passed). All on the laptop, `-j 2`, `--offline --locked`.
- Outcome: friction (queue wait), one defect of mine found by the suite, one unexplained
  failure not reproduced.
- Redacted command, cwd type, resource class: `tasks run --who mcp-placement -- cargo
  check|clippy|test --offline --locked -j 2 --workspace ...`, own worktree, compile class.
- What was exercised: real execution through the shared queue; hermetic two-daemon fixtures
  (isolated HOME/state/lock dirs on loopback) inside job 431–433.
- Expected: check to start soon after submission.
- Observed:
  - Job 425 waited about 21 minutes for the compile slot behind another agent's Gradle job
    (424) that itself waited behind 423. `tasks list --active` explained the wait clearly
    (holder and waiter per slot). No issue with admission itself.
  - Job 431: the new MCP anywhere/cluster_status tests failed because the peer runtime keeps
    peer snapshots without `history` rings and `Snapshot` required that field (my bug; fixed
    by `#[serde(default)]` on both history fields). In the same run the existing
    `denylisted_devices_..._unreachable_peers_are_clear_errors` saw its forwarded job stay
    `approved` until the 180 s client limit (exit None, expected 255). It passed in 432 and I
    did not find the cause; recorded as an unexplained, non-reproduced failure.
  - After every run `pgrep -af "tasks daemon"` showed no fixture daemon left (only three
    pre-existing daemons of other checkouts/releases).
- Impact: about 20 minutes of idle waiting; no work lost.
- Suggested improvement: `tasks run` could print the expected wait from the snapshot ETA of
  the holder and of the jobs queued ahead (the same estimate placement now computes) on its
  `WaitingBuild` line.
- Acceptance test for suggestion: with a compile holder whose estimate is 300 s, a new
  `tasks run` prints "expected wait ~5m (holder #N)" on its waiting line.
- Workaround requested/authorized: none; I waited.
