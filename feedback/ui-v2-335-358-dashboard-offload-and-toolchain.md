# Trial feedback: ui-v2 dashboard work, worker offload and toolchain drift

- Agent/session: ui-v2 (egui dashboard redesign, branch feature/ui-v2)
- Job ID(s): laptop 335, 336, 338, 339, 341, 345, 349, 352, 354, 358;
  worker (via tasks-offload) 114, 115, 117, 118, 123
- Outcome: friction (one toolchain divergence); otherwise no issues observed
- Redacted command, cwd type, resource class:
  - `tasks run --offload --who ui-v2 -- tasks-offload --worker WORKER --who ui-v2 --ssh-key KEY -- cargo test|clippy --locked -j 8`
    from my own worktree (offload slot on the laptop, compile slot on the worker)
  - `tasks run --who ui-v2 -- sh -c 'cargo clippy --offline --locked -j 2 --all-targets -- -D warnings && TASKS_UI_SCREENSHOT_DIR=SCRATCH cargo test --offline --locked -j 2'`
    (compile slot on the laptop)
- What was exercised: real execution. Worker offload for the edit/test loop
  (5 runs: red test runs, clippy failures, green), laptop queue for the final
  test + clippy and for the headless screenshot test (files must land locally).
- Expected: the worker's clippy and the laptop's clippy agree on `-D warnings`.
- Observed:
  - Worker offload: sync 1.2–1.7 s for ~2 MB / 149 files, warm incremental
    builds 3–40 s; the final JSON line (`remote_job_id`, `durations`,
    `remote_phase`) was all I needed to find the worker log. Smooth.
  - Worker clippy (job 123, clippy for rust 1.99) passed; laptop clippy
    (job 352, clippy for rust 1.92) failed on `clippy::nonminimal_bool`, which
    the newer clippy does not report. Conversely the worker reported lints the
    laptop does not have (`manual_is_multiple_of`, `chunks_exact_to_as_chunks`,
    job 115). Fixed both ways; final 354 (laptop) and 358/123 (worker) green.
  - Laptop job 352 waited 2m23s for the compile slot behind another agent's
    clippy (expected FIFO, shown clearly by `tasks list --active`).
- Impact: "green on the worker" is not "green on the laptop"; an agent that
  only offloads could hand in code that fails the laptop's clippy.
- Suggested improvement: print the remote `rustc -V`/`cargo clippy -V` in the
  tasks-offload summary JSON next to the local ones, and warn when they differ
  (or pin the worker toolchain to the laptop's).
- Acceptance test for suggestion: run `tasks-offload ... -- cargo clippy` with
  differing toolchains; the final JSON has `remote_toolchain` and
  `local_toolchain` fields and stderr carries one "toolchains differ" warning.
- Workaround requested/authorized: none; ran the final validation on both.
