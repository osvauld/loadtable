# macOS adaptation — development checkpoint

This is an incremental backend adaptation, not a second implementation of the task manager.
**The headless `tasks` application now compiles and links on macOS. Local runtime remains
explicitly disabled.** The CLI accepts offline catalog/setup inspection (and fixture UI inspection
when built with UI); other commands reject before connecting to a daemon, creating state or
submitting jobs. No macOS daemon, GUI, scheduler, recovery or local execution acceptance is
claimed.

## Shared primitives implemented

`crates/platform` (`task-manager-platform`):

- `MachineSampler` retains a `sysinfo` collector for CPU, memory, swap, core count and
  hostname. No background thread or process/environment enumeration. CPU is unknown until
  two sufficiently separated samples exist. Values are observational: sysinfo's available
  memory has OS-specific semantics, not a promise of Linux MemAvailable equivalence.
- `process::{identity, alive}` supplies one process-identity API. Linux preserves
  `/proc/PID/stat` start ticks. macOS uses `proc_pidinfo(PROC_PIDTBSDINFO)` with full-buffer,
  PID, timestamp-range and overflow checks. Its token is the microsecond start timestamp.
  Zombie state or a native ESRCH result is not live. sysinfo's seconds-resolution process start time is deliberately
  NOT used to authorize cancellation. Identity tokens are host-local.
- `process::signal_owned_group` uses `nix`, requires a matching native identity and a group
  leader, and preserves ESRCH handling. Like the existing implementation, it requires an
  owned stage child whose leader remains unreaped through escalation; identity validation
  plus killpg is not atomic and not a substitute for that supervisor contract.
- `src/worker.rs` delegates its identity/aliveness/group-signal helpers to this crate.
  Worker startup and internal stage/admission/command entry points reject unsupported local
  runtime before accessing state. Parent-death and inherited-FD hooks are native operations;
  macOS returns ENOTSUP rather than skipping protections.
- `thread` selects Linux's existing per-thread rusage/nice/idle-I/O calls or macOS's native
  thread CPU clock and thread background priority. `fs::file_key` handles native dev_t types.
  These small primitives remove Linux syscall/type dependencies from shared health code.
- Linux daemon auto-start now uses the same inherited-FD scrub hook as detached device
  helpers: close_range where available, with the existing helper's fcntl fallback otherwise.
  This replaces the old daemon-specific best-effort close_range-only call. Linux runtime
  validation of this consolidation remains outstanding.

Shared library modules are no longer hidden wholesale behind Linux guards. Admission
thresholds, policy, queues, flock leases, cache planning, snapshots, health data and Unix
transport compile together. `src/admission/native.rs` owns live pressure acquisition;
Linux's PSI behavior and error messages are retained, while macOS pressure acquisition
returns Unsupported. `src/runtime_support.rs` distinguishes compilation from runtime
acceptance and is checked before CLI, daemon and worker mutations. The existing Linux
monitoring readers still need macOS integration; compiling them is not a native data feed.

`crates/peer`:

- `nix::errno::Errno::clear` replaces direct Linux errno access around `readdir`.
- One small target-selected rename helper uses Linux `renameat2(RENAME_NOREPLACE)` or
  macOS `renameatx_np(RENAME_EXCL)`. Both retain atomic no-overwrite publication. Other
  targets explicitly return Unsupported, not a check-then-rename fallback.
- The rest of source-transfer logic stays shared.

Existing Cargo `ui` / `mcp` features remain independent of the compilation target. Do not
add manually selectable linux/macos features that can disagree with the actual target.
The existing ledger, wire protocol, approvals, slot filenames and identity JSON field
names have not changed. The Linux default Cargo cap remains two; `-j 4` below is an
operator-approved local macOS bootstrap, not a change to deployed Linux policy.

## Integration with current dev

Merged `origin/dev` at `f110c1b` into `dev-macos`. Its canonical serialized
`ProcessIdentity`, native memory/disk/child-usage APIs, capabilities, setup app, relay
configuration and tests are retained. Our signalling/pre-exec helpers are in
`process_control.rs`, while observational sysinfo readings are in `telemetry.rs`.
Redundant native identity implementations were removed, not maintained as competing APIs.

