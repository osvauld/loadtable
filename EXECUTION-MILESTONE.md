# Task manager: active continuation and execution milestone

## Active location

Continuation now lives in `/home/abe/p2p_chat/.claude/worktrees/task-manager-next`
on branch `tools/task-manager-next`. The main-checkout tool directory has been
relocated; do not recreate it there. See `HANDOFF.md` for current ownership and
worktree validation. The sections below record the earlier main-copy milestone.

## Selected base (historical)

The user selected the implementation with the most additions: `tools/task-manager/`
in the main `/home/abe/p2p_chat` checkout on `dev`. Continue here, not in
`.claude/worktrees/task-manager`. The old worktree is retained untouched; its
`AGENTS.md` change is not part of this continuation. Both tooling directories
were untracked at inspection; no staging, commit or merge is authorized.

`TAKEOVER.md` describes the earlier control-plane recovery, not current execution
capabilities. The current README documents the version-2 executor. Its old example
revision is illustrative only: submissions need the actual current full Git HEAD.

## What is enabled

- One executable catalog entry: `task-manager.tests`, `debug/offline`, `quick`.
- Per-job explicit approval and separate worker start; neither opening the window
  nor submitting a proposal executes it. This policy is preserved, not a newly
  resolved product decision. Agents can use the existing CLI approval/start actions.
- Bounded source capture, SHA-256 sealing, shared build/run admission, freshly
  executed test harnesses, durable results, cancellation and interruption handling.
- No real Tinline core, media, desktop or Android suites are admitted yet.
- Runtime observations are not queue-aware completion ETAs.

See README.md for source trust assumptions, cleanup limitations and the command
reference. Approval is not sandboxing.

## Continuation changes

- Compact job table at ordinary window widths: essential phase, elapsed/runtime
  and actions stay together; suite hover retains owner/config/source/revision and
  submission metadata. Smaller widths can still scroll horizontally. Wide windows
  retain the metadata columns. Approval warning remains explicit.
- Runtime spread and sample detail remain in the hover text instead of widening
  the runtime column.
- ASCII submit/approve/start instructions avoid the missing arrow glyphs.
- Readable log view extracts rendered compiler diagnostics and test-harness/build
  summaries instead of dependency artifact JSON. Unknown events, partial tail
  lines and ordinary test output are retained. A raw-JSON toggle exposes the exact
  received tail. Stored logs and the protocol are unchanged.
- Three deterministic formatter tests cover diagnostics, summaries, build failure,
  unknown events, missing diagnostics, partial tails and empty logs.

## Validation status

Prior handoff reports 34 Rust tests passed, clean clippy and a live self-suite run.
Those are historical reports, not fresh validation of these changes.

Fresh checks passed through the shared build lock, with a separate target:

```sh
export CARGO_TARGET_DIR=/home/abe/build-cache/p2p_chat/task-manager-continuation-target
python3 scripts/buildlock.py --who pi-task-manager-continuation -- sh -c \
  'cargo test --locked --offline --manifest-path tools/task-manager/Cargo.toml &&
   cargo clippy --locked --offline --manifest-path tools/task-manager/Cargo.toml --all-targets -- -D warnings &&
   cargo build --locked --offline --manifest-path tools/task-manager/Cargo.toml'
```

Results: **37 Rust tests passed, 0 failed, 0 ignored** (17 library, 3 binary/log
formatter, 7 contracts, 4 reconnect, 5 snapshot, 1 socket). Doc-tests contained
0 tests. Clippy passed with warnings denied; the binary build passed. Final log:
`/tmp/task-manager-continuation-final-checks.log`. Initial fresh-cache validation
also passed: `/tmp/task-manager-continuation-checks.log`.

The first foreground queue wait was terminated by the tool timeout before
compilation began; the exited process was confirmed before the single background
queue entry was created. Native validation revealed wrapped action labels; these
were given a minimum width and the full checks above were rerun successfully.

Native smoke command:

```sh
python3 scripts/buildlock.py --who pi-task-manager-ui --run -- \
  python3 /tmp/task-manager-continuation-ui.py
```

Disposable state contained one proposal, one approved job and manual runtime
observations; no worker was started. Compositor geometry was checked at 1060 and
760 px. The phase, elapsed/runtime and unwrapped Approve/Cancel/Logs controls were
visible; the 760 px screenshot captured the full window. At 1060 px the output
edge cropped the far-right part of the floating surface, but not these columns.
Initial Sway resize attempts retained the initial geometry; the smoke now retries
and asserts actual geometry rather than trusting a successful resize command.
Fixture processes were stopped and prior focus restored. Final screenshots/state:
`/tmp/tm-continuation-ui-ekvfr055`. The smoke is local, not a checked-in portable
GUI automation harness. GUI button clicks, log toggling, hover behavior and worker
E2E were not revalidated; formatter tests do not prove those integrations.

## Live daemon caution

PID 611058 was still running at inspection, from the older takeover target. The
handoff identifies it as protocol v1 and owner of the default state directory.
Do not kill it, replace its state or assume a rebuilt v2 client can use it.
For later live validation use a disposable private `TASK_MANAGER_STATE_DIR`.
No existing daemon or user data was modified in this continuation.

## Remaining work

1. Native button/hover/log-control interaction checks and wider-window layout.
2. Settle agent-managed automatic execution versus explicit approval/start before
   changing those defaults; selection of the main copy did not authorize that.
3. Review real Tinline catalog entries and their source/resource/permission needs
   before extending execution beyond this self-suite.
4. Queue-aware estimates only from actual admission and worker observations; no
   fabricated finish times or percentage progress.
