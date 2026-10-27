# Trial feedback: lead review reruns + reading another agent's jobs

- Agent/session: lead-vp8enc (Tinline lead session, reviewing subagent vp8enc-core)
- Job ID(s), or "not submitted" with reason: 22, 24 (plus the run just before 22 for `cargo test --release -p video --features vp8 vpx`; its ID scrolled out of my tail)
- Outcome: friction (minor); no defects observed
- Redacted command, cwd type, resource class: `tasks run --who lead-vp8enc -- env VPX_DIR=<libvpx prefix> cargo test --release -p p2pcore --features vp8,dev-tools --test video_calls`; `tasks run --who lead-vp8enc -- cargo test -p proto`; live git worktree cwd; compile class
- What was exercised (real execution vs model/read-only observation): real execution of three compile+test jobs; `tasks admission`; `tasks list` read-only to see a subagent's jobs; `tasks logs 22` read-only
- Expected: run/wait/exit status; an easy way to see another agent's jobs and their states
- Observed (include evidence; unknown is fine):
  - `tasks run` printed `submitted job 22`, then `WaitingBuild` -> `Building` -> `Passed`, and returned the test output. All three runs passed, no queueing wait (admission showed every lock `busy: false`).
  - To answer "where is the subagent?" I filtered `tasks list` JSON for `--who vp8enc-core`. A naive recursive walk found the objects holding `configuration` (argv) but `id`/`state` were not on that same object (my script printed `None None None` for each), so I could see which commands existed but not their states without learning the schema.
  - `tasks logs 22` begins with `"execution_enabled": false` even though job 22 ran and passed; unclear what that flag means to a reader.
- Impact: small; the status question took an extra round of guessing. The `execution_enabled: false` line could make a reader think the job did not run.
- Suggested improvement (or "none from this use"): a human/compact filter, e.g. `tasks list --who NAME --brief` printing one line per job: `id state exit who argv-summary` (PATH-like env values elided). Document or rename `execution_enabled` in `logs` output.
- Acceptance test for suggestion (or N/A): with jobs from two `--who` names in the ledger, `tasks list --who A --brief` prints only A's jobs, one line each, including id and terminal state, and no env value longer than ~40 chars.
- Workaround requested/authorized (if any; no silent bypass): none needed