Native integration exposed two previously unexecuted cases: the running kernel returned
40 VM-statistic words while libc describes a larger revision; the reader now validates the
returned prefix through the last field it actually uses, never reads an unreported field,
and rejects short/oversized replies. Darwin libproc also returned ESRCH for an unreaped
zombie. Its test now captures identity while the child is held alive, permits that native
ESRCH observation, proves it is not live, and always reaps the child. This does NOT prove
macOS descendant cleanup when its group leader is a zombie; runtime stays disabled.

## Validation on ARM64 macOS

```sh
# Direct Cargo bootstrap explicitly approved because no installed tasks exists here.
# Do not generalize this exception to hosts with an installed coordinator.
cargo check --locked --no-default-features -j 4
cargo test --locked --no-default-features --test macos_boundaries -j 4
cargo test --locked --no-default-features --lib --no-run -j 4
cargo test --locked --no-default-features --lib admission::ops_tests:: -j 4
cargo test --locked --no-default-features --lib local_command::tests:: -j 4
cargo test --locked --no-default-features --lib runtime_support::tests:: -j 4
cargo test --locked -p task-manager-platform -p task-manager-peer --lib -j 4
cargo clippy --locked --no-default-features -p tinline-task-manager --lib --bin tasks -j 4 -- -D warnings
```

Observed after merging dev: headless check and link passed; the default UI/MCP-feature
metadata check also passed (not GUI linking/launch acceptance). Selected executions passed
76 tests: 4 macOS CLI/backend boundary tests, 9 shared admission fixture tests,
8 command-policy tests, 1 runtime-support test, 24 platform tests, 10 peer library tests,
17 setup library tests and 3 Apple-artifact integration tests. Zero failed/ignored in the
final runs; other main-library tests were filtered, NOT validated. Headless application
and boundary-test Clippy passed with warnings denied; platform/peer/setup all-targets
Clippy also passed with default features disabled.

Tests exercise native machine/thread readings, native process identity, mismatched identity
refusal, non-leader signal refusal, signalling/reaping a synthetic isolated sleep child,
synthetic Linux stat/PSI parsing, readdir rewind/errno behavior, and no-replace publication
preserving existing directories and dangling symlinks. CLI tests prove runtime commands
refuse without state creation or executing a synthetic touch command. No external peer,
phone or deployed state is used. Linux native execution and full supervisor/lease regression
coverage have NOT been run. Default-feature GUI code passed a Cargo metadata check;
a linked GUI binary and native window have not been validated.

## Next bounded work packages

1. **Admission backend.** Shared module boundaries now compile. Define and implement a
   macOS policy using genuinely supported signals, replacing the explicit Unsupported
   acquisition result. Do not populate synthetic PSI zeros or weaken the Linux gates.
   Files: `src/admission.rs`, `src/admission/native.rs`, `src/cache.rs`.
2. **Monitoring/health integration.** Feed macOS snapshots from `MachineSampler`; preserve
   the existing snapshot schema (`pressure: null` when unavailable). Replace Linux-only
   health thread priority/accounting and separate Linux lock parsing from native holder
   discovery. Unknown holder/lock state must not become a false "free". Coordinate shared
   platform API edits separately. Files: `src/metrics/`, `src/health/`.
3. **Execution and lease safety.** Implement macOS parent-death/supervisor behavior and
   read-only lock/descriptor inspection. Test daemon/worker crashes, escaped descendants,
   cancellation escalation, inherited leases and PID reuse before enabling local jobs.
   Do not simply remove prctl or use racy lock probes as a replacement for holder scans.
   Files: `src/worker.rs`, `src/lease_audit.rs`, admission holder backend.
4. **CLI/daemon/UI integration.** Wire the shared modules with target-selected primitives;
   explicitly reject unported Linux-only adapters. Validate isolated headless execution
   before the default-feature GUI build. Leave deployment and installed state untouched.

Parallel contributors should use distinct worktrees and agree exact file ownership;
`src/lib.rs`, Cargo manifests/lockfile and shared API definitions need one integration owner.
No subagent execution facility was available during this checkpoint.
