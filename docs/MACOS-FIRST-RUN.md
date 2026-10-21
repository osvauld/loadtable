# Apple Silicon worker: first-run handoff (UNVERIFIED TEMPLATE)

## Status and authorization boundary

No Mac, Apple SDK, CI run, binary build or simulator execution was available for
this handoff. The starting revision `fd8d016a6b539c79a27cd92323e033d6526d90c9`
is Linux-only. Native platform primitives and explicit opt-in relay configuration
are now integrated on `feature/macos-worker`, but the full macOS worker remains
blocked on `docs/design/macos-platform.md`. **Do not treat this as a working Mac
build-worker release.** Review the port and record actual native results first.

Owned deliverables:

- `.github/workflows/macos-worker.yml`: **manual `workflow_dispatch` only**,
  acknowledgement required; no push/PR trigger, release or deployment.
- `tests/fixtures/ios-smoke/`: dependency-free, synthetic UIKit build fixture;
  no connection to p2p_chat, no app integration or device acceptance claim.
- `tests/test_macos_fixture.py`: Linux structural checks, not Xcode validation.

This handoff authorizes no repository publication, hosted CI dispatch, remote
machine access, runtime replacement, SDK installation or service restart.
Future owner action/consent is required for each of those. No root worker,
signing identity, provisioning profile, Apple credentials, secrets or
notarization service is required or configured. Apple toolchains may emit
implicit linker metadata/ad-hoc signatures; that is not Developer ID signing,
notarization or a trust guarantee.

## Standalone setup app versus the worker

`crates/setup` is deliberately independent of the Linux coordinator. Its binary
`tasks-mac-setup` can assess tools and render inert setup plans; it CANNOT pair,
start a daemon, install tools, approve jobs or run remote builds. Default UI builds
open an egui window; probes require a separate local button click. No-UI builds
print an unprobed plan when invoked without arguments.

Source recipes (not evidence of native builds):

```sh
# On an authorized real Mac/runner, under its applicable build admission:
cargo build --locked -j 2 -p task-manager-setup --features ui --bin tasks-mac-setup
# Explicit local inspection, not an installer:
./target/debug/tasks-mac-setup --help
./target/debug/tasks-mac-setup --requirements docs/examples/apple-silicon-ios-requirements.json --probe --json
```

The integrated coordinator exposes the same options as `tasks setup`; it does not
contact a daemon. `--plan FILE` renders untrusted received JSON without running it.
The JSON requirements/plan schema is a bounded wire/display document, NOT a second
machine-policy language or an implemented HUML recipe loader. A digest binds exact
bytes only, not identity/approval/freshness. There is no setup-plan iroh operation
or remote install engine yet. The full config-send/owner-approval workflow remains
future work; source-file transfer alone does not authorize executing a setup plan.

Manual CI dispatch now builds this standalone app in a separate `setup` job and
wraps it as `Task Manager Setup.app` in a ZIP, recording revision/tool versions,
linked libraries and SHA256. This is still an unsigned, unnotarized, unexecuted
packaging template. The `build_worker` input defaults false: leave it false until
the full port is completed and reviewed. The native egui window itself is not
validated by a Linux UI fixture or SDK-free Rust check.

The sample requirements cover Apple Silicon Rust targets and the presence of
Xcode/iOS SDKs. They do not prove a particular project's pinned Rust/Xcode version
or latest Xcode compatibility with macOS 13. Review the actual project manifest.
A read-only inventory found another agent's speculative iOS work at
`/home/abe/p2p_chat/.claude/worktrees/ios-app/ios`; do not overwrite it. At inspection
its app recipe additionally needs **XcodeGen**, and the repository pins Rust
**1.99.0** with Android targets in its toolchain declaration. Coordinate a reviewed
Mac-specific recipe with that agent; do not silently install those Android targets,
run project setup scripts, or change their parallelism/output handling here.
No p2p_chat build, Swift binding or codec/platform integration was validated.

## 1. Owner prepares the Mac

1. Use a dedicated **standard, non-admin account** on an Apple Silicon Mac.
   Jobs execute with that account's rights; admission is not a sandbox. Keep
   personal files, keychains, credentials and other projects out of the account.
   Grant only the intended checkout/output directories; no root daemon or sudo
   in worker jobs, no blanket Full Disk Access or firewall disablement.
2. The owner installs Xcode from an approved source, launches it interactively,
   reviews/accepts Apple's terms, completes first-launch setup, and chooses the
   intended developer directory. The owner also supplies an iOS Simulator SDK
   for the optional fixture. Stop if tools request setup or licence consent;
   never autoaccept terms, install SDKs or run downloaded installers in jobs.
3. Check (read-only): `uname -sm`, `sw_vers`, `xcode-select -p`,
   `xcodebuild -version`, `xcrun --sdk macosx --show-sdk-version`, and optionally
   `xcrun --sdk iphonesimulator --show-sdk-version`. Expect Darwin/arm64.
   Record the exact macOS, Xcode and SDK versions; don't silently switch Xcodes.
