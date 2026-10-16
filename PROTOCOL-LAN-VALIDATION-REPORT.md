# Sol — LAN identity/transport validation

## Result and scope

**Shared job 68 passed: 4 LAN tests + 10 parent tests, 0 failed/ignored, followed by offline/locked all-target clippy with warnings denied.** This validates a test-only identity/transport fixture on IPv4 loopback, not a production LAN worker or confined two-machine network.

Worked only in `/home/abe/task-manager-worktrees/osvauld-foundation`, branch `feature/osvauld-foundation`, unborn HEAD; verified cwd/branch/status and read the required instructions/reports/tests before changes. Parent `/home/abe/osvauld2` was inspected read-only: `dev`, HEAD `90d22ecf00215fcff2ce52b2b5893ac7bd58aef2`, with existing untracked `demo_apps/swag_shop/` and `upload.png` preserved. No main/other-worktree edits, staging, commits, deployment, service restart, cache cleanup or device operations.

Owned changes:

- `experiments/osvauld-foundation/tests/lan_identity.rs`: removed only unused `Exchange.request_body` and its initializer (two lines). No assertions, protocol behavior or security policy changed. `rustfmt --edition 2024` made no additional changes; final `--check` passed.
- `experiments/osvauld-foundation/Cargo.lock`: updated offline by the already-authorized job 64 for added LAN dependencies; unchanged by job 68. No manual lock edits or manifest changes.
- Experiment `README.md`: updated only the LAN section, preserving the latest historical job-status context already present when editing.
- This report and new `feedback/protocol-lan-resume-68-validation.md`.

`parent_contracts.rs`, root `src/local_command.rs`, `scripts/worker_source.py`, `scripts/linux_worker_setup.py`, `scripts/setup-linux-worker.sh` and experiment `Cargo.toml` were not edited. SHA-256 checks across the narrow fix/rerun confirmed these files, and the job-64 lockfile, unchanged. The separate source-only placement fix was neither edited nor deployed by this validation.

## Exact validation evidence

All Cargo commands ran through installed shared `tasks`, with explicit `-j 2`, offline, and a stable experiment source during each job. Inspected `tasks list`, `tasks admission` and `tasks logs ID --json`; no pressure/disk gates were bypassed.

Historical distinction: job 33 had passed only the original 10 parent tests and clippy. Job 56 was refused at the disk-reserve check **before Cargo**; the new four LAN tests had never compiled or run then. Neither historical job was replayed.

### Job 64 — parent-submitted, monitored to terminal before editing

Owner `protocol-lan-resume`; cwd `experiments/osvauld-foundation`:

```sh
sh -c 'cargo test --offline -j 2 && cargo test --offline --locked -j 2 && cargo clippy --offline --locked -j 2 --all-targets -- -D warnings'
```

- First test command resolved 262 added packages from offline inputs and passed: **4 LAN + 10 parent**, 0 failed/ignored/measured/filtered out.
- Locked test rerun actually ran and passed: **4 LAN + 10 parent**, all zero failed/ignored/measured/filtered out.
- Warnings-denied all-target clippy actually ran but failed solely on `dead_code`: `Exchange.request_body` was never read. Job terminal `failed`, build stage exit **101**. The enclosing worker also logged its failure wrapper; this was not a test failure or missing offline input.
- Observed CPU and memory PSI waits; recorded admission wait 259.877 s and command-stage duration 251.449 s. No duplicate submission, source edits while queued/running, or retry after client timeout.

After that terminal result, removed the unused field/initializer without suppressing warnings.

### Job 68 — sole follow-up validation submission

From the experiment directory:

```sh
tasks run --who protocol-lan-resume-sol -- sh -c 'cargo test --offline --locked -j 2 && cargo clippy --offline --locked -j 2 --all-targets -- -D warnings'
```

