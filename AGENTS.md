# Task-manager repository instructions

This is the authoritative standalone source repository for the shared Rust/egui
workload coordinator. It was extracted from Tinline without deleting historical
copies or changing the running service/state. Read README.md and TRIAL.md fully.

- Verify cwd, branch, HEAD (may be unborn) and git status before work.
- Coordinate exact file ownership; never overwrite another agent's work/feedback.
- Heavy builds/tests use installed `tasks run --who UNIQUE_AGENT -- COMMAND`.
  Inspect `tasks list` and `tasks admission` first. No outer lock around worker
  E2E/self-suite/native_commands scripts: their workers acquire admission.
- Required trial feedback: task report plus your own new feedback file as in
  TRIAL.md. Record blockers before requesting a workaround; don't bypass locks.
- Keep each checkout stable during live compile/run. Honor host cache policy;
  don't override RUSTC_WRAPPER, CARGO_TARGET_DIR, GRADLE_USER_HOME or share targets.
  Missing cache-output links are placed by tasks; don't delete/migrate outputs.
- Build outputs, logs, captures, credentials and state must stay outside Git.
  Preserve required package/runtime assets and deliberate baselines.
- No staging, commits, pushes, merges/resets, phone installs/data wipes, cache
  cleanup or unrelated service restarts without explicit authorization.
- Installed runtime is a pinned independent release, not this Cargo target.
  Rebuilding source is NOT permission to replace it or restart a busy daemon.
  Preserve existing SQLite/socket compatibility, job history, stable flock
  inodes, approval semantics, process-group/descriptor leases and cancellation.
- Tinline adapters/cache namespaces are retained compatibility, not permission
  to modify Tinline, weaken its protocol policy or claim generic hermetic builds.
- Use synthetic identities/fixtures. Report exact checks/results/ignored tests
  and limitations; format owned files only. Models aren't production integration.

Repository is local, initially uncommitted, no remote. Do not manufacture an
initial commit just to give source-capsule tests a Git HEAD; request permission.
