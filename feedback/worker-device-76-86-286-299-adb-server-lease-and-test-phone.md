# Trial feedback: adb server keeps lock leases; test phone on the worker

- Agent/session: worker-device (feature/worker-device, from ace65a9)
- Job ID(s): worker 76, 77 (adb server probe and cleanup), 78 (Rust red),
  79/80 (test/clippy green), 81/82 (descriptor-free adb start and cleanup),
  83/84/85 (NixOS module evaluation; 83 and 84 were fixes to my own script),
  86 (re-run of test and clippy). Laptop 283-299, matching these through
  `tasks run --offload`, plus 286 (Python red), 287/295/297 (Python green), and
  298/299 (final `cargo test`/clippy at `-j 2`).
- Outcome: defect (adb lease leak; fixed on this branch, not deployed) and one
  near-miss caused by my own test design (below).
- Redacted command, cwd type, resource class: `tasks run --offload --who
  worker-device -- tasks-offload --worker WORKER --ssh-key KEY --who
  worker-device [--remote-run] -- CMD` from my own task-manager worktree.
  Local resource is offload; the worker resource is compile or run.
- What was exercised: real worker run jobs that ran the SDK adb (no phone
  attached): `adb version`, `adb devices -l`, `adb kill-server`, plus `ps` and
  `/proc/PID/fd` inspection. Real worker compile jobs ran `cargo test`/clippy.
  A real `nix-instantiate` of the worker's configuration ran with the new
  module (no switch). No phone, no udev or USB access, no install or logcat.

## 1. A job-spawned adb server holds the job's admission lock indefinitely

- Expected: when a job ends, its slot is free.
- Observed: in worker job 76 (`--run`), the first `adb devices` forked
  `adb -L tcp:5037 fork-server server`. It called `setsid` (pgid = sid = its own
  pid, parent the user manager), so the job's process-group cancellation never
  reaches it. It also inherited fd 9 → `/tmp/claude-1000/p2p-run-0.lock`. After
  job 76 passed, `tasks admission` showed `p2p-run-0.lock busy: true, holder:
  null`, and `/proc/locks` showed the flock held by that server. It was freed
  only by `adb kill-server` in job 77. In a `--device` job, the same mechanism
  would hold `p2p-device-SERIAL.lock`, blocking that phone for every later job.
  (Leases are deliberately inheritable, `Lease::inherit`. That is right for
  ordinary children, but a daemonizing helper keeps the lease after the job.)
- Impact: a run slot or device slot is silently lost until someone kills a
  process that is outside the ledger. `holder: null` gives no hint about which
  process holds it.
- Fix on this branch: `--device` jobs start the adb server before the command
  with every descriptor above 2 close-on-exec (`device::without_inherited_fds`,
  unit test `helper_processes_do_not_inherit_lease_descriptors`). Worker job 81
  checked the same mechanism (`close_fds=True`): the server had 0 lock
  descriptors and all run slots were free after the job. Device jobs also get
  `ANDROID_SERIAL`.
- Suggested follow-up: when a slot is busy with no holder note, `tasks
  admission` could name the PIDs that have the lock file open (scan
  `/proc/*/fd`, own uid only).
- Acceptance test: a `--run` job running a synthetic `setsid sleep 60 &`
  daemon that inherits fds. After the job passes, `tasks admission` reports
  that slot's lock as held by that PID (not just `busy`/`holder: null`).

## 2. Near-miss: the fake-worker test shim executes argv on the laptop

- Observed: my first (red) Python tests for the denylist used real commands
  such as `adb -s ZD22257P6G logcat -d`. Before the refusal existed,
  `tests/test_tasks_offload.py`'s fake `~/.local/bin/tasks` ran them **on the
  laptop** (laptop job 286). The personal phone was not connected, so adb sat
  waiting for that device. The test timed out after 120 s and left the
  orphaned `adb ... logcat -d` and fake-tasks processes. I killed both (my
  own). Nothing reached a device, and no logcat output was captured.
- Impact: with the personal phone plugged in, a red-phase test would have
  read its log (or, with an install command, changed it).
- Fix on this branch: device tests now use inert `echo adb ...` argv, and a
  comment explains why.
- Suggested improvement: have the fake `tasks` in that fixture refuse any argv
  whose basename is `adb`/`fastboot`/`gradlew` (fail the test loudly instead of
  executing it).
- Acceptance test: a fixture self-test where `offload('adb', 'devices')` exits
  non-zero with "fake worker refuses device tools" and spawns no process.

## 3. Other observations (no issue)

- Offloading to the worker worked well throughout: sync 1 to 2 s, warm
  `cargo test --locked -j 8` about 11 s, clippy about 16 s. `--offload` started
  immediately while another agent's compile ran on the worker.
- Grepping `ps` for a pattern that appears in your own `sh -c` text also matches
  the job's shell (cosmetic noise in job 81's probe output).

- Workaround requested/authorized: none. The cleanup jobs (77, 82) and the
  killed orphans were my own processes. No other agent's work was touched.
