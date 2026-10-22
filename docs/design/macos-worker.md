# macOS worker: first-machine readiness plan

Status: preparation in progress. Branch `feature/macos-worker`, based on main fd8d016.
Native primitives, read-only setup/egui onboarding, opt-in relay configuration and
packaging/simulator fixtures are being integrated; the full macOS worker remains
blocked on the safety work in `macos-platform.md`. No Mac has been available; no
macOS binary, installation, cross-network connection, Xcode build or iOS app has
been validated. Proposed work below is not a promise of next-day completion.

## Goal

A consenting Mac owner installs the worker, reviews the tools and commands needed
for a project, pairs with a Linux controller over iroh, and approves a workload.
The controller transfers source, receives logs and exit status, and retrieves
artifacts. Neither SSH nor a shared LAN is a product requirement.

Preserve existing SQLite/socket compatibility, peer identity checks, approval
semantics, stable flock inodes, job history, cancellation and descriptor leases.
Do not change the installed Linux release or any running daemon to test this work.

## Verified baseline and gaps

- The daemon-to-daemon protocols already support source sync, idempotent job
  submission, log attachment, cancellation, reconnect and artifact return:
  `crates/peer`, `src/peer`, `docs/design/peer-link.md`.
- `crates/peer/src/lib.rs::lan_endpoint` explicitly disables relays, address
  lookup and port mapping, and binds one explicit address. Existing peer config
  is address-based. An endpoint ID alone does not currently reach an arbitrary
  friend's Mac behind NAT.
- LAN-only transport is deliberate current policy, not an accidental missing
  flag. Add a separately reviewed, explicit internet mode; preserve LAN defaults
  and LAN regression tests. Do not replace the default endpoint preset silently.
- `src/lib.rs` gates admission, worker, peer runtime, cache and other modules on
  Linux. Worker process identity and lock observation use `/proc`; child cleanup
  uses `prctl(PR_SET_PDEATHSIG)`. Merely widening cfg attributes is unsafe.
- Existing tool/capability/recipe policy is partly design, not a complete setup
  engine: `docs/design/policy-file.md`, `node-setup-policy.md`. Existing peer
  `approve = "auto"` is not suitable as a surprise public-onboarding default.
- The Apple Silicon Rust std target was subsequently installed and the platform
  primitive crate cross-type-checked. This does not supply an Apple SDK, linker or
  Swift compiler, nor enable linking/checking the entire coordinator on macOS.
- A shallow read-only inventory of `/home/abe/p2p_chat` found no iOS/Swift project,
  but subsequent worktree discovery found an active separate agent's iOS work at
  `/home/abe/p2p_chat/.claude/worktrees/ios-app/ios`. Do not overwrite that work.
  Coordinate its exact build recipe/tool requirements instead of inventing a second
  app. Coordinator work does not supply an Apple media backend/signing entitlement.

## Work that can be prepared and tested on Linux

### 1. Platform seam, then compilation

Inventory all Linux assumptions. Separate OS-independent ledger, policy, transfer
and protocol code from platform implementations for process identity, memory/disk
admission, process-group ownership, cleanup, environment and paths.

Do not report missing macOS pressure data as zero or mark Linux-only metrics as
healthy. Use explicit unavailable/unsupported fields and native bounded probes.
Fail closed when safety-critical admission or process ownership cannot be checked.
Preserve PID-reuse protection and lease inheritance; cancellation must only affect
the owned job group. Orphan/restart handling needs native tests, not a Linux model.

Prepare headless and UI build recipes for x86_64-apple-darwin and
aarch64-apple-darwin. Cross-check what can be checked without Apple tools, but label
it accurately: Linux fixture tests and source/type checks are not Mac acceptance.
A runnable first binary must come from a real Mac or an explicitly approved macOS
CI runner; neither is currently provisioned. Keep the initial source-bootstrap
path available if no prebuilt artifact can be produced before the friend joins.

### 2. Declarative setup plan

Use the existing HUML design rather than introducing a second policy language.
A project declares requirements and workload argv; the machine policy remains the
ceiling. A remote config is untrusted data, never permission to run commands.

Prepare strict versioned plan/report types and fixture tests for:

- OS/architecture, Rust version/targets, selected developer directory, full Xcode
  versus Command Line Tools, available SDKs and simulator runtimes;
- required/optional tools with installed/missing/unsupported/unknown states;
- rendered exact argv, source, destinations, network access and privilege needs;
- manual Apple installation/license steps, without automatically accepting terms;
- unknown keys/actions, incompatible hosts, stale probes and changed plan rejection;
- no command execution just from receiving, parsing or previewing a plan.

Start with Rust and Apple tooling. CMake/Ninja, CocoaPods, Node, Flutter and binding
or codec tools are project requirements, not a global mandatory bundle. Do not run
incoming project-supplied "probe" commands automatically: probes also execute code.

Approval must bind to the exact plan, requester and version/digest. Any change
requires fresh consent; decline, disconnect, timeout and restart must not turn into
approval. Use a curated action set for initial installation guidance. Custom argv
must be clearly labeled as arbitrary code. Do not request admin/root by default.

