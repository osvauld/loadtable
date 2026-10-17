# Trial feedback: offload admission class; offloading the test loop to the worker

- Agent/session: offload-slot (feature/offload-slot, lighter host slot for `tasks-offload`)
- Job ID(s): host 266 (red, cancelled by me), 269, 270, 271/272 (my own argv mistake),
  273, 274 (worker offloads), 275, 276, 277 (local); worker 65 (red), 67, 68, 69/70, 71, 72.
- Outcome: no issues observed with tasks/offload. One self-inflicted mistake (271/272).
- Redacted command, cwd type, resource class:
  `tasks run --run --who offload-slot -- tasks-offload --worker WORKER --who offload-slot
  --ssh-key KEY -- cargo test --locked -j 8` (host run slot, remote compile slot);
  `tasks run --who offload-slot -- cargo test|clippy --offline --locked -j 2` (compile);
  `tasks run --run --who offload-slot -- env PYTHONDONTWRITEBYTECODE=1 python3 -m unittest
  discover -s tests -p 'test_*.py'` (run). Cwd: my own worktree.
- What was exercised: real execution on this host and on the NixOS worker. The new
  `--offload` class itself was exercised only by unit tests. The installed release does not
  have it yet, so these offloads still used `--run`.
- Expected: the red run fails its new tests; green runs pass; offload sync is quick.
- Observed:
  - First offload of this new worktree (worker 65): cold remote build, about 2m19s wait
    plus build. Later offloads synced in about 1.2 s and ran in about 3–9 s.
  - Red run: 9 new tests failed as expected. The pool test then hung, because the skeleton
    mapped Offload to the run locks the test itself held. I cancelled host 266. That
    cancelled worker 65 as documented: SIGTERM, then exit 130.
  - `tasks run` streams only phase lines live, so a long remote test run looks idle locally.
    `ssh WORKER tasks logs 65` showed the hang.
  - Host 271/272: I ran a zsh loop with the command in an unquoted `$c`, and zsh did not
    word-split it. The worker got the single argv `'cargo test --locked -j 8'` and failed
    with exit 2. This was my error, not a tasks defect. Reran as 273/274.
  - Laptop final: 275 test passed, 276 clippy passed, 277 Python ran 66 tests, OK, 1 skipped
    (the iroh loopback class: no local `lan-peer` binary built).
- Impact: none beyond the cold first remote build. Offloading the iteration loop kept laptop
  compile load to the final validation only.
- Suggested improvement: the offload class in this branch, i.e. `tasks run --offload`, so a
  waiting offload neither holds a run slot nor waits on CPU PSI while the laptop compiles.
- Acceptance test for suggestion: `admission::offload_tests::*` and
  `local_command::offload_tests::*`. After deployment, start an offload while all four run
  slots and the compile slot are held and CPU PSI is above 40%. It must start, and
  `tasks admission` must show it on `p2p-offload-0` with `offload_pressure_blocker: null`.
- Workaround requested/authorized (if any; no silent bypass): none.
