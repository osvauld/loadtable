# Task-manager takeover — 2026-10-13

Historical control-plane handoff. The user subsequently selected this main-checkout
copy as the continuation base. See `EXECUTION-MILESTONE.md` and `README.md` for the
current version-2 execution milestone, validation and remaining limitations.

## Ownership and recovered progress

Active continuation is tools/task-manager in the main dev checkout (HEAD at
inspection: 69fae85f97915f58808bf7bb9f0fa199ab5b2ae1). Only this directory was
copied/edited. Original .claude/worktrees/task-manager remains untouched, including
its unrelated AGENTS.md change. No branch switching, staging, commits, merges,
installation, or removal of worktrees/build artifacts was performed.

The checkout has three separate registered worktree parent spellings:
.claude/worktree, .claude/worktrees, and .claude/worktreess. They are not aliases.
Do not rename/consolidate them while other worktrees or sessions may use them.

The preceding session stopped due to its rate limit while checking window-driven
daemon restart. Its process-selection smoke test had PID-selection errors. An
isolated reproduction of the existing release binary showed restart already
worked. The continuation makes the recovery boundary explicit and testable:
only a missing/refused initial connection can start a daemon and retry. Once
connected, a write/read/protocol error never replays a potentially applied action.
This helper is shared by the window and CLI.

Corrected obsolete taskctl wording to tasks and renamed the table's ETA column
Est. runtime. These numbers are self-reported runtimes for recorded stages, not
scheduled completion times. No executor or actual admission scheduler is enabled.

## Validation

Dedicated target directory (no writes to the old worktree's cache):
/home/abe/build-cache/p2p_chat/task-manager-takeover-target

Commands from the main checkout, with CARGO_TARGET_DIR set to that directory:

~~~sh
python3 scripts/buildlock.py --who goose-task-manager-takeover --   cargo test --locked --offline --manifest-path tools/task-manager/Cargo.toml
python3 scripts/buildlock.py --who goose-task-manager-takeover -- sh -c   'cargo clippy --locked --offline --manifest-path tools/task-manager/Cargo.toml --all-targets -- -D warnings && cargo build --locked --offline --manifest-path tools/task-manager/Cargo.toml'
python3 scripts/buildlock.py --who goose-task-manager-live-smoke --run --   python3 /tmp/tm-takeover-smoke.py
~~~

- Tests: 12 passed, 0 failed, 0 ignored (7 contracts, 4 reconnect, 1 real daemon
  socket test). Socket test leaves the crash socket in place and checks recovery
  and persistence. Reconnect tests cover missing socket, live daemon, lost reply
  after mutation delivery, and failed startup.
- Clippy: all targets passed with warnings denied. Binary build passed.
- Live smoke: six simultaneous CLI cold starts succeeded with one active daemon;
  submit, record (EWMA 10s then 20s -> 12s), list and cancel succeeded.
- A silent connection did not block another client's request (0.001s observed).
- Native egui window was visible in Sway. Killing only the disposable fixture's
  daemon caused the open window to restart it in 4.58s. Jobs/history survived;
  closing the window left the daemon running.
- Smoke used a disposable state directory and killed only its own processes.
  Logs retained outside Git at /tmp/tm-takeover-final-33n0r8rn. The smoke script is
  local validation, not a portable checked-in GUI automation harness. No GUI
  button-click/screenshot assertion, phone/emulator, or actual test-execution
  integration was validated. No Rust tests were skipped.

## Launch now

~~~sh
/home/abe/build-cache/p2p_chat/task-manager-takeover-target/debug/tasks
~~~

No arguments opens the dashboard and starts the daemon automatically. Agents use
the same executable with list, submit SPEC.json, cancel ID, or record arguments.
See README.md for fields, state-directory overrides and build instructions.

## Next work (not silently enabled)

1. Review a bounded task catalog with source revision, build dependencies,
   resource requirements, and destructive/device-operation permission gates.
2. Implement an explicit local worker using immutable source/artifact preparation
   and the existing build/device locks. Avoid nested lock acquisition.
3. Add execution phases, structured logs/results, restart reconciliation and safe
   process-group cancellation before admitting real work.
4. Record worker timings and distinguish partial runtime estimates from resource
   wait and scheduled finish estimates. Add UI/per-test progress only when real
   observations exist.

The current SQLite ledger is a proposal/control plane, not a replacement for
buildlock or evidence of production scheduler correctness.
