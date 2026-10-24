# Trial feedback: SSH offload v0 client tests through shared admission

- Agent/session: offload-client (worktree osvauld-foundation, unborn HEAD)
- Job ID(s): 114 (expected red), 115 (green)
- Outcome: no issues observed with the task manager; one friction note (PSI wait)
- Redacted command, cwd type, resource class: `tasks run --who offload-client --run -- env PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -p 'test_*.py' -v`; live worktree root; run slot.
- What was exercised (real execution vs model/read-only observation): real run-only execution of stdlib Python tests through installed admission. The offload tests use a fake worker (ssh shim, temp HOME, fake `tasks`), real local rsync. No real remote host, Cargo build or deployment.
- Expected: red run before `scripts/tasks_offload.py` existed, green after.
- Observed: job 114 sat in `waiting_run` for a long time while `tasks admission` reported `CPU PSI some avg10 above 40%` (other agents' compile jobs 112/113 active). My 600 s client timeout expired first; I queried the job with `tasks list`/`tasks logs 114` instead of resubmitting. 114: 48 run, 15 failures (module missing), 33 ok. 115: 51 passed, 0 failed/skipped, ~54 s.
- Observation for offload design: `tasks run` prints the job log only at the terminal phase (main.rs), so "live streaming" over ssh shows phase transitions live and the log at the end. Host `tasks cancel` sends SIGTERM to the process group then SIGKILL ~300 ms later (worker.rs terminate_group), so the offload client must issue the remote cancel from its signal handler in a detached session; a full cancel-and-wait cannot fit inside that grace.
- Impact: a run-only job that needs negligible CPU still waits on host-wide CPU PSI; slower iteration. I iterated on a scratchpad copy outside the checkout with direct `python3 -m unittest` (light stdlib tests) while 114 waited, and kept the checkout unchanged until 114 finished so the red result stayed honest. Official red/green results are only 114/115.
- Suggested improvement: (1) `tasks run --follow` to stream job log text while running, so remote/offload callers see progress; (2) expose the job's cancel grace or allow a longer configurable SIGTERM grace for clients that must perform remote cleanup.
- Acceptance test for suggestion: (1) a run-only job printing a line every second shows lines on the client before it terminates; (2) a job trapping SIGTERM and sleeping 2 s before exit completes its handler when grace is set to 5 s, and is SIGKILLed at 300 ms by default.
- Workaround requested/authorized (if any; no silent bypass): none; did not lower gates or resubmit.
