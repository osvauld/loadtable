# Trial feedback: macOS handoff template structural validation

- Agent/session: macos-handover; branch feature/macos-handover, starting HEAD
  fd8d016a6b539c79a27cd92323e033d6526d90c9; dedicated worktree.
- Job ID(s): 3896 (failed, own test bug), 3897 (passed).
- Outcome: no issues observed with task-manager admission/execution/result reporting.
- Redacted command, cwd type, resource class:
  `tasks run --run --who macos-handover -- python3 -B -m unittest discover -s tests -p test_macos_fixture.py -v`;
  live dedicated checkout, run-only slot, no compilation.
- What was exercised: real installed `tasks list`, `tasks admission`, compact
  active listing and two lightweight admitted Python executions on Linux.
  No macOS runner, SDK, Cargo build, CI dispatch, signing, simulator launch,
  deployment, network request or peer pairing was exercised.
- Expected: run-only structural tests record their actual exit statuses, without
  taking the busy compile/offload slots or modifying the pinned runtime/state.
- Observed: job 3896 exited 1: my reference-regex test incorrectly counted an
  Xcode TargetAttributes entry as an object definition. After tightening the
  regex to object definitions with `isa`, job 3897 passed all 8 tests (exit 0).
  Tests parse plist/scheme XML, resolve project object references, inspect
  manual workflow gates/pins and bounds, check normalized ignored output paths,
  and run `bash -n` on shell blocks. A separate lightweight direct PyYAML
  BaseLoader check parsed the workflow and verified its trigger/permissions/job
  structure. This is not GitHub Actions schema/execution acceptance, Xcode
  project parsing, or binary build validation. The manager placed the ignored checkout target
  link on the first job; it was preserved. Existing compile/offload holders
  were visible and left untouched.
- Impact: lightweight validation completed without heavy build contention;
  the fixture/workflow remain explicitly unexecuted macOS templates.
- Suggested improvement: none from this use. Full unfiltered list was large,
  as documented; the existing compact `--active --limit 5` option was useful.
- Acceptance test for suggestion: N/A.
- Workaround requested/authorized: none; no admission bypass. Mac availability,
  parent port/relay CLI completion, and authorized published CI access are
  blockers to native acceptance, recorded in docs/MACOS-FIRST-RUN.md rather
  than worked around. User authorized an owned-files commit only on this branch.
