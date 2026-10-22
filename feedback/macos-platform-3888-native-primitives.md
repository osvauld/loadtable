# Trial feedback: native platform preparation and host-specific placement

- Agent/session: macos-platform, feature/macos-platform.
- Job IDs: 3886, 3887, 3888, 3889, 3893, 3894, 3901; final checks recorded below.
- Outcome: friction (host selection), otherwise admission behaved as documented.
- Commands/cwd/resource: dedicated task-manager worktree; Rust target install,
  primitive Cargo library tests/Apple checks and targeted Linux library tests;
  compile slots, plus automatically selected/rerouted offload slots. Every
  submitted Cargo command explicitly used `-j 2`.
- Exercised: real installed enqueue/admission, queueing, cancellation, logs,
  target installation and Linux execution; Apple Rust type-checking is not native
  execution. No runtime deployment, service restart, SDK acquisition or phone use.
- Expected: host-specific target installation and native checks run on the
  machine where requested, without silently changing the Cargo job count.
- Observed:
  - Job 3887's unpinned Cargo test was placed on the LAN peer; placement replaced
    explicit `-j 2` with the peer cap of six. Its log named remote job 1246 waiting
    on memory PSI. Cancelled own local job (terminal cancelled, exit 2), then
    resubmitted locally with `--pin` only after it was terminal. No result from
    that offload is counted as validation.
  - Job 3886 (`rustup target add aarch64-apple-darwin`) initially waited locally,
    then was rerouted to the peer as remote job 1247. The remote installation
    completed (exit 0) before my cancel request; local Apple std was still absent.
    Job 3889 pinned the local installation and passed. No remote toolchain
    cleanup was attempted. This was not an SDK install or a tasks upgrade.
  - Job 3888 pinned the primitive test locally: 14 passed, 0 failed/ignored.
  - Job 3893's real memory PSI blockers were logged before admission; after
    admission, 16 primitive tests passed, then the Apple metadata check exposed
    a missing libc binding for `mach_port_deallocate`. Fixed using its small
    libSystem ABI declaration, without fabricating an SDK or changing gates.
  - Job 3894 targeted Linux regression passed (exit 0); exact target counts and
    final primitive/Apple-check results are recorded below.
- Impact: unpinned host-specific preparation can modify/check the wrong host;
  also the submitted explicit Cargo parallelism was rewritten by placement.
  Source remained stable while admitted commands ran. Shared memory/slot gates
  were respected, not lowered or bypassed.
- Suggested improvement: refuse automatic relocation/rerouting of toolchain
  mutation commands such as `rustup target add`, or require explicit anywhere/
  named-peer intent. Document `--pin` prominently for native validation and
  explain whether an explicit `-j` can be rewritten by placement.
- Acceptance test: enqueue synthetic `rustup target add` argv while a worker peer
  is available and local admission is busy; verify it remains pinned or requires
  explicit remote intent, and the stored argv/host in progress and final reports
  make any parallelism rewriting unambiguous. Do not mutate a real toolchain in
  that policy test.
- Workaround: explicit supported `--pin`; no lock bypass, pressure override or
  service change requested. The rerouted target installation had already passed
  when cancellation was requested; it was not undone.

## Final validation

- Job 3901 passed (exit 0), all commands offline/locked with explicit `-j 2`:
  - Platform library tests: **16 passed, 0 failed, 0 ignored**. These include
    real Linux identity/zombie/resource reads plus synthetic Darwin token,
    4/16 KiB memory arithmetic and RSS-unit cases, not native Darwin execution.
  - `cargo check -p task-manager-platform --all-targets --target
    aarch64-apple-darwin`: passed. Rust library/tests/example metadata only;
    no native linker, SDK, binary or Mac execution.
  - Platform all-target Clippy with `-D warnings`: passed for Linux and
    `aarch64-apple-darwin`.
  - Read-only Linux probe example: passed; explicit native resource source,
    identity and `pressure: null`, no daemon or job launch.
- Job 3894 passed (exit 0), all root tests `--no-default-features --lib`,
  offline/locked, explicit `-j 2`:
  - `worker::tests`: **14 passed**, including owned-group cancellation,
    unreaped-leader escalation, stale identity and non-invasive lock watch.
  - `cache::tests`: **7 passed**.
  - `admission::`: **50 passed**.
  - `metrics::tests`: **31 passed, 2 ignored**. Existing ignores are
    `fixture_tests::write_fixtures` (writes deliberate fixtures) and
    `sampler_cost` (opt-in host measurement). Neither was run.
- Total final executed Rust tests: **118 passed**, 0 failed, 2 intentionally
  ignored; no E2E or production/native integration acceptance implied.
- Owned-file rustfmt and `git diff --check`: passed.

Full logs remain outside Git under `/tmp/macos-platform-*`. Native macOS
linking/execution/worker/UI/lease/cancellation validation remains unavailable;
see `docs/design/macos-platform.md` for the complete blocker list.