- Client printed submitted ID **68**, then the harness call timed out at 30 seconds while waiting. Queried that ID repeatedly rather than submitting again.
- Observed I/O and memory PSI waits, then ordinary shared admission; recorded wait 740.814 s, command-stage duration 18.408 s. The trusted shell recipe retains the compile slot for the entire test/clippy chain; the ledger records a single build stage, not separate test-run stages.
- `lan_identity`: **4 passed**, 0 failed/ignored/measured/filtered out (10.92 s reported by test harness).
- `parent_contracts`: **10 passed**, 0 failed/ignored/measured/filtered out (4.57 s).
- `cargo clippy --offline --locked -j 2 --all-targets -- -D warnings`: **passed**, dev-profile completion recorded, no warning suppression.
- Job terminal **passed**, stage exit **0**. No ignored tests; unrun suites are not passes.

Both retained job logs were untruncated when inspected. Evidence remains queryable using `tasks logs 64 --json` / `tasks logs 68 --json`; filtered local copies are `/tmp/protocol-lan-resume-sol-job{64,68}.json` (outside Git). The final experiment graph includes iroh/iroh-base/iroh-relay **1.3.0**, tokio **1.53.3**, Loro **1.16.2**, redb **2.6.4**. Lockfile SHA-256: `179346f186b18ef0f51a245d2a2111afd87825217a6a0b41104224883cf24d79`. Absolute mutable parent paths remain a development/provenance limitation, not revision-pinned distribution.

Not run: root coordinator Cargo suites/build, worker E2E/self-suite/native_commands, Python setup/source suites, second-machine networking, IPv6/multihomed confinement, packet capture, blob transfer, MCP, remote daemon execution, or durable distributed cancellation/recovery.

## What the four LAN tests actually cover

| Test | Observed assertions |
| --- | --- |
| `installations_from_one_mnemonic_share_device_key_but_not_transport_key` | Same synthetic account DID and SAME parent-derived device key; distinct randomly generated installation transport keys stored in sealed vault entries. Raw entry lacks plaintext secret bytes; locked/reopened-before-login reads fail; reopen/login recovers the existing transport key. Also characterizes that a same-account installation can decrypt copied sealed entries. |
| `endpoint_attestation_reuses_courier_claimant_rooted_token` | Existing unchanged courier claimant-rooted, non-delegable attestation binds the expected account to the endpoint-id namespace. Valid token passes receiver policy. Rejects missing token, other endpoint, other account DID, unprefixed device-style binding, wrong audience/holder, delegable root grant, expired token, revoked token and valid-but-unallowlisted endpoint. A newly issued token is distinct from the revoked one. No platform capability is conferred by the relationship attestation. |
| `lan_endpoint_has_no_relay_lookup_or_public_fallback` | Actual iroh endpoints use Minimal preset, disabled relays/portmapper, cleared address lookup, minimal net report and only `127.0.0.1:0`. Checks loopback sockets, no advertised relay/home relay/lookup services; endpoint-id-only connection fails without an address; wrong ALPN fails without handling. No public networking experiment was performed. |
| `receiver_admits_only_attested_allowlisted_fresh_requests_over_loopback` | Real QUIC/TLS `remote_id()` is checked against account attestation and a separate receiver-local endpoint allowlist before issuing a challenge. Missing/wrong-DID/wrong-endpoint/expired/revoked/unapproved cases reject without incrementing the handled counter. Honest requests receive transport-key-signed responses. New random session challenges differ; old signed request replay, wrong-challenge signatures, old responses against new challenges and responses checked against changed request bytes fail. Requester clock +1 day succeeds while parent timestamp proof rejects that skew. Locked vault loses new account-signing access. |

Request signatures bind domain, challenge, both endpoint IDs and SHA-256(request body); response signatures also bind the response hash. The tests verify those actual constructions, but do not separately mutate a transmitted request body, response body or endpoint tuple. Malformed/oversized frames, malformed signatures, chained-token rejection and tampered-token signatures are not independently exercised here. The handled counter is a synthetic acknowledgment boundary, **not command execution or task approval**.

## Limitations and unresolved policy

