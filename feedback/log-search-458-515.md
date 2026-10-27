# Trial feedback: log-search branch builds/tests, and a job lost to a host shutdown

- Agent/session: log-search (feature/log-search worktree; two sessions, the first cut off by a
  laptop shutdown at 2026-10-17 20:14 IST, the second resumed it)
- Job ID(s): 458, 464, 465, 468, 471, 472, 477, 482, 483, 484, 493, 498 (first session);
  515, 517, 518 (local) and 522, 536 (offloaded to the NixOS worker; worker jobs 157, 163)
  in the second session
- Outcome: friction (no defect in the task manager found)
- Redacted command, cwd type, resource class: `tasks run --who log-search -- cargo
  check|clippy|test --offline --locked -j 2 ...` from the agent's own live worktree; compile.
- What was exercised (real execution): target-scoped `cargo test --lib --test peer -- FILTER`
  iterations, clippy `--all-targets -D warnings`, full `cargo test --workspace`, background
  `tasks run` with the client waiting, and `tasks list --who` / `tasks logs` after a reboot.
- Expected: after the reboot, the job that was running when the host went down is clearly
  marked and its partial log is readable.
- Observed:
  - Job 498 shows `interrupted` with `EXIT 0` in `tasks list --who log-search`; its retained log
    was readable and showed that two hermetic peer tests had already failed before the
    shutdown, because the fixture daemons' jobs sat in `waiting_run` behind host memory PSI
    (`memory PSI some avg10 above 1%`, `memory PSI full avg10 above 0.5%`) until the tests'
    wait budget ran out. The same command resubmitted as job 515 passed (3 peer tests, 1 lib
    test).
  - Offloaded job 522 (`sh -c 'cargo clippy ...; cargo test --workspace --no-fail-fast'`):
    the local `tasks logs 522` and the client output kept only the last ~460 lines, so the
    clippy result and the one failing target's panic were cut off; a second, narrowed offload
    (536) was needed just to see them. It also shows `passed` for the wrapper although the
    inner `cargo test` exited 101 (expected for `sh -c` whose last command is `echo`, but easy
    to misread).
  - On the worker, `-p task-manager-peer --test sync_checkout`
    `sync_never_follows_symlinks_and_refuses_excluded_paths` fails (`victim.txt 600` vs
    `644`): the test writes the file with `std::fs::write` and so depends on the umask; it
    passes on the laptop (job 484). Pre-existing test, not touched by this branch.
  - Admission waits dominated: e.g. job 477 waited 17m40s for a 2m27s run, job 498 23m01s.
- Impact: `EXIT 0` on an interrupted job reads like success at a glance; an agent that only
  checks the exit column could wrongly treat 498 as passed. The hermetic peer fixtures keep the
  host's PSI gates, so on a loaded host they fail by timeout rather than by assertion.
- Suggested improvement: (1) show `-` (or no exit) for `interrupted` jobs in the table and in
  `tasks run`'s final line, as for unfinished ones; (2) when an offloaded log is truncated,
  keep the head (first ~50 lines) as well as the tail and say how many lines were dropped;
  (3) make the sync_checkout test set the victim file's mode explicitly.
- Acceptance test for suggestion: a job whose worker dies mid-run lists as
  `interrupted  -` in `tasks list --who X`, and `tasks logs ID` stderr says
  "interrupted (no exit status)"; a 2,000-line offloaded log shows its first lines, a
  "... N lines dropped ..." marker and its tail; sync_checkout passes under `umask 077`.
- Workaround requested/authorized (if any; no silent bypass): none; the interrupted job was
  resubmitted once after the reboot (it was not live), no gate was lowered.