4. Rust/Cargo supporting edition 2024 and target `aarch64-apple-darwin` must
   already be provisioned by the owner. Record `rustc -Vv`, `cargo -V`, and
   `rustup target list --installed`. This template installs no toolchains.
5. Review the merged port's pressure/admission implementation, process-group
   cancellation, stable descriptor/flock leases, state/socket compatibility,
   and paths. Linux PSI and `/proc` behavior must not be claimed on macOS.
   Agree on worker capacity and cache placement before running builds. Preserve
   existing cache policy; do not override `RUSTC_WRAPPER`, `CARGO_TARGET_DIR`,
   or `GRADLE_USER_HOME`, share targets or remove existing outputs.

## 2. Generate worker artifacts on a real runner (pending)

Only after publication/CI access and dispatch are explicitly authorized, an
owner may enable the workflow for the reviewed revision. GitHub manual dispatch
may require the workflow on the default branch; putting this file on a feature
branch alone does not prove it is dispatchable. This local repository has no
published CI setup. Owner must verify action policy and runner-label availability.

The template requests `macos-15` and **asserts native arm64**, failing on Intel.
Runner-label mappings can change; owner must verify an Apple Silicon label for
their repository before dispatch (do not silently select a paid runner).
The jobs use preinstalled Rust/Xcode only. Checkout and artifact actions are
SHA-pinned; the token is contents-read-only, not persisted. No secrets are used.
Cargo may download locked dependencies; this is not a hermetic/offline build.
No installer is downloaded/executed. Review dependencies/build scripts as code.

The isolated, ephemeral CI runner compiles directly with Cargo because the
installed Linux `tasks` runtime cannot bootstrap a not-yet-built macOS worker.
This is not permission to bypass admission on a shared machine. On a shared Mac,
use its **installed, verified native** `tasks`, inspect `tasks list` and
`tasks admission` first, and submit builds through it. Do not nest internally
admitted helpers or install/restart a daemon just to run this template.

The worker job builds, sequentially, release `tasks` for
`aarch64-apple-darwin` with `-j 2`:

- `--no-default-features`: copied as `tasks-headless` (no UI/MCP features).
- Default features: copied as `tasks-ui` (same CLI plus UI and MCP).

Copies are made before the next variant replaces the Cargo output. Neither
binary is launched by CI; compilation would not validate macOS admission/UI or
network behavior. The port may still fail to compile. A separate optional
`ios-smoke` job can build the fixture independently of worker-build success.

Successful worker output would be a seven-day CI artifact containing
`tasks-macos-aarch64.zip` and `SHA256SUMS`. The ZIP includes both executables,
`BUILD-INFO.txt` (source revision, worktree status, OS/Rust/Xcode/SDK versions),
`LINKED-LIBRARIES.txt`, and this handoff. The ZIP is made with
`ditto -c -k --keepParent` to preserve modes and symlinks, not by uploading a
raw bundle tree. The revision describes checked-out source, not a reproducible
build attestation. SHA256 detects corruption, **not publisher authenticity**.
There is no app wrapper, DMG, pkg, signing, notarization, release or installer.

## 3. Inspect and evaluate locally (pending owner acceptance)

- Obtain archive and checksum from the same owner-approved run/revision via a
  trusted channel. In the download directory, `shasum -a 256 -c SHA256SUMS`
  verifies bytes only. Review archive paths (`unzip -l`) before extracting to a
  new empty directory with `ditto -x -k ARCHIVE.zip DESTINATION`.
- Confirm provenance/revision, executable architecture (`file`) and linked
  libraries (`otool -L`). Do not overwrite an installed `tasks` or its state.
  Unsigned binaries are experimental; keep quarantine and Gatekeeper enabled.
  If macOS refuses execution, stop and have the owner review normal OS security
  policy; do not strip quarantine, disable Gatekeeper, re-sign as a workaround,
  or copy the binary into a trusted location to evade assessment.
- After trust review, verify the built binary's help and native CLI in an
  explicitly isolated test state/account before pairing or shared operation.
  `tasks-headless` is the compute variant; the default-feature executable can
  open a UI if called with no arguments. No launchd/service installation is
  provided. An isolated test is not permission to restart a busy shared daemon.

## 4. Pairing and network acceptance: parent integration required

The starting source has **LAN-only** iroh networking: an explicit local bind,
no relay, no public discovery. Existing source syntax is
`tasks peer init --bind IP:PORT`, `tasks peer id`, and
`tasks peer add NAME ENDPOINT_ID ADDR --role ROLE --approve manual`
(with optional `--config PATH`). See `docs/design/peer-link.md` and verify the
actual integrated binary's help before using even this syntax on macOS.
These are not completed Internet pairing instructions.

The source branch adds **explicit relay mode with an owner-configured HTTPS
relay URL**, no public discovery, and manual approvals by default in relay mode.
See [peer-relay.md](design/peer-relay.md) for `tasks peer init --relay HTTPS_URL`
and `tasks peer add NAME ENDPOINT_ID - ...`. These are coordinator commands, NOT
commands provided by the standalone setup app. They do not make the Linux-only
worker run on macOS. No relay was provisioned/contacted and no real remote-network
acceptance was performed. Do not choose a public default relay, publish endpoint
identities or open broad inbound access. Endpoint allowlisting is not account
identity attestation, and a relay does not sandbox jobs.

