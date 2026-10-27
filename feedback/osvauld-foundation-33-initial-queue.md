# osvauld-foundation — job 33 — initial admission

## Work exercised

Created an isolated orphan task-manager worktree without staging/committing, then used the installed shared coordinator to enqueue/approve/start one bounded offline foundation test + clippy job. Command cwd is `experiments/osvauld-foundation` in that worktree. The experiment imports the real local osvauld2 crates; it is not a simulation or remote execution proof.

Command: `sh -c 'cargo test --offline -j 2 && cargo clippy --offline --locked -j 2 --all-targets -- -D warnings'`.

## Observations

- `enqueue` returned authoritative `submitted_id=33`; approval and worker start returned successfully.
- Job 33 appeared as `osvauld-foundation:job-33` in native admission's compile waiters. The compile slot was owned by `lead-vp8enc:job-30`, with jobs 31 and 32 also waiting. No lock bypass, duplicate submission or cancellation of other work.
- At that observation host pressure had no blocker and available memory was about 15.6 billion bytes. A busy compile slot still correctly prevents concurrent compilation.
- No issues observed in submission/approval/queue visibility. Subsequently job 33 acquired admission and PASSED: 10 tests, 0 failed, 0 ignored; warnings-denied all-target clippy also passed. First dependency resolution was offline, with explicit `-j 2`. These are real parent API tests, not remote-integration acceptance.
- A cache-placement gap was observed: `local_command::place_outputs` discovers the Git root and places its target link there, while this independent nested Cargo workspace generated its own ignored local `target` (~1.2 GiB). No cache environment overrides were supplied. The output was preserved, not moved/deleted, and further large builds should wait for a placement solution. This is not caused by missing admission.
- `tasks list` returned a large full-history JSON response. A compact owner-filtered view would make checking one agent's job easier; this is a usability observation, not a scheduling failure.

## Acceptance / follow-up

Job 33 completed successfully; no retry is needed. Retained local diagnostic: `/tmp/task-manager-osvauld-foundation-job33.json`. Parent HEAD/status remained unchanged at the post-run check.

Nested placement acceptance: a job whose cwd is an independent Cargo workspace below a Git root should use a uniquely owned cache-backed target for that workspace without overriding machine environment policy or changing/moving any populated existing target. Exercise both direct Cargo and compound shell argv; preserve root-workspace behavior and different-checkout isolation. This follow-up is recorded, not implemented in this foundation slice.

Suggested compact view acceptance: owner filtering shows only that owner's jobs with one line per ID, phase, exit code and argv summary, while explicit JSON preserves existing protocol data. No implementation of this suggestion is included here.