### 3. Owner UI and peer delivery

Build a small first-run/status UI using the existing egui stack initially, rather
than making a native Swift coordinator UI a prerequisite:

- local identity, paired peer identity, connect/revoke;
- detected prerequisites and rendered proposed setup actions;
- separate setup and project-code execution approvals;
- active job/logs, cancel and pause new work;
- explicit signing/capture/control permissions, disabled for initial build-only use.

Sending setup plans over iroh requires an explicit bounded protocol operation and
permission, not smuggling an installer through an auto-approved build job. Test
unknown/observer peers, role mismatch, oversized plans and old-peer incompatibility.
Bound stdout, runtime, input/output sizes and artifact paths. Retain idempotent
submission and UNCERTAIN/reconnect behavior; never replay setup or a build silently.

Pairing means allowing trusted code execution under that account unless stronger
isolation is implemented. A source snapshot is not a sandbox. Recommend a dedicated
standard macOS account and do not describe a directory choice as filesystem isolation.

### 4. Opt-in internet connectivity

Design explicit transport mode, permitted relay configuration and endpoint address
exchange/pairing. Review relay metadata/privacy, endpoint allowlisting, invite expiry
and revocation before adding public discovery. Reuse the existing authenticated
protocol; do not create an SSH fallback or weaken LAN tests.

Linux tests can exercise config refusal/defaults and controlled loopback protocol
fixtures. Actual NAT traversal, relay fallback and network interruption need two
real networks and explicit owner consent. Existing LAN-only design must be amended
explicitly before claiming this mode is supported.

### 5. Packaging and handover

Prepare an artifact manifest with architecture, version/source revision, hash,
minimum supported macOS and build provenance. No package may contain peer keys,
Apple credentials or personal project data. Do not disable Gatekeeper/SIP or advise
blanket quarantine removal. For public distribution plan signing/notarization;
local unsigned testing has owner-reviewed macOS trust prompts and is not a public
release. Confirm OS/Xcode compatibility before choosing the minimum OS version.

An Intel binary is not a substitute for an Apple Silicon binary. Publish explicit
architectures first; universal bundling and native tests can follow. No new
launch agent or background persistence without local opt-in; first tests can run
in the foreground. Keep all logs/build outputs outside Git.

## Tomorrow: request these facts from the Mac owner

- Intel versus Apple Silicon, macOS version, RAM and available disk space.
- Whether full Xcode is installed, its version, and whether it can launch normally.
- Whether a separate standard account is possible and installing Rust is acceptable.
- Whether their network permits the agreed iroh/relay connectivity.
- Whether the goal is simulator testing, a personal phone, or distribution.

Full Xcode and simulator runtimes can be large downloads. Have the owner install
and launch Xcode ahead of the session if possible. The owner accepts any Apple
license/setup dialogs locally. Do not ask for Apple ID credentials or signing keys.

## First-Mac acceptance ladder (stop at the first failure)

1. Build/install the worker natively, or verify an approved matching artifact;
   print version/architecture; use disposable state, not a Linux production state.
2. Local foreground daemon: read-only preflight; allowlisted `/usr/bin/printf` job;
   exact exit status/log; deliberate failure and cancellation; restart/reconnect
   and lease behavior. Observe actual Mac resource admission.
3. Pair on iroh using explicit consent; refuse unknown endpoints. On the real
   remote network verify direct or permitted relay mode, with no SSH dependency.
4. Transfer a tiny synthetic Rust project. Approve, compile, retrieve one artifact,
   verify hashes, then repeat a disconnected run without duplicate execution.
5. Build a small Rust static library for the matching iOS simulator target:
   `aarch64-apple-ios-sim` on Apple Silicon or `x86_64-apple-ios` on Intel, subject
   to the selected Xcode/runtime supporting that simulator architecture.
6. Integrate a minimal Swift/SwiftUI simulator shell with a narrow Rust FFI. This
   requires app-repository authorization and its own review. First prove a trivial
   Rust function, not the entire media/calling stack.
7. Inventory p2p_chat core dependencies, native codecs, networking/storage and
   platform APIs. Select a minimal supported feature set, build it, then integrate
   it into the shell. No claim that Android/desktop bindings work unchanged.
8. Only after simulator success: device target `aarch64-apple-ios`, entitlements,
   signing and approved device testing. Personal-device provisioning differs from
   TestFlight/App Store distribution; neither is authorized by a build request.

Screenshots and UI control are a later consented phase; they require a graphical
session/simulator as applicable and do not block the first compile-and-fetch loop.

## Definition of the initial milestone

One owner-approved remote synthetic Rust build on a real Mac, source/log/artifact
transfer over iroh across the actual networks, no SSH, correct failure/cancel/
reconnect semantics, and an honest prerequisite report. iOS simulator smoke build
is the next gate. Full p2p_chat iOS calling, public distribution, unattended install,
network universality and production isolation are not initial acceptance claims.
