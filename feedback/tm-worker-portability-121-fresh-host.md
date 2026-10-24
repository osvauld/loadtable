# Trial feedback: configurable admission thresholds and fresh-host portability

- Agent/session: `tm-worker-portability`, worktree `task-manager-worktrees/osvauld-foundation`
  (branch `feature/osvauld-foundation`, unborn HEAD, all files untracked).
- Job ID(s): 113 (red run), 117 (first green test run), 119 (clippy failed),
  120 (clippy passed), 121 (final tests passed). All `local.command`, compile class,
  submitted with `tasks run --who tm-worker-portability -- cargo ... -j 2`.
- Outcome: friction (long queue wait), no coordinator defect observed.
- Redacted command, cwd type, resource class: `cargo test --offline --locked -j 2` and
  `cargo clippy --offline --locked -j 2 --all-targets -- -D warnings`, live worktree cwd, compile.
- What was exercised: real execution through the installed shared queue. The new
  threshold code ran only in unit tests with synthetic lookups, records and pressure
  samples. The worker host itself was NOT used, and no daemon ran with the new code.
- Expected: red run fails, then green runs.
- Observed:
  - Job 113 waited about an hour behind compile job 112 (another owner). The log named
    the blocker as `CPU PSI some avg10 above 40%` three times as the state flapped, then
    showed "Blocking waiting for file lock on package cache". The red run failed as
    intended: 80 compile errors from the missing threshold API, exit 101.
  - 117 and 121: 110 tests passed, 0 failed, 0 ignored (lib 89, plus 4/7/4/5/1/0 in
    the other targets). This includes 7 new tests.
  - 119: clippy `manual_is_multiple_of` (2) in new code. After the fix, 120 passed.
  - `tasks run` printed final summaries correctly. The client never hit EAGAIN, so
    nothing was resubmitted.
- Impact: a hard-coded 8 GiB MemAvailable gate means a 7.6 GiB worker could never
  admit anything. On a hand-started daemon, an SSH-autostarted daemon takes the
  client's environment, so per-host thresholds need a unit plus a worker record to be
  reported truthfully.
- Suggested improvement:
  1. Implemented here: `TASKS_MIN_AVAILABLE_MEMORY_MIB`, `TASKS_MIN_CACHE_FREE_GIB`
     and `TASKS_MIN_STATE_FREE_MIB`, with the worker record
     `<state>/admission-thresholds.json` and `thresholds` in `tasks admission`.
  2. Not implemented, needs the placement owner: when `git` is missing,
     `local_command::place_outputs` fails every live command with a bare
     "No such file or directory". It should instead give an explicit error naming git,
     or skip placement and log that it did.
- Acceptance test for suggestion 2: run a `tasks run -- true` job in a non-Git dir
  with PATH lacking git. It either passes, logging "placement skipped: git not found",
  or fails with a message naming git. It never fails with a bare ENOENT.
- Workaround requested/authorized: none. The README documents git as a worker
  prerequisite.

## Fresh-host audit (local.command path), read-only code review

Fixed or added here:
- Memory gate (8 GiB) and the cache (20 GiB) and state (256 MiB) disk reserves can now be
  set by validated env variables. Defaults are unchanged. `tasks admission` and
  `tasks cache` report the effective values and their source. Blocker text names
  the effective threshold, and the default text is unchanged.
- Daemon lifecycle: added a `deploy/worker/` user unit template. Without it, the first
  SSH client autostarts the daemon with its own env/PATH, and the daemon survives
  logout only if logind does not kill user processes.

Already fine on a fresh host, no change needed:
- The state dir is created by `ensure_daemon` and `daemon::run` (0700).
- `/tmp/claude-$UID` is created on the first acquire. Status tolerates its absence.
- `~/build-cache/...checkouts/<key>` is created on demand. The disk check walks to the
  nearest existing ancestor.
- The device lock list only reflects existing files. GRADLE_USER_HOME is set only if
  that directory exists.

Deferred, with no code change:
- A missing `git` fails every live command, as described above (owner: local_command placement).
- Admitted commands use the daemon PATH, not the client's, and nix develop shells are
  not visible. This is documented, and the unit sets PATH.
- `/proc/pressure/*` is required, and admission fails closed without it. Documented.
- The `/tmp/claude-$UID` lock dir name is not configurable. systemd-tmpfiles age
  cleaning of /tmp could remove idle lock files, and a unit with PrivateTmp would
  split the namespace. The draft `nix/worker-module.nix` service sets
  `PrivateTmp = true`, which would break `tasks` admission sharing if it were reused
  for this daemon (that file is not mine and was not changed).
- `~/build-cache/p2p_chat` and `.local/state/tinline-task-manager-native` are compat
  namespaces that are kept as they are.
- Self-suite target `~/build-cache/p2p_chat/tasks/targets/self-tests`, sccache
  `~/build-cache/sccache`, `cache-plan` root, Android/Gradle `~/build-cache/gradle`,
  libaom tarball and android-lanes paths: Tinline-specific adapters, not touched.
- A host-built binary (Arch glibc, /lib64 loader) will not run on NixOS without
  nix-ld. Build it on the worker.
- `~/.local/bin` is not on the non-interactive SSH PATH on NixOS. Use an absolute path.
