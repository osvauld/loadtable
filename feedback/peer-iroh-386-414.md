# Trial feedback: daemon-to-daemon iroh link (peer-iroh)

- Agent/session: peer-iroh (branch `feature/peer-iroh`, worktree `task-manager-worktrees/peer-iroh`).
- Job ID(s): 386, 393-399, 400, 402-404, 409-414 (all `--who peer-iroh`, compile class, laptop
  queue, `-j 2`, offline). Final: 413 `cargo test --offline --locked -j 2 --workspace` passed;
  414 `cargo clippy --offline --locked -j 2 --workspace --all-targets -- -D warnings` passed.
- Outcome: one **defect found and fixed** (lock-descriptor leak through an autostarted daemon,
  self-inflicted by my fixtures), otherwise friction only.
- Redacted command, cwd type, resource class: `tasks run --who peer-iroh -- cargo
  check|test|clippy --offline [--locked] -j 2 ...` from my own worktree; compile then run stage.
- What was exercised: real execution through the installed release (admission, cache target
  links, logs, exit codes); the new tests also start their own isolated daemons/workers.

## 1. Leaked fixture daemons held all four shared run slots (defect, fixed in this branch)

- Observed: job 404 waited 13m16s in `waiting_run`. `tasks admission` showed run-0..3 held with
  "no live holder note, not in ledger". `/proc/locks` named dead pids; the descriptors were
  held by live processes from MY earlier test runs (jobs 400/402/403): `tasks daemon`
  processes with fixture state dirs, re-parented to init, and one stuck fixture worker whose
  state dir had been deleted.
- Cause: a test's `tasks run` client, waiting while the test had killed its daemon, autostarted
  a new daemon (`ensure_daemon`). That daemon inherited every descriptor of the client,
  including the admitted job's run-slot lock (deliberately inherited by job processes), and it
  outlived the test. The same happens in production to any `tasks` client run INSIDE a job
  that finds no daemon.
- Action: I killed only my own fixture processes (identified by their fixture
  `TASK_MANAGER_STATE_DIR`); job 404 then started. Fixes: `ensure_daemon` marks inherited
  descriptors above stdio close-on-exec (`close_range(3, ~0, CLOSE_RANGE_CLOEXEC)`); the test
  fixtures do the same for every process they spawn and reap all processes of their state dir
  on drop. Jobs 409-414 left no fixture process behind (checked with `pgrep`).
- Impact: other agents' run-class jobs were blocked during that window (I saw no other run job
  queued, but cannot rule it out).
- Acceptance test: a job whose command runs `tasks list` against a stopped daemon (autostart),
  then exits; afterwards `tasks admission` shows the job's slot free and the autostarted daemon
  holds no `p2p-*.lock` descriptor (`ls -l /proc/PID/fd`).

## 2. Friction

- `cargo test` without `--workspace` in a workspace with a root package tests only the root
  package; the README's commands should say `--workspace` once crates/peer exists (done in
  my docs only for my own commands).
- Real-worker tests: the existing ones are `#[ignore]`d because of PSI gates. The new two-daemon
  tests run in `cargo test` with isolated lock dirs and relaxed reserves (memory 1 GiB, disks,
  run-slot CPU), memory/IO PSI unchanged; they passed in 31-32 s in jobs 402/404/409/413 and
  would queue, not fail, under real memory/IO pressure.
- First runs failed on my own test bugs (job 398/400: log lines also appear inside the logged
  argv, so `contains("started")` matched too early; job 403: a lost update of `cancel_sent`
  between the forward session and the cancel watcher; job 409: fixture needed a claim before
  `finish`). All fixed and rerun.

## 3. No issues observed

Offline resolution added iroh 1.3 / iroh-blobs 0.103.1 to the root lockfile without changing
any existing locked version; the cache-target link for the worktree was placed automatically.
