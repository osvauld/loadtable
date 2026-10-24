# Trial feedback: read-only local Linux-worker contracts

- Agent/session: Sol delegated implementation; owner `sol-linux-worker`.
- Job IDs: 37, 38, 40, 41, 42 (all owned, run-only; no Cargo/build jobs).
- Outcome: no issues observed in admission; known nested-target blocker remains.
  Client timeout was reconcilable without duplicate submission.
- Commands/cwd/resource: owned osvauld-foundation worktree;
  `env PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -p 'test_*.py' -v`
  for final job 41; job 42 was Python subprocess assertions for
  `scripts/setup-linux-worker.sh --inspect/--plan/--apply`. Resource: run.
- Exercised: real shared admission and Python fixture/CLI execution. No remote
  transfer/execution, Android operation, installation or production deployment.
- Expected: preserve the compile slot for other agents, honor host PSI gates,
  return authoritative job IDs/results, do not duplicate after client timeout.
- Observed: job 37 failed import before implementation (test-first); 38 passed
  the initial 10 tests; expanded job 40 had five new credential-name subtest
  errors before exclusions were added; final 41 passed 18, 0 failed/skipped.
  Jobs 40/42 logged memory PSI some/full waits before normal admission. The
  job-42 client timed out after 20 seconds while waiting; `tasks logs 42` and
  `tasks logs 42 --json` confirmed it later passed (job exit 0). No resubmission
  or cancellation. Real EndeavourOS x86_64 was correctly unsupported by the new
  inspector (CLI exits 2); supported-distro coverage was synthetic only.
- Evidence: retained `tasks logs 37/38/40/41/42`; outside-Git
  `/tmp/sol-linux-worker-job{38,40,41,42}.json` and host CLI JSON. Full state/logs
  are deliberately not copied here.
- Impact: bounded Python checks could proceed without expanding the preserved
  nested Cargo target or bypassing another agent's active compile lease.
  No timing/performance or successful installed-worker claims are made.
- Suggested improvement: retain the existing job-33 nested-placement follow-up;
  no runtime change attempted here. A compact owner/ID status query would also
  reduce the large full-history JSON response when reconciling a timeout.
- Acceptance test: nested independent Cargo workspaces get unique cache-backed
  targets for direct/compound argv without moving/deleting existing targets or
  sharing across checkouts; preserve root placement and host environment policy.
  Compact query: one owned ID returns phase/exit/owner without unrelated history,
  retaining the existing full JSON protocol as an option.
- Workaround requested/authorized: none. Placement is documented as blocking
  further heavy Cargo builds. No gate weakening, cache override, cleanup,
  daemon restart, deployment replacement or other agent's job cancellation.
