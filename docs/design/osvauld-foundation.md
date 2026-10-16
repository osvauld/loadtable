# Osvauld-derived compute foundation

Status: local groundwork in `feature/osvauld-foundation`. Shared job 33 passed all 10 parent characterization tests (0 failed/ignored) and warnings-denied all-target clippy, offline with `-j 2`. Sol implemented the read-only setup inspector and explicit immutable in-memory source/missing-blob contracts: job 41 passed 18 Python tests (14 new, 4 wait-budget; 0 failed/skipped), job 42 passed real-host unsupported-distro CLI checks. No remote transfer/execution or deployment enabled.

## Acceptance tests, before implementation

1. Import the parent's identity, vault, storage and courier crates directly, without Tinline/chat/media dependencies. Recover the same synthetic account DID and verify signatures; reject a different signer and changed bytes.
2. Characterize the parent's device derivation honestly: recovering the same mnemonic yields the SAME device public key. It does not create two independently identifiable installations.
3. Persist isolated per-node stores and sealed vault entries; reopen and recover exactly their own data. Locking must remove signing access.
4. Verify courier request proofs reject changed bodies, wrong recipients, stale timestamps and replay. Document the shared-clock limitation rather than claiming WAN readiness.
5. Use two distinct Loro writer IDs and exchange actual document snapshots. This proves document convergence only, never execution ownership.
6. Next integration gate: two independent persisted transport keys, account-signed binding to each endpoint, receiver authorization and bounded authenticated iroh transfer. Wrong DID/endpoint, revoked device, unapproved task and replay must be rejected before execution.
7. Execution gate: approved immutable input manifest and missing-blob transfer, one bounded workload under native admission, durable sequenced logs/result/artifacts. Reconnect, restart and cancellation must not create a second execution attempt. A disconnected unknown outcome must remain uncertain until reconciled.

Tests 1–5 live in `experiments/osvauld-foundation/tests/parent_contracts.rs`. Tests 6–7 are not implemented and need the task-specific adapter; passing the first slice does not prove them.

## Sol local milestone (implemented, not remote integration)

Owned implementations: `scripts/setup-linux-worker.sh`, `scripts/linux_worker_setup.py`, `scripts/worker_source.py`; acceptance tests: `tests/test_linux_worker_contracts.py`. See [local contract](linux-worker-local-contracts.md), [usage](../../scripts/README-linux-worker.md), [report](../../SOL-IMPLEMENTATION-REPORT.md) and `feedback/sol-linux-worker-41-local-contracts.md`.

Inspect/plan is bounded read-only JSON; initial distro/architecture scope is Ubuntu 24.04/Debian 12 x86_64/aarch64. Apply always refuses. Tool executables are discovered via metadata, never invoked; versions/backing shim installations, pinned package/checksum/download budgets, KVM/GPU/emulator runtime remain unvalidated. Real host EndeavourOS x86_64 correctly returned unsupported (CLI exits 2 in job 42); supported-host coverage is synthetic only.

Source selection is an explicit bounded individual-file list, including dirty/permitted untracked files without HEAD. Linux no-follow descriptor traversal rejects escapes, links, hardlinks, specials, known credential/cache/output names and oversized inputs. Retained immutable bytes and a two-inventory comparison support deterministic local diff/missing planning; removals require the caller's updated selected set. It is not a whole-tree atomic snapshot, secret scanner, blob store, materializer or execution API. The Rust workspace adapter's digest framing/two-inventory pattern is reused conceptually under a distinct SHA-256 domain; existing closed HEAD/layout-specific adapters and receipts are unchanged. Future iroh-blobs hashes are explicitly absent, not substituted with SHA-256.

Test-first evidence: job 37 import failure before modules; job 38 initial 10 passed; job 40 expanded tests exposed five additional credential exclusions; job 41 final 18 passed, 0 failed/skipped. Jobs 40/42 honored memory PSI waits; job 42's client timeout was reconciled by querying its ID, never duplicate submission. No Cargo builds were attempted: the populated nested target and installed placement behavior remain unchanged. Target placement is still blocked as documented before this validation. Authenticated iroh/iroh-blobs transfer, exact execution-envelope approvals and identity/network policy gates remain future work.

## Audited parent, not Tinline

