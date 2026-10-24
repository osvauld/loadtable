# Trial feedback: standalone source extraction and adoption

- Agent/session: standalone-extraction agent
- Job IDs: shared job 8; optional isolated CLI-test jobs 1–3 (not shared ledger IDs)
- Outcome: successful standalone compiler validation; usability/generalization follow-ups
- Command/cwd/resource: live `/home/abe/task-manager`, compile; shell chaining
  `cargo test --offline --locked -j 2`, warnings-denied all-targets clippy, build.
- Real evidence: 90 Rust tests passed, none failed/ignored; clippy/build exit 0,
  `/tmp/task-manager-extraction-validation.log`. Four Python wait-clock tests passed.
  The relocated binary's independent-client/two-concurrent-run assertions passed
  in `/tmp/task-manager-extraction-native-commands.log`.
- Queue observation: at this report's creation the optional tiny Cargo stage was
  waiting behind a real legacy compile holder labelled `callui-android`, observed
  through `tasks admission`; no timeout resubmission or external job termination.
  This is a point-in-time partial test result, not a complete CLI-suite pass.
- Expected: source outside Tinline can compile independently; agents can discover
  activity and submit/query without contention bypass.
- Observed: standalone build works, output link is private per canonical checkout,
  shared row 8 persists. Legacy holder visibility requires `admission` in addition
  to `list`. New-repo `.gitignore` needed `/target` rather than `/target/` to ignore
  the output SYMLINK; corrected in extraction, not an admission defect.
- Impact: existing sessions won't automatically reload new trial policy; source
  extraction alone doesn't migrate legacy traffic or generalize immutable adapters.

## Suggested improvements (not implemented)

1. A discoverable combined status view: show durable jobs plus clearly labelled
   live legacy lock observations. Never synthesize historical results or claim
   advisory holder text is authoritative lock ownership. Acceptance: while a
   compatible legacy client owns compile and native work waits, one query shows
   both, then clears the live-only row after release without creating fake jobs.
2. Root-package immutable self-suite / repository-aware GUI source chooser.
   Source inspection shows the retained tools/task-manager adapter and GUI
   `CARGO_MANIFEST_DIR/../..` default don't describe the new repo root. This was
   NOT an observed raw-command failure. Acceptance: support both layouts with
   the same sealing limits, clear unborn-HEAD diagnostics, no fabricated commit,
   and independent fixture tests; raw live execution remains distinct.

No silent workaround or permission expansion. No exact wait duration/ETA claimed.
Full optional CLI completion must be checked in its log/fixture summary; do not
promote initial assertions to a whole-suite pass. No device, network or production
runtime validation was performed.
