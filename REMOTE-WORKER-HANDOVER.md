# Handover: Linux LAN worker and osvauld foundation

## Goal and agreed scope

Extend the standalone task manager to a second Linux desktop/laptop, potentially with a GPU. Reuse foundations directly from **osvauld2**, not through Tinline. Keep execution local to the user's machines and LAN: no cloud coordinator, public discovery, public relay or Git remote required for normal operation.

Provide a Linux setup script for our worker build, build toolchains and optional Android emulator readiness. Initial downloads may require internet; that is separate from LAN-only operational networking. Offline builds work only when required dependencies/toolchains/images are already present.

Current bounded slice: **read-only setup inspector/dry-run plan and local immutable manifest/missing-blob contracts implemented** (Sol; jobs 41/42). See [usage](scripts/README-linux-worker.md) and [local contract](docs/design/linux-worker-local-contracts.md). These are not authenticated transfer. Remote execution, emulator installation and GPU scheduling are not implemented or authorized for deployment by this handover.

## Where to continue

- Authoritative main repository: `/home/abe/task-manager`.
- Owned worktree: `/home/abe/task-manager-worktrees/osvauld-foundation`.
- Worktree branch: `feature/osvauld-foundation`.
- Both main and this branch had **unborn HEADs** at creation. Git 2.54 supported `git worktree add --orphan`; no initial commit, staging or invented history was performed.
- Worktree contains a verified copy of 60 nonignored main source files. Original inventory: `/tmp/task-manager-osvauld-foundation-baseline.json` (temporary local evidence, not a durable source dependency).
- All baseline files are untracked; ordinary `git diff` does NOT describe the baseline or show new-file contents. Do not infer that an empty diff means no changes.
- Main is used by other agents and may have advanced independently. Never copy this entire baseline back over main. Reconcile only owned additions/changes with current main, and get explicit authorization before staging/committing/merging.
- Parent audited read-only: `/home/abe/osvauld2`, branch `dev`, HEAD `90d22ecf00215fcff2ce52b2b5893ac7bd58aef2`.
- Parent had unrelated untracked `demo_apps/swag_shop/` and `upload.png`; preserve them. Verify current status rather than assuming it is unchanged.

Read `AGENTS.md`, `README.md`, `TRIAL.md` and `SAFETY-UPDATE.md` in this worktree. For parent changes, read its own instructions/current subsystem docs first; parent changes are not part of the current owned scope.

## What is actually built

The existing installed coordinator supports local Rust-native resource admission, explicit live-command approval, durable SQLite job state, bounded logs, process ownership/cancellation and recovery. This work did not replace or restart it or migrate its history.

New worktree additions:

- `docs/design/osvauld-foundation.md`: acceptance gates and audited reuse boundaries.
- `experiments/osvauld-foundation/Cargo.toml`: independent test-only Cargo workspace importing actual local parent crates.
- `experiments/osvauld-foundation/tests/parent_contracts.rs`: 10 real characterization tests.
- `experiments/osvauld-foundation/Cargo.lock`: generated experiment dependency lock.
- Experiment `README.md` and `.gitignore`.
- `feedback/osvauld-foundation-33-initial-queue.md`: shared-coordinator trial evidence and cache-placement issue.

Sol's next bounded additions (no production coordinator manifest/API changes):

- `scripts/setup-linux-worker.sh` and `scripts/linux_worker_setup.py`: bounded read-only JSON inspect/plan; Ubuntu 24.04/Debian 12 x86_64/aarch64 inspector scope; apply always unsupported. Executable-path evidence only, no tool/version/shim invocation or installation.
- `scripts/worker_source.py`: explicit no-HEAD file selection, bounded immutable in-memory capture, versioned SHA-256 manifest receipt, diff and missing-content planning/retrieval from retained bytes. No store, transport, materializer or execution approval.
- `tests/test_linux_worker_contracts.py`: synthetic filesystem/metadata and CLI contract tests.
- `scripts/README-linux-worker.md`, `docs/design/linux-worker-local-contracts.md`, `SOL-IMPLEMENTATION-REPORT.md` and `feedback/sol-linux-worker-41-local-contracts.md`: usage, limitations and exact validation evidence.

No remote listener, applying Linux bootstrap, account-linking UI, blob exchange, emulator setup or GPU execution exists yet.

