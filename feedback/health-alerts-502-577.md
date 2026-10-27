# Trial feedback: health-alerts branch builds/tests, local then offloaded to the NixOS worker

- Agent/session: health-alerts (resumed after the laptop shut down at 2026-10-17 20:13 IST)
- Job ID(s): 502 (previous session: `cargo test --workspace --no-run`, passed), 516 (local
  `cargo check --all-targets`, failed on a real compile error), 519/154, 521/156, 528 (own,
  cancelled while queued: stale source), 533/162, 566/167, 570/171, 574/175, 576/177,
  577/178 (final clippy)
  (local/worker ids; all `tasks run --offload -- tasks-offload --worker ... -- cargo ...`)
- Outcome: friction (queueing, flaky peer tests on the worker, worker umask), no blocker
- Redacted command, cwd type, resource class: own worktree; `cargo clippy|test --locked -j 6
  [--workspace --no-fail-fast]` through tasks-offload, offload class; one compile job local.
- What was exercised: real execution on the laptop (516) and the worker (the rest).

## Observations

1. **Worker queueing is invisible from the laptop.** Job 521 (remote 156) sat in
   `WaitingBuild` on the worker for about 20 minutes, and 533 (remote 162) for a similar
   time, behind other agents' offloads. The laptop only shows `running` with `…` for
   WAIT/RUN, and the installed release has no `tasks place` to see the worker's queue.
   Impact: no way to tell queueing from a hang without ssh (not allowed in this brief).
2. **Worker umask makes a peer-crate test fail.** `crates/peer/tests/sync_checkout.rs:280`
   (`sync_never_follows_symlinks_and_refuses_excluded_paths`) expects `644` but the worker
   creates `600` (job 566/167). The file is unchanged on this branch, so it is environmental.
3. **tests/peer.rs is flaky under worker load.** Job 570/171: `cancel_on_either_side_propagates`
   and `lost_link_is_uncertain_...` failed (a job stuck in `Approved`, a client losing its
   daemon across a restart). In the rerun (574/175) both passed, and
   `source_sync_sends_only_missing_blobs...` failed at peer.rs:599 instead. lease-leak's
   feedback reports the same peer.rs:599 failure and a pass on rerun.
4. **The worker's sccache server runs inside a job.** The sample job 576/177 (isolated test
   daemon) found sccache pid 352816 with `TASKS_JOB_ID=169` in its environment: the
   node-setup-policy incident (an inherited lease), detected live by the new
   `compiler-cache:NODE:sccache` check.
5. Cancelling my own queued offload job (528) worked cleanly; the remote job was cancelled too.

## Suggested improvements and acceptance tests

- Show the remote phase and wait reason for offload jobs on the laptop (e.g.
  `remote #156 WaitingBuild: compile slot held by #154`). Acceptance: while a remote job
  waits, `tasks list --active` on the laptop shows its remote phase and blocker.
- Make the sync_checkout mode assertion independent of umask (set the umask in the fixture,
  or compare against `0o666 & !umask`). Acceptance: the test passes under `umask 077`.
- Find out why tests/peer.rs jobs stick in `Approved` under load: suspect a worker-start
  race (StartWorker arriving while the previous worker exits). Acceptance: 10 consecutive
  `cargo test --test peer` runs pass on the loaded worker.
- Deploy the declared sccache user service (worker-package), then check that
  `tasks doctor` on the worker shows no `compiler-cache` alert.

Workaround requested/authorized: none. Lead-directed switch from laptop to worker for heavy
jobs (message mid-task); no locks were bypassed.
