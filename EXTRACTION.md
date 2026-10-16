# Standalone extraction / trial adoption

## What changed

- Authoritative source is `/home/abe/task-manager`, outside Tinline, initialized
  as a local Git repository on `main`. No commits, staging, remote or pushes.
- Copied the independent Cargo workspace, source/tests, historical docs and GPL
  license. Existing Tinline worktrees/copies were NOT removed or reset.
  The owned historical next-worktree copy now has MOVED.md.
- Tinline main AGENTS.md directs agents to this source and TRIAL.md, requires
  shared facility use and session feedback in their report plus a unique file
  in feedback/. Existing sessions must reread; no claim of universal adoption.
- feedback/TEMPLATE.md defines evidence, impact, suggestion and acceptance test.
  No-issues-observed is valid. Blockers must be reported, not silently bypassed.
- No runtime upgrade/restart/migration. Installed pinned native release, default
  shared state, global lock inodes and durable history are unchanged. New repo's
  target is an independent cache-disk link, ignored including symlink form.

## Validation

Shared native job **8**, owner `standalone-extraction-validation`, live cwd this
repository, ran:

```sh
cargo test --offline --locked -j 2
cargo clippy --offline --locked -j 2 --all-targets -- -D warnings
cargo build --offline --locked -j 2
```

**90 Rust tests passed, 0 failed/ignored**, clippy and build exit 0.
Evidence `/tmp/task-manager-extraction-validation.log`, marker
`EXTRACTION_VALIDATION_EXIT=0`. No app workspace dependency or cache-policy override.
Four Python wait-budget tests passed with:
`PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -p test_wait_budget.py -v`.

Fresh standalone binary's opt-in native CLI test:
`python3 tests/native_commands.py "$PWD/target/debug/tasks"`, evidence
`/tmp/task-manager-extraction-native-commands.log`. It acquires admission
internally, not under an outer compile lock. Initial independent-client/two-run
slot assertions passed; its tiny Cargo stage waited correctly behind an actual
legacy compile holder labelled `callui-android`. No duplicate launched and no
other agent's work was terminated. Final result is recorded below when complete.

## Retained limits

This is a source extraction, not a generalization of every Tinline adapter.
Package name, stable compatibility paths/locks and immutable suite contracts are
retained. Source-capsule self-suite still expects tools/task-manager beneath a
committed source repository; the new root package/unborn HEAD is not supported
by that adapter. The GUI's legacy source default also needs generalization.
Use explicit live cwd commands here; no fabricated Git HEAD/initial commit.

No sealed artifacts, production integration, phone/network tests, native Android
APK acceptance or full crash E2E rerun is claimed by this extraction. Prior crash
E2E evidence is historical; the actual relocated-source compiler/native checks
are separately identified above. No cleanup/data wipe/install/publish operation.
