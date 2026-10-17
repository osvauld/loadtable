# Trial feedback: reliability fixes (busy ledger, client EAGAIN, exit labels, cancel grace)

- Agent/session: fix-reliability (worktree `task-manager-worktrees/fix-reliability`, branch `fix/reliability`)
- Job ID(s): host 170 (worker 16, compile error), 179 (worker 19, red), 180 (worker 20, cancelled by me: hung red test), 208 (worker 25, red), 212 (worker 27, green test + clippy), 214 (host-only final test + clippy)
- Outcome: friction (offload worked; one self-inflicted hang)
- Redacted command, cwd type, resource class: `tasks run --run --who fix-reliability -- tasks-offload --worker <lan-worker> ... -- cargo test --locked -j 8` (run slot on host, compile on worker); final `tasks run --who fix-reliability -- sh -c 'cargo test --offline --locked -j 2 && cargo clippy ...'` on host; live worktree cwd.
- What was exercised: real execution on the host queue and the LAN worker via tasks-offload; cancel of my own job 180 (propagated to worker job 20, both ended `cancelled`).
- Expected: offloaded run returns the remote result and exit code; cancel stops both sides.
- Observed:
  - Offload worked: sync ~1-2 s, remote job ID, exit code and the final JSON line were all present. Remote compile from cold took ~66 s; later runs ~6-30 s.
  - Host job 180 waited ~10 min on memory PSI for a `--run` slot even though the work runs on the worker. Same observation as offload-client-115.
  - My red client test hung (the test server waited for a connection the old client never made). I cancelled 180; host and worker jobs both ended `cancelled` with "owned group stopped". Fixed the test server to time out.
  - Every failing job printed the double exit status this branch fixes (#17): `local command exited: exit status: 101` then `native admitted command failed: exit status: 2`, both on the host and on the worker.
- Impact: ~10 min lost to PSI wait on a run slot; no wrong results.
- Suggested improvement: an offloaded job should not wait for host CPU/memory PSI it does not use (e.g. a light `offload` resource class), keeping the run-slot limit.
- Acceptance test for suggestion: with host memory PSI above the threshold, an offload job is admitted while a local compile job still waits.
- Workaround requested/authorized (if any; no silent bypass): none.
