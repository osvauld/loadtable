# Mac preparation handoff

Branch/worktree: `feature/macos-worker` at
`/home/abe/task-manager-worktrees/macos-worker`, based on main fd8d016.
Three subagent branches were merged locally. Nothing was pushed or deployed by
this task; no Mac/Apple SDK/CI runner was accessed.

## Is `tasks run --peer mac --fetch ... -- xcodebuild ...` ready?

**No. Do not tell the app agent that a Mac worker is ready.** The existing Linux
peer path works and has loopback regression coverage, but enabling the macOS
worker safely still requires native lease observation, supervisor-death cleanup,
descriptor hygiene, admission/metrics/health and module-gate work. Details and
native acceptance cases: `docs/design/macos-platform.md`.

No macOS worker binary, .app/.ipa build, real relay transfer or native egui launch
has been validated. Source/config/model tests are not those acceptance tests.

## Prepared

- Native Linux/macOS identity, memory/disk and child-usage primitives in
  `crates/platform`, keeping the existing process identity JSON shape.
- A standalone **read-only setup app**, `crates/setup` / `tasks-mac-setup`, sharing
  its inert schema, fixed opt-in probes and egui window with `tasks setup`.
  It is NOT a worker and cannot pair/install/execute jobs. Isolating it means it
  does not depend on completing the Linux coordinator's Mac port first.
- Explicit relay opt-in for the coordinator, preserving LAN defaults and the
  retained LAN adapter. Operators select one shared HTTPS relay and verify peer
  IDs; no default/public relay or discovery was enabled. Real relay behavior is
  unverified. See `docs/design/peer-relay.md`.
- Manual-only CI template for a setup .app ZIP; full-worker build gated off by
  default. No repository publication, CI dispatch, signing or notarization.
- A small synthetic unsigned iOS Simulator Xcode fixture and structural tests.
  Fetch zipped bundles, not raw .app trees (symlinks are otherwise skipped).

## Other app agent

An iOS scaffold is being developed separately at
`/home/abe/p2p_chat/.claude/worktrees/ios-app/ios`. This task only inspected it;
no files there were changed. Its actual toolchain, XcodeGen, bindings and native
media dependencies must be coordinated with its owner. Do not infer the app is
buildable because a synthetic Xcode fixture exists.

## First Mac session

Have the owner supply Apple Silicon/macOS/RAM/free-disk facts and install/launch
full Xcode ahead of time, accepting terms locally. Use a dedicated standard
account. A reviewed real macOS runner is needed to produce linked artifacts.

Start with the setup app and the primitive tests. Only after the worker safety
port and native tests pass should the friend pair/approve a tiny synthetic remote
job. Then test Xcode simulator build + ZIP/hash return; signed device IPA export
is a separate, explicitly authorized milestone.

Read `docs/MACOS-FIRST-RUN.md` for commands, limits and outstanding work. The final
integration verification was blocked by another session's shared maintenance
drain; consult this task's coordinator feedback and recorded job log for the
latest result. Never clear that drain or replace the installed runtime to test
this branch.
