# Trial feedback: worker package (headless build, NixOS flake/module), resumed after a shutdown

- Agent/session: worker-package (two sessions: the first was cut off by a laptop shutdown at
  20:14 IST; the second resumed it).
- Job ID(s): laptop 476, 478, 486, 495, 496, 499, 500, 501 (first session); 508, 511
  (interrupted by the shutdown); 513, 514, 520, 524, 553 (resumed session). Worker-side ids
  for the offloads: 144, 147-149, 153, 155, 159, 165 (553). 553 (eval only) ran 44m57s,
  almost all of it queued behind the worker compile slot.
- Outcome: friction (two items) and one defect, already known and owned elsewhere (lease leak).
- Redacted commands, cwd type, resource class: own worktree, live cwd.
  `tasks run --who worker-package -- cargo {clippy,test} --offline --locked -j 2 ...` (compile);
  `tasks run --offload --who worker-package -- tasks-offload --worker abe@<worker> --ssh-key <key>
  --who worker-package [--fetch flake.lock] -- {nix flake check --no-build, cargo clippy/test/build --locked -j 6}`
  (offload, then compile on the worker).
- What was exercised: real execution of all of the above. Evaluation only for Nix: no
  `nix build`, no nixos-rebuild, nothing installed.

## 1. Interrupted jobs after a shutdown (friction, observation)

- Expected: after the reboot, the two jobs that were live during the shutdown (508, 511) show
  clearly as interrupted, and resubmitting is safe.
- Observed: `tasks logs 508` said "job 508 interrupted (exit -) ... no worker process at reply
  time (... this does not mean the job did not run ...)". That was clear. Resubmitting as 513/514
  worked. Nothing tells you whether the remote half of 511 (an offload) had ever started on
  the worker. I had to reason that from the worker's list.
- Impact: small. An offloaded job interrupted on the controller makes you look on the
  worker before you retry.
- Suggested improvement: for an interrupted offload job, `tasks logs` names the remote job id
  and its last known remote phase from the sidecar (or "never submitted remotely").
- Acceptance test: kill the controller daemon while an offload job is in `running`; after
  restart, `tasks logs ID` prints `remote job N: <phase>` or `not submitted`.

## 2. Job-spawned sccache server holds the worker's compile slot (defect; owned by fix/lease-leak)

- Expected: once a compile job finishes, the worker's `p2p-build.lock` is free.
- Observed (worker, read-only inspection): `tasks admission` showed `p2p-build.lock`
  `busy: true, holder: null`. The only process holding the lock fd was an `sccache` server
  with `TASKS_JOB_ID=158` (another agent's finished job) in its environment and fd 6 ->
  `/tmp/claude-1000/p2p-build.lock`. My worker job 159 waited 41m53s for the slot (run 7m19s).
  The same happened with each later job: job 161's rustc spawned new servers. My own jobs
  155/159 spawned such servers too, through the RUSTC_WRAPPER they must not override.
- Impact: the worker's single compile slot sits idle for roughly the sccache idle timeout
  (10 min) after every job, and the queue serializes on that.
- Workaround: none taken. I did not kill or stop the server, and I did not override
  RUSTC_WRAPPER. I waited.
- Declaration-side fix in this branch: `services.tasks-worker.sccache.server` (default on)
  runs `tasks-worker-sccache.service` outside any job (SCCACHE_IDLE_TIMEOUT=0, same dir and
  size), ordered before the daemon. Evaluated only; the real worker still uses its user unit.
- Acceptance test: on a worker with the module, run two compile jobs back to back. The second
  is admitted within seconds of the first ending, and `pgrep sccache` shows one server whose
  environment has no TASKS_JOB_ID.

## 3. Editing during a live compile, and the laptop vs worker load (friction, my own error)

- I edited `src/bin/tasks/main.rs` while job 513 was still running. Its test binary turned out
  to predate the edit (8 peer tests, not 9), so the result counted only for the old code and I
  reran everything. Two timing-sensitive peer tests (`cancel_on_either_side_propagates`,
  `lost_link_is_uncertain_then_reattaches_and_restarts_reattach`) failed there under laptop
  I/O pressure. The same tests passed on the worker (jobs 155, 159) and earlier on the laptop
  (496). This is a hypothesis: load-induced timeouts, not a code defect.
- Suggested improvement: `tasks list` could flag a live compile job whose cwd has files
  modified after the job started ("checkout changed during build").
- Acceptance test: start a compile job, touch a tracked source file, and `tasks list` marks the
  job `cwd changed`.

## What worked (no issues observed)

- Offload with `--fetch flake.lock` returned exactly the lock file that `nix flake check`
  wrote on the worker (job 514 -> worker 153).
- `tasks run` exit status and the final summaries were accurate for every job above.
- Laptop pressure gates queued work as documented (memory/IO PSI). No bypass was needed.

## Full Nix build cost estimate (not run)

Measured on the worker (12 threads, -j 6, sccache possibly warm): `cargo build --release
--bin tasks` took 2m34s headless and 1m57s default (worker job 159). `nix build .#tasks-worker
--max-jobs 1 --cores 4` has no sccache and fetches about 300 crates into the store first.
Expect roughly 5-10 minutes of CPU-bound compile, a few hundred MB of downloads, and about
1-2 GB of build memory at 4 codegen jobs. This is an estimate, not a measurement. It runs inside
nix-daemon, outside tasks admission.
