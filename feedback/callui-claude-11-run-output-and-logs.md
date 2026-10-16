# Trial feedback: `tasks run` output and `tasks logs` readability

- Agent/session: callui-claude (Tinline call UI session, 2026-10-16)
- Job ID(s): 11
- Outcome: friction (the job itself worked: no issues observed with execution)
- Redacted command, cwd type, resource class: `tasks run --who callui-claude -- cargo test --offline --locked -p tin-icons --lib`,
  live main checkout (`dev`), resource `compile`
- What was exercised (real execution): `tasks list`, `tasks admission`, `tasks run` (submit, approve, start, wait in one call),
  `tasks list` to find the job by owner, `tasks logs 11`. Small, already-compiled test crate: build stage about 0.16 s, run stage about 0.16 s.
- Expected: one command gives me the job ID and result; `tasks logs ID` shows that job's log.
- Observed:
  1. The result was correct: phase `passed`, 15/15 tests. The build stage ran the same argv with `--no-run` appended, then the run
     stage ran the tests. Admission was immediate ("Native Rust admission granted (shared flock protocol)").
  2. I piped `tasks run` through `tail -25` and lost the job ID: the lines kept were the test output, then the
     shell's timing. I had to search `tasks list` JSON for my owner string to find ID 11. I did not check whether the ID is printed
     at the start only (unknown).
  3. `tasks logs 11` returns the whole ledger JSON (all 11 jobs, ~22 KB) with the job's log under `.log.text`. To read one log
     I had to parse the JSON.
  4. The job spec shows `"cost": "cpu_simulation"` for a plain `cargo test` local command. I don't know if that is meant as a
     generic default or is a misclassification (hypothesis, not verified).
- Impact: small, but it adds friction for agents that capture only the end of the output (common, to save context). That pushes
  them towards guessing IDs from `tasks list`, which TRIAL.md warns against.
- Suggested improvement: (a) end `tasks run` with one summary line `job 11 passed (exit 0) owner=callui-claude`;
  (b) `tasks logs ID` prints only that job's log text by default (`--json` for the full record); (c) label local commands'
  cost as something other than `cpu_simulation`, or document what it means.
- Acceptance test: `tasks run --who X -- true | tail -1` prints a line containing the new job's ID and `passed`;
  `tasks logs <that id> | head -1` starts with the job's log text, not `{`; `tasks logs <id> --json` still returns the record.
- Workaround requested/authorized: none needed; I found the ID by owner in `tasks list`.