- Loopback is not a real two-machine LAN or a confined multihomed/VPN network. Configuration assertions and no-address failure are not packet-captured proof of zero egress. No IPv6/interface-selection/address-policy acceptance exists. Iroh still constructs a DNS resolver, unused by this no-lookup/no-relay fixture.
- `restore_installation` recreates the parent's private vault disk layout using public identity/storage calls and then real vault login. It is a synthetic fixture, **not a production restore API**; a reviewed restore/linking API remains necessary.
- Parent derivation domains are unchanged: same mnemonic still yields the same parent device key. Sealing is account-key-based, so hostile devices retaining shared account secrets cannot be strongly revoked merely by removing an endpoint from an allowlist. Account succession/key rotation and attestation revocation policy remain open.
- Receiver allowlist and revocation sets are in-memory and endpoint-level, not durable task-scoped execution permissions. Membership is not execution authority. Attestation expiry still uses the receiver wall clock; `iat` is not checked here, and sufficiently negative issuer skew can expire an attestation early. Challenges remove request timestamp dependence, not every clock issue.
- The receiver checks the requester's account attestation; the requester pins the expected receiver transport ID and verifies its response signature. This fixture does not implement reciprocal server account-attestation negotiation. Transport-key proof is not a second factor.
- No production transport adapter, iroh-blobs integration, MCP, remote daemon execution, immutable execution approval envelope, durable attempt reconciliation or cancellation was added. No security/product policy was silently resolved by the lint fix.

## Output placement and blockers

The operator-restored cache headroom was sufficient: this session observed 105,430,970,368 bytes free on `/home/abe/build-cache` before compilation (about 98.2 GiB). The experiment output filesystem separately had 37,661,396,992 bytes available then. Job 64 cleared the former hard disk refusal normally.

For this explicitly authorized regression, the existing per-worktree experiment `target` real directory was used **in place**. Its observed inode **38268639** remained the same after validation. It was not migrated/deleted/shared, and no cache-policy variables were overridden. Installed root-only placement remains an operational limitation; another agent's placement candidate is outside this validation and no deployment is implied. No remaining compilation/API blocker in the owned experiment was observed. Production restore/linking, policy, provenance, network confinement and transfer gates above remain blockers to production integration.

## Next gate: authenticated missing-blob transfer (not implemented here)

Use upstream iroh-blobs with a reviewed compatible version/provenance; do not invent a replacement store or reinterpret existing SHA-256 receipts as its native hashes. First acceptance must use bounded synthetic manifests/content over actual authenticated loopback transport, with no execution API:

1. Distinct persisted installations reconnect with expected account/endpoint binding and explicit receiver approval. Wrong DID/endpoint, expired/revoked attestation and unapproved peer fail before content access.
2. Bind manifest receipt, peer identities, freshness and transfer permission/budgets together. Knowing a content hash is not authorization; unreferenced/unapproved blob requests and cross-manifest reuse attempts must fail without leaking content.
3. Verify received hash, length and complete-manifest consistency. Reject tampered/truncated/wrong-hash content, invalid paths/types/modes, oversized frames/files/counts/aggregate bytes and quota excess without publishing partial inputs.
4. Actual missing-content reconciliation transfers only missing approved blobs; unchanged content and rename reuse avoid retransmission, executable-mode changes alter the manifest, deletions affect only a new materialization, and eviction/missing content triggers verified retransmission.
5. Interrupt at manifest negotiation, mid-blob and after transfer/before acknowledgment; reconnect/reconcile idempotently without duplicate publication or treating incomplete inputs as executable. Fresh challenge/body binding must reject replayed transfer requests/responses.
6. Keep the loopback-only Minimal/no-lookup/no-relay transport. A separately authorized two-machine, IPv4/IPv6 and multihomed/VPN/interface-policy test with egress evidence is a later acceptance step, not inferred from loopback.

Remote execution and durable cancellation remain later gates, requiring their own exact immutable approval envelope, local worker admission, durable attempt ownership and disconnect/restart reconciliation tests.

## Task-manager trial feedback

Shared admission honored real CPU/memory/I/O pressure; restored disk headroom allowed the previously uncompiled LAN tests to run. Job-64 failure was honestly separated from its passing tests; job 68 recovered the lint-only issue. Client-timeout reconciliation used the printed ID with no duplicate, cancellation or restart. Root-only nested-output placement remains visible friction, not fixed or bypassed here. Required unique feedback: `feedback/protocol-lan-resume-68-validation.md`.