### Validation

Shared **job 33**, owner `osvauld-foundation`, passed:

```sh
# cwd: experiments/osvauld-foundation
sh -c 'cargo test --offline -j 2 && cargo clippy --offline --locked -j 2 --all-targets -- -D warnings'
```

- 10 tests passed, 0 failed, 0 ignored.
- All-target clippy with warnings denied passed.
- Job initially waited behind other agents, then acquired native admission normally.
- Diagnostic response: `/tmp/task-manager-osvauld-foundation-job33.json`; shared retained logs remain queryable with `tasks logs 33` or `tasks logs 33 --json`.
- Tests exercised identity recovery/signatures, encryption, parent device-key derivation, request-proof/replay/clock behavior, independent/reopened redb stores, sealed vault entries/locked signing, and actual two-writer Loro snapshot convergence.
- These tests do **not** prove remote execution, remote cancellation, LAN confinement, blob authorization or production device revocation.

### Sol bounded-slice validation

All submitted through installed shared admission, owner `sol-linux-worker`, run-only (no compilation):

- Job 37: expected initial import failure before implementation (1 loader error).
- Job 38: initial 10 contracts passed, 0 failed/skipped.
- Job 40: expanded 14 tests, 5 expected new credential-path subtest errors before adding those exclusions. PSI wait honored.
- Job 41: final `env PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -p 'test_*.py' -v`: **18 passed, 0 failed/skipped** (14 new, 4 existing wait-budget).
- Job 42: real-host CLI inspect/plan/apply smoke passed (job exit 0). EndeavourOS x86_64 correctly unsupported; all three CLI calls returned 2. Client timed out while admission waited on memory PSI; job queried, not resubmitted, then confirmed passed.
- Retained evidence: `tasks logs ID`; `/tmp/sol-linux-worker-job{38,40,41,42}.json` and `/tmp/sol-linux-worker-host-cli.json`. Fixtures were synthetic, no credentials/toolchain/device execution.

Supported distro tests are fixtures only. Tool versions, KVM ioctls, GPU/emulator runtime, installability, remote auth/transfer/execution and Rust regression suites were not validated. Existing SHA-256 receipt formats are untouched; new local SHA-256 receipts are explicitly not iroh-blobs hashes. See Sol report for owned files and next review boundary.

### Cache-placement blocker before more large builds

The installed `local_command::place_outputs` discovers the Git root and creates output links there only. The nested independent experiment generated an ignored local `experiments/osvauld-foundation/target` (~1.2 GiB at observation), rather than using a nested cache-backed target.

The output was preserved. No environment override, deletion or migration was attempted. Resolve placement non-destructively before further large builds. Do not claim changing the cwd alone solves it, and do not share another checkout's target. Feedback contains a suggested acceptance test; the placement fix is not implemented.

## Reuse audit: important gaps

| Component | Use | Caveat |
|---|---|---|
| Parent `cryptography` / `identity` | Frozen derivation domains, account DID, signatures, encryption/keystore | Same mnemonic derives the **same device key**. Two installations need separately generated transport/device credentials with an account binding; do not silently change parent derivation domains. |
| Parent `vault` | Headless unlock, signing closure, sealed opaque entries | Use dedicated compute data directories, not the live shell vault. `with_signer` holds the account mutex; do not reenter the vault inside it. |
| Parent `storage` | Per-device redb byte stores | Public `put` commits one key at a time; no exposed atomic multi-key batch. Retain SQLite job ledger and atomic completion. Never transfer a live database file. |
| Parent `courier` | Pure authenticated claims/tokens, exact-byte request proofs/replay, document sync | Not an iroh transport. Timestamp checks assume shared clocks, with no future skew; a LAN does not guarantee synchronized clocks. Existing workspace roles do not authorize command execution. |
| Loro | Optional collaborative job views and metadata | Distinct writer IDs per installation. Not execution arbitration, distributed locking or automatic retry authority. |
| Parent `voice/src/net.rs` | Actual iroh 1.3 connection example | Voice POC, not an account-authorized compute connection. |
| Upstream `iroh-blobs` | Source/artifact content transfer | No parent Cargo dependency was found. Integration is new work; do not claim the parent already provides it. Tinline's 0.103 usage is reference only. |

