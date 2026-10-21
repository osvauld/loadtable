# macOS platform preparation (not a runnable macOS worker)

This is an independently useful first layer, **not completion of the macOS port**.
The coordinator binary and egui dashboard remain Linux-only in this change. Do
not widen the Linux module gates and assume that Unix sockets + flock make the
worker safe on Darwin. No native Mac was available for this work.

## Implemented boundary

`crates/platform` has no scheduler, network, SQLite, GUI or C-build dependency.
It can be Rust-type-checked for `aarch64-apple-darwin` with just Rust's Apple std
component. It provides:

- Native process-generation identity and liveness. Linux still reads field 22
  of `/proc/PID/stat`; macOS reads `PROC_PIDTBSDINFO` through libproc. On Darwin,
  the token is `pbi_start_tvsec * 1_000_000 + pbi_start_tvusec`, preserving
  subsecond precision. Invalid PIDs, short native reads, inconsistent data and
  arithmetic overflow are errors. Liveness checks identity AND zombie state
  from one snapshot, never `kill(pid, 0)` or a PID-only fallback.
- The existing serialized `ProcessIdentity { pid, start_ticks }` shape and public
  `execution::ProcessIdentity` path are retained. `start_ticks` is an opaque
  host-local generation token, not a portable duration. Linux values are
  unchanged; Darwin values must not be fed to the Linux tick-based sampler.
- Native memory headroom. Linux uses `MemAvailable`; macOS uses
  `(free_count - speculative_count + purgeable_count) * actual_page_size`, with
  total RAM from `hw.memsize` and VM counters from `host_statistics64`. The
  acquired Mach host send right is released. Darwin's conservative headroom
  excludes speculative, inactive, compressed and swapped pages. It is explicitly
  labeled `macos_free_and_purgeable`, not described as Linux MemAvailable. 16 KiB
  Apple Silicon pages are supported. Inconsistent counters fail closed.
- User-available disk space via `statvfs.f_bavail * f_frsize`, inspecting the
  nearest existing ancestor without creating it. Cache/state reserves still
  belong to admission policy. No APFS snapshot purge/reclaim is attempted.
- `ResourceSample::floor_blocker` checks memory/cache/state floors only. Passing
  those floors is **not execution admission**: pressure policy, ownership,
  approvals and leases remain mandatory separate checks. The default 8 GiB
  memory policy is not silently lowered for small Macs.
- Waited-for descendant usage via `getrusage(RUSAGE_CHILDREN)`. Darwin's maximum
  RSS is already bytes; Linux's is KiB. Neither is simultaneous process-tree RSS.
- A read-only `probe` example emits identity, resource sources, capabilities,
  child usage and `pressure: null`. It launches no worker or daemon.

The Linux worker now uses this identity reader, cache disk measurements use the
native disk helper, and the sampler uses the native child-usage helper. No
flock inode, descriptor-inheritance, cancellation, reconciliation, admission
threshold, persisted job, approval or socket protocol is deliberately changed.

## Unavailable is not zero

macOS has no Linux PSI. This layer does not synthesize a zero-pressure sample.
Its capabilities explicitly say `linux_psi: false`,
`noninvasive_flock_observation: false`, and `kernel_parent_death_signal: false`
on macOS. Those describe implementation/OS facilities, not the success of a
particular measurement; a missing Linux kernel feature can still fail a sample.
The existing full Linux admission policy still requires its real PSI files.

A future Darwin sampler must emit `Machine.pressure: null` and leave unavailable
CPU/job measurements unknown. It must not reuse the current `/proc` sampler or
legacy UI fallbacks: empty `/proc` inputs can otherwise turn unknown quantities
into zero CPU/RSS, an empty process tree or falsely free slots. Native memory
source information must remain visible when choosing/diagnosing floor policy.

## Safety blockers before enabling the worker

1. **Non-invasive lease observation.** `worker::{running,lock_held_by_anyone}`,
   `admission_holders`, `metrics/slots`, health and lease auditing inspect
   `/proc/locks` and/or `/proc/PID/fdinfo`. Darwin has neither. A PID sidecar cannot
   identify an escaped descendant retaining the lease. Do not use transient
   try-flock probes in the worker-start liveness path: the current worker can
   lose startup to that observer. Do not unlink/recreate lock files or signal
   unknown holders. Either implement a proven Darwin observer, or explicitly
   redesign startup handshaking and unknown-status reporting while keeping
   authoritative acquisition and descriptor leases intact.
