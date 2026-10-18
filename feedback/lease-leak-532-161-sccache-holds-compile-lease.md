# Trial feedback: detached sccache server keeps a finished job's compile lease

- Agent/session: lease-leak (fix/lease-leak worktree, Claude Code subagent)
- Job ID(s): laptop offload clients 532, 565, 569, 572 = worker jobs 161, 166,
  170, 173 (all `cargo test`/`clippy` of fix/lease-leak).
- Outcome: defect (reproduced live on the worker while testing the fix)
- Redacted command, cwd type, resource class: `tasks run --offload --who
  lease-leak -- tasks-offload --worker WORKER --who lease-leak --ssh-key KEY --
  cargo test --locked -j 6`; own git worktree; laptop offload slot, worker
  compile slot.
- What was exercised (real execution vs model/read-only observation): real
  execution of my own offload jobs; read-only `tasks list`/`tasks status` and a
  read-only `/proc/*/fdinfo` scan on the worker. No worker/daemon/sccache
  process was signalled, restarted or reconfigured.
- Expected: worker job 161 is admitted to the compile slot once the jobs ahead
  of it finish.
- Observed: worker 161 sat in `waiting_build` for 20+ minutes behind #158/#159
  with `waiting for compile slot held by an untracked holder` and `tasks
  admission` showing `compile holds (no live holder note) pid ?`. The
  read-only fdinfo scan found the holder: `sccache-0.15.0` pid 86869, fd 6 ->
  `p2p-build.lock` with a `FLOCK` lock line in its fdinfo, environment
  `TASKS_JOB_ID=157`, i.e. the sccache server autostarted by worker job #157
  that outlived it (same pattern as #154 -> #155..#158 in the original report).
  Each later build restarts the server and leaks again, so the queue drains at
  about one job per sccache idle timeout (600 s).
- Impact: the worker compile slot is effectively serialized at ~10 min per job
  after any sccache restart; other agents see an unexplained "untracked holder".
- Suggested improvement: implemented on fix/lease-leak: (1) before running a
  job command whose RUSTC_WRAPPER is sccache, the worker runs `sccache
  --start-server` (then `--show-stats` if one already runs) from a child with
  every descriptor above stderr close-on-exec and its own session, so the
  server never holds a lease; (2) when a slot is busy with no live holder note,
  `tasks status`/`tasks admission`/the snapshot/the waiting job's log name the
  real holder (pid, command, originating TASKS_JOB_ID, and "which has ended").
  Locks are never stolen or holders killed.
- Acceptance test for suggestion: `cargo test --locked -- --ignored --test
  lease_leak` on a host with headroom: a job that daemonizes a child keeps the
  slot and the next job's `waiting_for` names `pid N (sleep 300) from job #A,
  which has ended`; with a synthetic sccache-like RUSTC_WRAPPER the next build is
  admitted while the server still runs. Unit tests
  `daemonized_child_that_outlives_its_job_is_named_as_the_holder`,
  `server_autostarted_by_a_job_keeps_its_lease_after_the_job` (control) and
  `server_started_outside_the_job_never_holds_the_lease`.
- Second finding (my own test, fixed before commit): worker job 166 ran the
  new ignored E2E tests; they failed on the worker's memory gate because the
  isolated test daemon did not get the worker's 1 GiB
  `TASKS_MIN_AVAILABLE_MEMORY_MIB` (the job env is an allowlist; the default
  8 GiB exceeds the worker's 7.6 GiB RAM). The fixture's Drop killed only the
  daemon, so the detached test `tasks worker`/`__stage`/`__admit` processes
  survived and, having inherited job 166's REAL compile lease, blocked the
  worker queue (#167..#170) as one more "untracked holder". I stopped those
  six processes of my own isolated instance (identified by its unique
  `TASK_MANAGER_STATE_DIR=/tmp/tasks-lease-*`); nothing else was touched. The
  fixture now spawns its daemon with descriptors close-on-exec and kills every
  process of its instance on drop; the E2E reruns pass with
  `env TASKS_MIN_AVAILABLE_MEMORY_MIB=1024` (the worker's own recorded
  threshold, not a lowered one). Same caution applies to tests/status.rs's
  ignored E2E fixture (unchanged here).
- Other observations: a full run (job 161) had one failure in
  `tests/peer.rs::source_sync_sends_only_missing_blobs_prunes_deletions_and_honours_path_rules`
  (panicked at peer.rs:599 after 215 s); it passed on rerun in 166 and 173.
  Looks timing/load related; not investigated. The bounded job log
  truncated the clippy output head of job 565, so I grep-filtered later runs.
  A `tasks cancel` of my own still-queued worker job was refused by this
  session's permission policy, so I waited instead.
- Workaround requested/authorized (if any; no silent bypass): none. I waited in
  the queue; stopping the leaked server (`sccache --stop-server` on the worker)
  would free the slot at once but is the user's call, not taken.

Do not include credentials, full logs, environment dumps or private source.