The parent's older architecture prose understates current local sync: courier/kunki now have local bridge document sync. This still is not desktop-to-desktop compute networking.

The experiment currently uses **absolute mutable path dependencies** into `/home/abe/osvauld2`; its lock resolves compatible Loro 1.16.2 and redb 2.6.4, not the parent's exact locked graph. Before distributing, use a reviewed revision-pinned dependency or provenance-recorded extraction preserving the parent's MIT license. Do not ship these machine-local paths or assume the other device has this checkout. Task-manager remains GPL-3.0-or-later.

## Source change detection and transfer contract

Git/jj can supply provenance but are not required for execution or delta detection. Dirty and permitted untracked files must work, including this unborn repository.

Proposed per-job flow:

1. Select an explicit source root and bounded allowed file set. Exclude VCS metadata, build/cache outputs, local state and credentials. Ignore rules alone are not a secret scanner.
2. Capture immutable bytes and a versioned manifest: normalized relative path, blob content hash, byte length and executable bit. Validate paths, file types and size/count/depth limits. Reject escaping paths and unsupported links/special files.
3. Detect/reject or boundedly retry concurrent modifications during capture; never claim an atomic filesystem snapshot without implementing that guarantee. Hash and transfer the captured bytes, not a later reread of mutable workspace files.
4. Bind approval to that exact manifest and execution specification. Preserve existing SHA-256 receipt semantics; distinguish those hashes from iroh-blobs' native hash format rather than reinterpreting one as the other.
5. An authenticated receiver reports missing referenced blobs. Transfer only those blobs, verify them and enforce per-job authorization/size budgets. Knowing a hash alone must not grant access to private source/artifacts.
6. Materialize a new isolated job directory. Verify exact contents and modes before execution. Later workspace edits belong to a new snapshot, never the running job.

Expected cases:

- Unchanged file: no content retransmission when receiver still holds the blob.
- Rename with unchanged bytes: manifest/path change; usually reuse the existing blob.
- Executable-bit change: manifest change even with identical content bytes.
- Deletion: absent from new materialization, not deletion from another active job/workspace.
- Receiver missing/evicting an old blob: transfer again; do not assume a previous manifest proves current possession.
- Transfer interruption: resume/reconcile without executing incomplete inputs or creating another execution attempt.

Do not transfer `target/`, Gradle outputs, dependency caches, SDKs or host-native build outputs as ordinary source. Provision/cache toolchains separately per host and architecture. Use upstream blob facilities rather than inventing a second content store; file-level deduplication does not imply arbitrary cross-file delta compression.

## Next implementation sequence

### 1. Setup inspector and manifest tests first

Implemented local interface: `scripts/setup-linux-worker.sh --inspect` and `--plan` return JSON with bounded metadata; `--apply` always refuses. Inspection support is narrowly Ubuntu 24.04/Debian 12 x86_64/aarch64; no package-manager/install support is claimed. Missing tool versions, pins/checksums and download budgets are explicit blockers, not executable installation proposals. Source contracts require an explicit individual-file list; no implicit tree scan or Git HEAD. See usage for bounds and caller responsibility for updating the selected set.

Read-only inspection should identify:

- Distro/architecture, CPU count, RAM/swap, free disk and intended cache location.
- Installed tool versions and missing prerequisites without triggering auto-installs.
- GPU/driver availability as capability information, not proof of usable GPU workloads.
- `/dev/kvm` presence/access, virtualization support and Android emulator prerequisites.

Plan output should enumerate package/download sizes where known, pinned versions/checksums, destination paths, permission changes and readiness blockers. No automatic sudo, account-secret access, public network discovery, service restart, emulator launch or device installation in inspect/plan mode.

Tests: inspect is side-effect-free; unsupported distro/architecture fails clearly; missing KVM permissions produce a plan, not a privileged mutation; source manifests correctly detect edits/renames/mode changes/deletions and reject excluded/escaping/oversized inputs.

### 2. Reproducible worker package and opt-in setup

Distribute a matching worker build with version/protocol/platform metadata and checksum verification. Avoid requiring osvauld/Tinline source trees on the second machine. Keep any experimental binary/state distinct from the installed shared coordinator until a deployment is explicitly authorized.