2. **Parent-death cleanup.** `worker::stage` uses Linux `PR_SET_PDEATHSIG` for the
   admission child. A Darwin replacement must cover supervisor death BEFORE
   admission/command entry as well as during execution, without acting on a
   reused PID/group. Kqueue process-exit observation is a possible ingredient,
   not an implemented replacement. Keep identity verification, group ownership,
   unreaped-leader escalation (`waitid/WNOWAIT`) and inherited worker/target/slot
   descriptors through every cleanup path. Existing watchdogs alone are not a
   justification for removing the pre-exec protection.
3. **Detached helper descriptors.** Daemon startup in `src/bin/tasks/main.rs`
   uses `SYS_close_range/CLOSE_RANGE_CLOEXEC`. `device::without_inherited_fds`
   also uses that syscall, then a bounded fcntl loop. Darwin needs a safe,
   async-signal-safe close-on-exec strategy that cannot miss high-numbered lock
   descriptors or close Rust's spawn-error channel prematurely. Validate adb,
   compiler-cache server and daemon detachment; never lend them job leases.
4. **Native metrics and health.** `/proc` process trees, hostname/uptime,
   pressure and slot readers need real native implementations or explicit
   unavailable fields. `ru_maxrss` conversion is prepared, not the rest of the
   sampler. `health/disk.rs` additionally uses Linux `RUSAGE_THREAD`, gettid and
   ioprio syscalls; `health/procs.rs` assumes Linux device-number/proc layouts.
   `tmp_usage` assumes Linux filesystem magic values, not Darwin filesystem
   names. Lease auditing and GC's process/cwd checks must not infer idle/free
   from unreadable Darwin `/proc` paths.
5. **Module gates and build.** Linux gates remain in `src/lib.rs`,
   `local_command.rs`, `metrics/mod.rs`, `health/mod.rs` and related paths.
   Unguarded modules also refer to gated modules: a whole-package Apple check is
   not supported merely by the new crate. Native C dependencies (bundled SQLite,
   crypto/network dependencies) and egui/winit need a real Apple SDK and linker
   for a linked application. No SDK was downloaded, forged or licensed here.
6. **Command environment.** Audit Darwin developer-tool selection and SDK
   discovery (`DEVELOPER_DIR`, `SDKROOT`, compiler/archiver selection) against the
   worker's environment allowlist. Do not blindly pass the submitting client's
   environment, migrate existing cache roots, or override the host's cache
   policy. Parent owns peer configuration/placement; setup owns setup UI/CLI.

## Validation and native acceptance

SDK-free cross-check of the primitive layer (all library/test/example Rust code,
NOT linking or executing macOS code):

```sh
tasks run --pin --who macos-platform-std -- rustup target add aarch64-apple-darwin
tasks run --pin --who macos-platform-check -- \
  cargo check --offline --locked -j 2 -p task-manager-platform \
  --all-targets --target aarch64-apple-darwin
```

Use `--pin` for host-specific checks/installations: installed placement/reroute
can otherwise move commands and even target installation to the LAN worker.

On a real Apple Silicon Mac with the user's licensed developer tools, run the
platform tests and read-only probe under THAT machine's admission:

```sh
tasks run --pin --who macos-platform-native -- \
  cargo test --locked -j 2 -p task-manager-platform --lib
tasks run --pin --who macos-platform-probe -- \
  cargo run --locked -j 2 -p task-manager-platform --example probe -- \
  /absolute/cache/path /absolute/state/path
```

Then validate the future complete port with isolated state and synthetic jobs:

- Live/zombie/exited identity, stale generation rejection, high PID churn;
  native `PROC_PIDTBSDINFO` error/permission behavior and linker symbols.
- Real VM counters, 16 KiB pages, pressure/null semantics, reserve boundaries,
  APFS available space and counter failures; memory floors are not PSI parity.
- Startup observer races; concurrent starters; worker, supervisor and admission
  SIGKILL at each startup/exec boundary; TERM-to-KILL escalation with stubborn
  children; no unrelated group signaled after leader exit/PID reuse.
- Descendants that fork/setsid and retain inherited locks; worker/target/slot
  descriptors must retain their stable original inode and never be stolen.
- Detached helpers must have NO inherited lease, including high FD numbers.
- Daemon restart/reconciliation, cancellation under a busy ledger, approval
  compatibility and old SQLite/socket/job JSON compatibility.
- Linked headless worker and default-feature egui build; actual native window,
  hidden-window polling, remote snapshot rendering and job lifecycle. A Rust
  metadata check, UI fixture or Linux test cannot substitute for these.

Exact checks/results for this preparation are recorded in
`feedback/macos-platform-3888-native-primitives.md` and the agent handoff. No
native macOS worker/UI, cancellation, lease, linking or resource-admission
acceptance is claimed by this preparation.
