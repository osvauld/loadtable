# Trial feedback: restart survival (auto-resume, interrupted exit, offload reconcile, head+tail logs, restart I/O)

- Agent/session: restart-survival (fix/restart-survival worktree, 2026-10-18)
- Job ID(s): laptop offloads 592 and 606 (cancelled by me while queued with a stale sync;
  worker jobs 187/189 cancelled with them), 607 (worker 190: clippy found one lint, tests
  ran), 628 (worker 196: clippy clean, full workspace tests), 632 (worker 198: clippy clean,
  lib + restart tests). Read-only evidence: laptop jobs 498, 507, 510-512; worker job 184.
- Outcome: defects fixed in source (not deployed); friction noted below.
- Redacted command, cwd type, resource class: `tasks run --offload --who restart-survival --
  tasks-offload --worker USER@WORKER --who restart-survival --ssh-key KEY -- sh -c 'cargo
  clippy ... ; cargo test --locked -j 6 --workspace --no-fail-fast'`; own git worktree; offload.
- What was exercised: real offload execution on the NixOS worker; read-only inspection of
  the laptop ledger (`tasks list --json`), `/proc/PID/io` and `/proc/PID/task/*/io` of the
  live daemon/worker, the worker's `tasks list`. Nothing installed, restarted or killed.
- Expected: after a reboot the queue runs again, finished results are kept, interrupted
  work reads as interrupted, and an offload's remote result can be found.
- Observed (evidence):
  - Job 510 (approved 20:11:38) started only at 20:58:36, after a manual start-worker;
    the daemon came back at 20:42:51 and had no record that execution was ever enabled.
  - Job 498 is `interrupted` with stages `[build exit 0]`: every exit display used the
    last stage's code, so it showed `EXIT 0`.
  - Job 507 (offload) `interrupted`; its sidecar still held the remote job id, but nothing
    read it, and the detail did not name the worker job.
  - Offload logs: the worker's `tasks run` without `--follow` prints only the last 32 KiB
    of the job log at the end; that is all the laptop job log got.
  - Restart I/O: `/proc/PID/io` of a process includes every child it has reaped. The live
    daemon (pid 2888, 10 h) showed write_bytes 22.7 GB while its own threads summed to
    about 13 MB; it reaps the workers it starts, and a worker reaps its stage supervisors
    (and through them cargo/rustc/gradle). A fresh worker's own threads wrote 0.8 MB.
    So the 4.9 GB attributed to `tasks worker` was its jobs' I/O, counted when it reaped
    them, not ledger writes. (Hypothesis, not measured: mostly cargo output on the ext4
    build cache, which is on the same NVMe as the btrfs /home.)
  - Worker job 184 (the 61 min compile-slot hold): not a restart orphan. The worker daemon
    restarted at 06:50:09 IST, 6.5 min before 184 was submitted (06:56:39). 184 was claimed,
    tracked (`building`, holder note job-184) and ran Gradle. Cargo finished in 5m58s, then
    `mergeDebugResources FAILED` and `compileDebugKotlin`: `OutOfMemoryError: Java heap
    space`, and Gradle never exited. The cancel stopped the owned group cleanly.
  - Reading results: the installed client's 32 KiB tail cut the clippy error out of job
    607's local log. I read it from the worker's job.log (the item-4 defect, hit live).
- Impact: queue stalled until a person noticed; an interrupted job could be read as a
  pass; a passed remote job looked lost; clippy results/panics past the last 460 lines
  were gone; the I/O investigation pointed at the wrong process.
- Suggested improvement: deploy fix/restart-survival on both machines (user step), then
  check the acceptance tests below on a real reboot. Restart daemons only with
  `tasks drain && systemctl --user restart tasks-daemon`. Separately: a progress or idle
  watchdog for jobs that are live but stuck (like 184) is not in this change.
- Acceptance test for suggestion:
  1. `tasks start-worker` (one line), approve a job, reboot before it runs: after the
     reboot any `tasks` command starts the daemon and the job runs without start-worker.
     `tasks stop-worker` then reboot: it stays approved until start-worker.
  2. `tasks list` / `tasks logs` / MCP `job` on an interrupted job show exit `-`, never 0.
  3. Reboot the laptop during an offload whose remote job passes: within ~2 min of the
     daemon restart, `tasks logs ID` ends with "remote job N on WORKER passed (exit 0)
     [terminal]" and the remote log; the local phase stays `interrupted`.
  4. An offload printing >4 MiB of output keeps its first lines, one marker and its last
     2 MiB in the laptop job log.
  5. Restart with `tasks drain` while agents keep calling `tasks run`: no job is
     admitted between "drained" and the daemon start, and the new daemon resumes them.
  6. A slot holder in the group of a job the ledger ended over 60 s ago is stopped at
     daemon start ("ORPHAN of job N" in taskd.log); one not matching a job is named
     "UNTRACKED" and left running.
  7. `worker.log` lines "startup done ... own threads wrote N bytes" and "exiting; own
     threads wrote N bytes" stay in the MB range across a restart with a backlog.
- Workaround requested/authorized: none. Friction: the worker's single compile slot
  queued my test run behind three Android builds for 13 min; I cancelled my own stale job
  rather than hold a second submission.

## Addendum: merge into feature/osvauld-foundation and install scripts (2026-10-18)

- Job IDs: laptop 637 (worker 200: matrix after the merge; every target failed to build
  on log-search's new `Recipe.broad_test` field in my test fixtures), laptop job for worker
  201 (matrix plus `nix flake check` after fix 81a2108).
- Observed: the merge conflicts were all additive (`reroute` and `remote_result` side by
  side), but a field added on another branch broke struct literals in tests that
  `git merge-tree` cannot see. A plain `cargo check --all-targets` right after a merge
  catches this in minutes; the full matrix takes much longer behind the single compile slot.
- Install path: neither installed release (laptop 7df4fe15, worker plain file) has
  `tasks drain`, so the first upgrade still has to wait for "no live jobs" and has the
  admission window that drain removes. Every later upgrade can use drain.
- Laptop: no systemd user unit, so after a reboot nothing runs until the first `tasks`
  command starts the daemon. The script prints this and proposes a unit; it does not apply it.
- Acceptance: after both installs, `tasks --usage-probe` mentions `drain [--timeout`, and
  `tasks doctor` is green on the worker with LIBCLANG_PATH in the daemon environment.
