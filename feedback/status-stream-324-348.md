# Trial feedback: status stream (lan-peer status ALPN, ssh fallback, offload sidecar)

- Agent/session: status-stream (feature/status-stream worktree)
- Job ID(s): local 324 (compile error in a new test), 326 (experiment tests, pass),
  327 (ssh-fallback Python tests, 7 pass), 328 (my mistyped unittest module path, fail),
  329 (sidecar Python tests, 5 pass), 342 (experiment clippy -D warnings + tests, pass),
  347 (full Python suite, 85 pass), 348 -> worker 121 (main crate `cargo test --locked -j 8`
  offloaded, 206 pass / 1 ignored, which was already ignored before).
- Outcome: friction (one blocker for offloading the experiment crate)
- Redacted command, cwd type, resource class: `tasks run --who status-stream -- cargo
  test/clippy --offline --locked -j 2` in `experiments/osvauld-foundation` (compile);
  `tasks run --run -- env PYTHONDONTWRITEBYTECODE=1 python3 -m unittest ...` (run);
  `tasks run --offload -- tasks-offload --worker abe@192.168.1.7 ... -- cargo test --locked -j 8`
  at the worktree root (offload).
- What was exercised: real local compile/test jobs, a real ssh offload to the NixOS worker,
  and loopback iroh tests between separate processes. No real two-machine status stream:
  the worker's lan-peer is not running and its `tasks` has no `status` subcommand yet.
- Expected: experiment crate tests offloadable to the worker as the instructions suggest.
- Observed: the experiment crate cannot build on the worker. Its path dependencies under
  `/home/abe/osvauld2` exist only on the laptop (`ls` on the worker: no such directory),
  and offload syncs only the git-visible checkout. So all experiment builds ran on the
  laptop queue (`-j 2`), cold build plus tests about 6 min (job 342). The main crate
  offload worked first time (sync 2.0 s, run 39.8 s).
- Observed: `tasks run` waited ~34 s on memory PSI for a run-only Python job while an
  unrelated offload was active. That was a correct gate, not a defect.
- Observed: `tasks logs ID` does not contain the client's final `job N passed` line, so a
  wait loop grepping logs for it never ended; I had to stop it by hand.
- Impact: experiment-crate validation loads the laptop. Log-based waiting is confusing.
- Suggested improvement: (1) document in the offload README that crates with path
  dependencies outside the checkout cannot be offloaded (or have tasks-offload detect
  `path = "/abs..."` dependencies outside the toplevel and refuse with a clear message);
  (2) have `tasks logs` show the terminal phase line, or document `tasks list --json`
  as the way to wait.
- Acceptance test: (1) `tasks-offload --dry-run` in `experiments/osvauld-foundation` names
  `/home/abe/osvauld2/identity` as an unsynced external path dependency; (2) `tasks logs N`
  of a finished job ends with `job N passed (exit 0)`.
- Workaround requested/authorized: none. Fell back to the laptop queue as fix-common.md
  allows for infrastructure reasons.