Before authorizing a host peer, the owner must verify endpoint IDs over a trusted
channel, apply least-privilege roles on both sides (a `host`/`both` peer can
submit arbitrary commands as the worker account), protect the private key and
confirm effective manual approval. Explicitly review each remote command and
checkout; `tasks approve-command REMOTE_ID` and `tasks start-worker` on the
receiving worker are the existing approval controls, subject to CLI verification.
Do not default to `approve=auto`. Never put keys/secrets in argv, CI or artifacts.

Parent/operator acceptance must demonstrate:

- Native headless startup and socket permissions; UI startup separately in an
  interactive account; no accidental production state migration/replacement.
- Allowlisted host only; unknown endpoint and role mismatch refused; no public
  discovery; relay traffic uses only the configured HTTPS relay service.
- Relay job waits for receiving-owner approval, then executes once; its local
  and remote IDs, logs, exit status and fetch result agree.
- Lost connection is uncertain/re-attached by ID, not resubmitted; cancellation
  cleans up only the job's process group; descriptor leases remain intact.
- Worker admission respects bounded concurrency, free memory/disk and cache
  policy without pretending Linux PSI is available. No busy-daemon restart.

## 5. Small iOS simulator build smoke (UNEXECUTED)

`tests/fixtures/ios-smoke/IOSSmoke.xcodeproj` has one shared `IOSSmoke` scheme,
one Swift UIKit source, an explicit plist and synthetic bundle identifier
`org.example.taskmanager.iossmoke`. No packages, entitlements, build scripts in
the Xcode project, assets to download or signing team. It builds a simulator app,
**not a device IPA**, and does not prove launch, tests, application integration,
p2p_chat compatibility or App Store readiness.

The build helper `bash tests/fixtures/ios-smoke/build-simulator.sh` checks native
Apple Silicon and uses this command from the fixture directory:

```sh
xcodebuild -project IOSSmoke.xcodeproj -scheme IOSSmoke \
  -configuration Debug -sdk iphonesimulator \
  -destination 'generic/platform=iOS Simulator' \
  -derivedDataPath "$PWD/out/DerivedData" -jobs 2 \
  CODE_SIGNING_ALLOWED=NO ARCHS=arm64 ONLY_ACTIVE_ARCH=YES build
```

`out/` is **inside this checkout's fixture directory** and ignored there; it is
not a shared target or an external path. No `clean`, simulator boot/install,
physical device access or runtime download is requested. On a shared Mac with a
verified installed native manager, run the helper as a compile job:

```sh
tasks list
tasks admission
tasks run --who macos-ios-smoke -- bash tests/fixtures/ios-smoke/build-simulator.sh
```

These commands are a pending operator recipe, not evidence of a Mac job. The
helper archives the built app with `ditto -c -k --keepParent` and records tool
versions/revision plus SHA256. Fetch only normalized checkout-relative paths:

```text
tests/fixtures/ios-smoke/out/IOSSmoke.app.zip
tests/fixtures/ios-smoke/out/SHA256SUMS
tests/fixtures/ios-smoke/out/BUILD-INFO.txt
```

Once the parent verifies peer CLI/pairing, those are the values for repeated
`--fetch REL` on a forwarded build job. Do not fetch `.app`/`.framework` trees
directly: a requested symlink is refused; directory traversal skips symlinks and
special files, breaking bundle structure. Package on the Mac before transfer;
ZIP (ditto) or tar preserves symlinks. Do not follow artifact symlinks into
personal files when inspecting/extracting, and keep archives outside Git.

### Actual transfer bounds checked at the starting revision

`crates/peer/src/source.rs::Limits::default()` is **64 MiB/file, 512 MiB total**
for source sync (20,000 files, depth 32). However, artifact serving and receiving
in `crates/peer/src/artifact.rs` explicitly use `Limits::artifact()`:
**2 GiB/file, 4 GiB total** (same file count/depth; up to 256 requested paths).
The older peer design doc's 64/512 artifact implication is stale. Confirm the
installed binaries on both ends; a previous release/experimental transport may
still enforce smaller limits. Keep this tiny fixture below even the old limits;
check actual archive size before fetching. Large source/generated trees remain
unsuitable; ignored fixture `out/` must never enter source sync.

## Evidence required to graduate this template

Record reviewed source revision, runner label/native architecture, exact tool
versions, job/run IDs, commands, archive hashes and byte sizes, compile failures
or success, ignored tests, and approval/network/cancellation observations.
Linux structural checks alone do not graduate it. Outstanding: real Xcode
project parsing/build, native Rust builds, artifact symlink round-trip on macOS,
UI/runtime behavior, relay setup/approvals and real peer transfer. No such checks
have been run by this handoff subagent.
