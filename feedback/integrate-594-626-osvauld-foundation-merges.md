# Trial feedback: integrating six branches into feature/osvauld-foundation

- Agent/session: integrate (Claude Code subagent; worktree osvauld-foundation)
- Job ID(s): laptop compile checks 594, 598, 600, 604, 608, 612, 615, 616, 617, 618, 619,
  620; laptop offload clients 621 (worker job 191: clippy + full tests, default and headless)
  and 626 (worker job 195: two single-test reruns + `nix flake check --no-build`).
- Outcome: friction (three items); no defect in the task manager itself. One flaky unit test
  of fix/lease-leak (item 3).
- Redacted command, cwd type, resource class: own worktree, live cwd.
  `tasks run --who integrate -- cargo check --offline --locked -j 2 --all-targets` (compile);
  `tasks run --offload --who integrate -- tasks-offload --worker abe@WORKER --who integrate
  --ssh-key KEY [--fetch REL]... -- sh -c '<multi-line script>'` (offload; compile on the worker).
- What was exercised: real execution of all of the above. Nix: evaluation only (`--no-build`).

## 1. An offload job waiting for a local offload slot says nothing (friction)

- Expected: `tasks list` / `tasks logs 621` say why the job has not started.
- Observed: 621 sat in `approved` for about 13 minutes while all four local offload slots were
  held by other agents' long offloads (586, 589, 601, 607). `tasks logs 621` printed only
  "job 621 approved ... worker process running" and "No execution log yet". The reason was only
  visible by reading `tasks list --active` and `tasks admission` and counting the slots.
- Impact: small; looks like a stuck job until you check the slot table.
- Suggested improvement: for an approved job held back by the offload budget, show
  `waiting for an offload slot (4/4 held by #586, #589, #601, #607)` in `tasks list` WAIT
  column and `tasks logs`.
- Acceptance test: hold all offload slots in an isolated state dir, submit an `--offload` job,
  `tasks logs ID` names the budget and the holders.

## 2. Failed offload runs keep only the tail and fetch nothing (friction)

- Expected: the full clippy/test logs of a failed remote run can be retrieved.
- Observed: `tasks-offload` prints "remote job exited 1; skipping --fetch", so the full logs I
  wrote to `target/integrate/*.log` on the worker stayed there; the local log keeps about the
  last 460 lines. I worked around it by having the remote script print a bounded summary
  (test-result lines, FAILED/ignored lines, panics with context) after the runs and then
  `exit` the combined cargo status. This worked, but every agent has to write that script.
- Suggested improvement: `--fetch-always` (or `--fetch-on-failure REL`) for logs, or keep
  head + tail of the remote log (already in the node-setup-policy table).
- Acceptance test: a remote command that writes `out.log` and exits 1 with
  `--fetch-always out.log` leaves `out.log` in the fetch destination and still exits 1.

## 3. Multi-line `sh -c` argv breaks `tasks list` rows; a flaky lease-leak unit test (observation)

- The worker's `tasks list --active` printed my script's embedded newlines verbatim, so the
  rows after job 191 were split ("L=target/integrate", "summ() …" on their own lines).
  Suggestion: escape control characters in the table (display only, like the env abbreviation).
  Acceptance test: a job with `sh -c $'a\nb'` shows as one row with `a\nb`.
- `admission::holders::tests::daemonized_child_that_outlives_its_job_is_named_as_the_holder`
  failed once in the headless lib run on the worker (job 621 / worker 191: "the escaped daemon
  still holds the lease", i.e. the lock was already free), passed in the default-feature run of
  the same suite in the same job, and passed rerun alone (626 / 195). Hypothesis: a timing race
  under load in the test's setup, not a feature difference; not investigated. When it fails
  before its cleanup, the test's `sleep 120` children are not killed explicitly; a read-only
  `pgrep` on the worker afterwards found none left.
- Known, not chased: `crates/peer` `sync_never_follows_symlinks_and_refuses_excluded_paths`
  (mode 600 vs 644 on the worker) and `tests/peer.rs` `cancel_on_either_side_propagates`
  (failed in 191, passed rerun alone in 195).

## What worked (no issues observed)

- Twelve local `cargo check` jobs: exit status and final summaries were accurate, queueing
  behind other agents' compile jobs was visible in `tasks list`.
- The worker compile queue (191 waited behind 186, 188, 190) was visible read-only with
  `ssh WORKER tasks list --active`. Locally the job only showed `running`, as reported before
  in feedback/reroute-523-582-worker-offload-queue.md.
- `--fetch flake.lock` returned the worker's flake.lock (unchanged, sha256 prefix
  6e764dca952931c0 before and after the check).
- Workaround requested/authorized: none. No lock bypass, nothing cancelled or killed.