Parent: `/home/abe/osvauld2`, `dev`, revision `90d22ecf00215fcff2ce52b2b5893ac7bd58aef2` at audit. Kept read-only. Relevant current sources:

| Foundation | Reuse | Boundary/gap |
|---|---|---|
| `cryptography`, `identity` | Existing HKDF domains, DID/signing/encryption, sealed keystore | `identity/src/identity.rs` derives the device secret from the account seed too. Do not change these frozen domains or clone this key as two endpoint identities. |
| `vault` | Headless account unlock, `with_signer`, sealed opaque entries | Separate compute data directory, not the live shell vault. `with_signer` holds an account mutex; do not reenter the vault from its closure. |
| `storage` | Per-device redb byte stores | Parent uses redb 2. Each `put` is its own transaction; no public atomic multi-key batch. Keep the existing SQLite job ledger and atomic completion semantics. Never copy an open database to another node. |
| `courier` | Transport-free authenticated claims, tokens, request proof/replay and document sync | Not an iroh server. Request proofs currently allow no future clock skew; source explicitly says remote transport needs node-issued time. Workspace roles do not authorize running commands. |
| `workspace`, `manifest`, Loro | Resource vocabulary, document data and replicated views when needed | Do not put execution arbitration in a CRDT. Distinct installations need distinct Loro writers, independently of account DID. |
| `kunki` | Reference composition of vault/courier, local bridge and push | Cargo dependencies contain no iroh; do not import the whole node/UI as a remote worker. |
| `voice/src/net.rs` | Real parent iroh 1.3 connection example | Voice POC, not a general account-authenticated compute transport. No parent `iroh-blobs` Cargo dependency was found. |

The older high-level architecture description of sync as future work is incomplete: current courier/kunki code and group-chat-sync status show working local bridge document synchronization. Neither implies desktop-to-desktop compute networking is already built.

Use upstream iroh-blobs for content transfer rather than inventing a blob store. Its integration is NEW here, not something already provided by the parent. Tinline's iroh-blobs 0.103 integration and newer device-linking behavior are reference material only; no dependency on Tinline is planned.

## First slice and isolation

The experiment is an independent Cargo workspace and does not alter the coordinator's manifest, socket schema, SQLite history, approvals, deployed binary or running jobs. Its generated lockfile resolves Loro 1.16.2 and redb 2.6.4 under the parent's compatible version ranges; it does not reuse the parent's workspace lockfile. Job 33 exposed a cache-placement gap: automatic links cover the Git root, not this nested workspace's `target`. The ignored local output was preserved without moving/deleting it; address nested-workspace placement before further large builds. It uses explicit local path dependencies into the read-only parent to test actual APIs before extraction. Those paths are machine-local and mutable: they are NOT a reproducible distribution or a pinned dependency claim. Before shipping, use a reviewed revision-pinned Git dependency or a provenance-recorded extraction preserving the parent's MIT license. Root task-manager remains GPL-3.0-or-later.

Worktree baseline: the main task-manager repository has an unborn HEAD. This is an orphan worktree with a verified copy of its 60 nonignored source files, not a fabricated initial commit or a normally mergeable committed branch. Baseline SHA-256 inventory is outside Git at `/tmp/task-manager-osvauld-foundation-baseline.json`. Main may evolve independently; reconcile intentionally, never overwrite it wholesale.

## Proposed next adapter, not silently settled policy

Keep one authoritative coordinator initially. Add a small task-specific protocol/ALPN adapter over upstream iroh; source/artifacts use iroh-blobs, per-node secrets use vault, job ownership stays in the existing ledger. A transport/device key should be generated per installation and account-attested, not derived identically from the mnemonic. Persist its private material only in a dedicated local sealed entry, never a replicated document or source capsule.

Before enabling the adapter, review the account-to-endpoint binding, explicit linking/allowlist and revocation rules, request freshness across independent clocks, task approval/capability envelope, authenticated blob access, cancellation and durable attempt reconciliation. Each approved envelope must bind coordinator/worker device, attempt ID, immutable inputs, argv/cwd contract, resource limits and output policy. Account membership alone must never authorize arbitrary execution. No automatic replay or failover after disconnect.

Sharing account mnemonic material cannot strongly revoke a hostile linked device possessing those account secrets. This is not an anonymity or hostile-host sandbox design. GPU/thermal/power eligibility, hard quotas and laptop deployment are later work, not implied by these tests.