Provision pinned Rust/JDK/Android SDK/NDK as needed by declared build recipes. Explicit approval is required for package installation, downloads, Android license acceptance, sudo/group changes or persistent services. Idempotent re-run must preserve existing SDKs, credentials, user AVDs and unrelated caches. No destructive cleanup or active swapfile changes.

For emulator setup, select a host-compatible image/ABI and private AVD, check KVM, and validate software/hardware graphics deliberately. GPU driver presence alone is not emulator compatibility. Do not install on or wipe the user's phone. Emulator runtime operations need the existing device-lock discipline and separate permission.

### 3. LAN-only identity and transport

Generate distinct persisted installation keys; bind endpoints to the stable account DID with reviewed account attestations and explicit linking/allowlisting. Keep private transport keys in local sealed entries, never replicated Loro/source manifests. Same account membership is not execution permission.

Audit the actual iroh version/configuration: a default preset is not evidence of LAN-only behavior. Disable public relays AND public discovery/address lookup, explicitly constrain permitted interfaces/peer addresses, and test no public fallback. Consider IPv4 and IPv6, and multihomed/VPN hosts. IP locality is not an authentication mechanism. This is a proposed requirement, not a claim that defaults already satisfy it.

Use a task-specific protocol/ALPN. Resolve independent-clock freshness/challenges and replay binding before treating courier proofs as remote-ready. Verify transport endpoint, attestation, account policy and specific task permission together. Do not weaken verified-DID policy.

### 4. Authenticated missing-blob transfer

Use synthetic accounts and two independent local endpoints first. Transfer bounded manifests/blobs over the actual transport, not only an in-memory model. Test wrong DID/endpoint, unapproved blob requests, tampering, quotas, disconnect and missing content. Add a second-machine test afterward; two processes on one host do not prove real LAN/emulator integration.

### 5. One approved bounded execution, then Android

Keep one authoritative coordinator initially. An immutable execution envelope should bind coordinator/worker device, attempt ID, source manifest, argv/cwd contract, resource budget and output policy. Worker must apply native admission locally; coordinator admission on machine A is not machine B resource control.

Persist acceptance/start/result and sequenced logs/artifact receipts. Duplicate requests must reconcile the same attempt, not spawn a second one. Disconnect means uncertain state until queried/reconciled, not automatic failover/replay. Test cancel/restart and dropped responses at each transition before enabling general commands.

Then prove one real build on the other Linux machine, then optional Android build/emulator readiness. Laptop battery/power/thermal/GPU eligibility and hard quotas are later explicit scope, not already implemented.

## Security and operational boundaries

- A shared account mnemonic cannot strongly revoke a hostile device holding those account secrets. Discuss device succession/revocation policy explicitly; do not promise otherwise.
- No anonymity, hostile-host sandbox, universal shell parser or hard resource-quota promise.
- Preserve author proofs, user source and existing SQLite history. Do not migrate to redb just because parent storage uses it.
- Keep approvals explicit. Never approve/replay the backlog, retry a mutation blindly or cancel another agent's job.
- Installed runtime: `/home/abe/.local/bin/tasks`; shared state `~/.local/state/tinline-task-manager-native`. Verify current deployment before operational work; do not replace it just by building source.
- Heavy builds/tests use the installed shared coordinator, explicit Cargo `-j 2`, and current pressure/admission gates. Do not override machine cache settings or nest internally admitted helpers.
- Required trial feedback: unique new feedback file plus a short task report; redact secrets and distinguish observed facts from hypotheses.

## Resume checklist

1. Verify cwd, branch, HEAD/unborn state, status and other agents' ownership. Reread local instructions.
2. Inspect `tasks list` and `tasks admission`; do not kill existing jobs to get a slot.
3. Read the design, experiment README/tests and job-33 feedback before compiling again.
4. Nested target placement remains a documented blocker, not fixed; no more heavy Cargo builds until reviewed non-destructive handling exists. Do not delete/move outputs or override host cache policy.
5. Review the implemented local inspector/capture contracts and Sol report. Next: choose reviewed package/provenance pins or target-placement work with explicit ownership; authenticated LAN iroh transfer remains a separate acceptance gate. Keep the deployed daemon, main checkout, parent source, phone and unrelated worktrees untouched.
6. Report exact checks, results and remaining limitations; obtain review/authorization before enabling privileged setup or remote execution.
