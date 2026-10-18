# Bounded local milestone — implementation contract

## Scope / target-placement blocker (recorded before validation)

No Cargo builds will be submitted in this slice. The populated nested
`experiments/osvauld-foundation/target` remains untouched. Installed
`local_command::place_outputs` keys only the Git root, not a nested independent
Cargo workspace; cwd alone is not a fix. A future separately reviewed change
must identify the actual workspace without running Cargo/toolchain shims,
namespace by canonical checkout + workspace, preserve every existing output,
and test direct/compound commands and cross-checkout isolation. No deployment
or cache-policy environment override is authorized. Until then further heavy
Cargo validation here is blocked. Python stdlib fixture tests need no target.

## Test-first acceptance

- Inspector performs bounded metadata reads only: no child tool invocation,
  networking, writes, privilege/device operations or toolchain shim execution.
  Installed tools are executable-path evidence, versions explicitly unknown.
- Initial planning scope: Ubuntu 24.04, Debian 12 and NixOS (reported
  VERSION_ID recorded; target worker is NixOS x86_64), x86_64/aarch64 only.
  NixOS adds read-only live-ISO/store/current-system/nix-ld facts; its plan
  points at the DRAFT declarative module in `nix/` (never applied) and blocks
  on live-ISO (non-persistent) or unknown persistence.
  Other platforms still return an inspection with an unsupported reason and
  nonzero status. This is inspector support, NOT installation/build support.
- JSON inspect/plan distinguish facts from unvalidated GPU/emulator readiness.
  Missing KVM access is a blocker, not a chmod/group command. Apply always fails.
  No package pins/checksums/download sizes are invented: unknowns block apply.
- Capture requires an explicit bounded list of individual relative file paths;
  it never scans a checkout, Git index, home directory or credentials. The caller
  selects permitted dirty/untracked inputs, including in an unborn/no-Git tree.
  No implicit directory recursion/globs. Known credential/VCS/tool-cache names
  are rejected anywhere; build-output names (`target`, `build`, `dist`, `out`,
  `outputs`, `sdk`, `ndk`, `cache`, `caches`) only at the root's top level or,
  for `target`/`build`, beside a Cargo/Gradle marker (stat-only, no follow).
  Exact rule: scripts/README-linux-worker.md. Content review is still required.
- Normalize by rejection (no absolute, dot/dotdot, duplicate, backslash, control,
  non-UTF-8 or overlong paths). The root is resolved once via realpath (must be
  an existing absolute directory; never recorded in manifest/receipt), then
  descriptor-relative no-follow opens of every component below it prevent link
  traversal; reject links, hardlinks and special files. Bound file/count/
  aggregate bytes and depth: conservative defaults, opt-in up to a fixed
  ceiling (20000 files, 64 MiB/file, 512 MiB total). Capture is in-memory;
  large captures need the future blob store. Double inventories detect
  ordinary mutation; not an atomic filesystem snapshot or hostile-UID sandbox.
- Retain bytes in immutable in-memory values. Transfer planning and retrieval
  use only captured bytes, never a later workspace reread. No disk blob store,
  persistence, materialization, wire protocol, execution or authorization added.
- SHA-256 file digests and a versioned manifest receipt use explicit labels;
  they are NOT future iroh-blobs (BLAKE3) hashes. Each file entry carries
  `iroh_blobs_hash: null` for a future transport to fill; it is outside the
  receipt, whose framing/golden vector are unchanged (schema not bumped). Existing Rust receipt formats stay
  unchanged. Reuse the existing workspace snapshot's path/mode/length/content
  digest framing and two-inventory pattern, with a distinct schema/domain.
  The closed Rust adapters cannot be called here: they require HEAD and fixed
  Tinline manifests/layout, and cannot express this explicit no-HEAD file set.
- Diff detects edits/modes/deletions and represents renames as removal+addition.
  Missing-blob planning deduplicates content, rechecks current possession on
  each plan, and requires the exact receipt. Possession input is a local fixture
  assertion, NOT authenticated peer evidence. Hash knowledge grants no access.

## Implemented files and validation

- `scripts/setup-linux-worker.sh`, `scripts/linux_worker_setup.py`: metadata-only
  JSON inspect/plan, gated apply. `scripts/worker_source.py`: retained-byte local
  capture/diff/missing contract. `scripts/README-linux-worker.md`: interface use.
- `tests/test_linux_worker_contracts.py`: 14 synthetic acceptance tests.
- Shared job 37 was red before modules existed; 38 passed the first 10; 40 was
  red for five newly tested credential/state-name exclusions; final job 41
  passed all 18 discovered tests (14 new + 4 existing), 0 failed/skipped.
- Job 42 passed real-host CLI assertions: EndeavourOS x86_64 is unsupported;
  inspect/plan/apply each exited 2 as designed. Client timeout during real PSI
  waiting was reconciled via the printed ID, with no retry or cancellation.
- Shell syntax, Python AST/owned whitespace checks passed. All 60 original
  baseline files still match the pre-foundation SHA-256 inventory.
- No Rust build/test/clippy, supported-distro real install/build, emulator/GPU
  run, remote transfer or deployment acceptance was performed. The nested
  target-placement blocker above remains unresolved and outputs untouched.

## Review-fix pass (owner `osv-worker-source`)

Test-first fixes for review findings: context-sensitive output-name denial,
raised opt-in ceiling, realpath-resolved root, per-file null iroh-blobs hash,
affinity-based/injectable CPU count and read-only cgroup v2 cpu.max/memory.max
(tightest ancestor; null for max/absent/malformed). Jobs: 50 red (27 run, 2
failures, 13 errors, as intended), 52 green (27 passed), 53 real-host inspect
smoke (EndeavourOS unsupported, exit 2 as designed), 54 final: 27 passed
(23 contracts incl. 9 new + 4 wait-budget), 0 failed/skipped. Intentional test
changes: a symlinked root is now accepted; `nested/build/a` without a marker is
now source (replaced by `build/a` plus marker-based denial tests).

## NixOS pass (owner `osv-worker-source`)

Jobs: 93 red (32 run, 2 failures, 3 errors), 96 red on a test bug (aarch64
loader path; test corrected, implementation unchanged), 97 green (32 passed),
98 real-host inspect/plan smoke (EndeavourOS unsupported, `nixos` null, exit 2),
99 final: 32 passed (28 contracts + 4 wait-budget), 0 failed/skipped.
`nix/flake.nix`, `nix/worker-module.nix` were drafts at the time (Nix absent on the
authoring host; unvalidated). They are now retired in favour of the root `flake.nix` and
`nix/module.nix` (`services.tasks-worker`), see nix/README.md.

## Future boundary

Review/pin packages and worker provenance before enabling any setup. Implement
actual authenticated LAN-constrained iroh/iroh-blobs transfer before claiming
remote readiness. Approval must additionally bind execution spec and both
installation identities; this local receipt alone never authorizes execution.
