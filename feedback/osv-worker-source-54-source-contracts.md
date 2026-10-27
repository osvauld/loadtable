# Trial feedback: Linux-worker source-contract review fixes

- Agent/session: `osv-worker-source` (delegated review-fix pass on Sol's files).
- Job ID(s): 50 (red), 52 (green), 53 (real-host inspect smoke), 54 (final).
- Outcome: no issues observed.
- Redacted command, cwd type, resource class: owned osvauld-foundation worktree
  (live cwd, unborn HEAD); `tasks run --who osv-worker-source --run -- env
  PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -p 'test_*.py' -v`
  and `sh -c 'scripts/setup-linux-worker.sh --inspect ...; test $rc -eq 2'`.
  Resource: run (no compilation).
- What was exercised: real shared admission for four run-only jobs while another
  agent's Android build held the compile lease; Python fixture tests and one
  real-host read-only inspector run. No Cargo, toolchain, device or transfer.
- Expected: run slots granted without touching the compile slot; truthful exit
  status propagated; test-first red then green.
- Observed: job 50 failed as intended (27 run, 2 failures, 13 errors); 52 and
  54 passed 27/27, 0 skipped; 53 passed with inspector exit 2 (EndeavourOS
  x86_64 unsupported, as designed). Jobs 52/54 briefly showed `WaitingRun`
  before admission; no client timeout, resubmission or cancellation needed.
- Impact: none negative; bounded checks proceeded alongside other agents.
- Suggested improvement: none from this use.
- Acceptance test for suggestion: N/A.
- Workaround requested/authorized: none.

## NixOS follow-up (same agent, second assignment)

- Job ID(s): 93 (red: 32 run, 2 failures, 3 errors), 96 (failed on my own
  test bug: aarch64 loop iteration checked the x86_64 loader; test fixed),
  97 (green, 32 passed), 98 (real-host `--inspect`/`--plan` smoke; inspector
  exit 2 on EndeavourOS as designed, `nixos` null), 99 (final, 32 passed,
  0 failed/skipped). All `--run`, owner `osv-worker-source`.
- Outcome: no issues observed. Jobs 93/97/98 briefly showed `WaitingRun`
  before admission; exit status propagated correctly; no timeouts/resubmits.
- Not submitted: `nix flake check` — `command -v nix` found no Nix on this
  host, so the `nix/` draft is unvalidated rather than run through tasks.
- Suggested improvement: none from this use.
- Acceptance test: N/A.
- Workaround requested/authorized: none.
