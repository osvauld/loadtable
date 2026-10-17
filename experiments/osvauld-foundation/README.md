# Parent foundation characterization

Independent test-only Cargo workspace. Imports `/home/abe/osvauld2` directly; no Tinline dependency, worker listener or command execution. Parent source and the installed coordinator remain unchanged.

See [the plan](../../docs/design/osvauld-foundation.md) for acceptance gates and audited gaps. In particular, the parent derives the same device key from the same mnemonic, courier's proof timestamps currently assume a shared clock, and its permissions do not grant compute execution.

From this directory:

```sh
tasks admission
# First resolution intentionally generates this experiment's own lockfile, offline:
tasks run --who osvauld-foundation -- cargo test --offline -j 2
# Once the lockfile exists:
tasks run --who osvauld-foundation -- cargo clippy --offline --locked -j 2 --all-targets -- -D warnings
```

Job 33 passed all 10 tests (0 failed/ignored) and warnings-denied all-target clippy. Its lockfile resolves compatible Loro 1.16.2 and redb 2.6.4; this is not the parent's exact locked graph.

Cache-placement limitation discovered during validation: the installed coordinator prepares output links at the Git root only. This nested workspace generated an ignored local `target` instead; it was preserved, not moved or deleted. Resolve nested-workspace placement before further large builds. Do not reuse the parent's or another checkout's target or override machine cache policy. The local absolute source paths are a development spike, not revision-pinned distribution. Check parent HEAD/status before validation and keep source stable during jobs. Synthetic account secrets live only in throwaway fixtures and must not be logged. Tests prove API contracts, not production remote integration.

## Gate 3: LAN-only identity and transport (`tests/lan_identity.rs`)

Implemented by `osv-lan-identity`; validated by `protocol-lan-resume-sol`. Real characterization/acceptance tests against parent `identity`/`vault`/`courier` and upstream iroh 1.3.0 (offline cache), not a worker implementation. Validation status: **4 LAN + 10 parent tests pass, 0 failed/ignored, and offline/locked warnings-denied all-target clippy passes (job 68).** Job 33 previously passed only the 10 parent tests; job 56 (osv-lan-identity) was refused by the coordinator's disk reserve before Cargo started. Shared job 64 (owner `protocol-lan-resume`) ran `cargo test --offline -j 2` and then `cargo test --offline --locked -j 2`. Both runs passed 4/4 `lan_identity` and 10/10 `parent_contracts` tests, 0 failed and 0 ignored, and the first run updated `Cargo.lock` offline. Its warnings-denied all-target clippy failed on one `dead_code` error: the unused `Exchange.request_body` field. Validation removed that field and its initializer without changing assertions. Job 68 ran `sh -c 'cargo test --offline --locked -j 2 && cargo clippy --offline --locked -j 2 --all-targets -- -D warnings'` successfully. See [validation report](../../PROTOCOL-LAN-VALIDATION-REPORT.md).

These regressions explicitly used this worktree's existing populated real `target` directory in place after operator-restored disk headroom; it was not moved, deleted or shared. Installed placement still handles only the Git root; the separate source-only placement fix was not edited or deployed by this validation. No cache-policy environment override or further root Cargo build was performed. The placement limitation described above remains unresolved operationally.

Verified assertions in the four tests (network cases run in-process on IPv4 loopback):

1. **Per-installation transport key.** Two vault installations of one synthetic account (one via `signup`, one recovered from its mnemonic) have the same DID and the SAME parent-derived device key, but each generates a DIFFERENT random iroh `SecretKey`. It is stored only as the sealed vault entry `task-manager/transport/v0/secret-key` in a temp data dir; raw store bytes do not contain it; locked/reopened-before-login vaults cannot read it; login after reopen recovers the same key instead of generating another.
2. **Account attestation of the endpoint.** Uses parent `courier::token::attest` unchanged: claimant-rooted, non-delegable, role `relationship` (no platform capability), `aud` = the account DID, `KeyBinding.device` = `task-manager/endpoint/v0:<z32 endpoint id>` (prefix separates it from courier's device-key bindings), signed through the vault's `with_signer`. The receiver runs `token::verify_chain` rooted at the expected account DID with its own clock and revocation set, requires a single link and that the binding equals the TLS-authenticated `remote_id()`. No parent domain, policy table or capability changed.
3. **Separate local approval.** A valid attestation is account membership only. A distinct receiver-local allowlist of endpoint ids is checked after it; an attested but unapproved endpoint is refused.
4. **LAN-configured iroh.** `Endpoint::builder(presets::Minimal)` (crypto provider only, never `presets::N0`) + `RelayMode::Disabled` + `clear_address_lookup()` + portmapper disabled + `NetReportConfig::minimal()` + only `127.0.0.1:0` bound. Asserted: empty address-lookup services, no relay URL in `addr()`, empty home-relay status, loopback-only sockets; connecting by endpoint id without a direct address fails promptly (no lookup/relay fallback); wrong ALPN is refused. Peers connect with explicit `EndpointAddr` direct addresses on ALPN `task-manager/worker/0`.
5. **Admission before handling.** Over the real QUIC connection the receiver rejects unattested, wrong-account-DID, other-endpoint, expired, revoked and unapproved requesters before issuing a challenge; a handled-counter proves no request was handled.
6. **Freshness without shared clocks.** Receiver issues a random 32-byte challenge per session; requester signs `(domain, challenge, requester id, receiver id, SHA-256(body))` with its transport key; receiver signs the response over `(domain, challenge, both ids, request hash, response hash)`. A replayed old request, a signature over the wrong challenge, and an old response checked against a new session's challenge are rejected. A requester clock one day ahead still works (parent `proof::verify` refuses the same skew with `StaleRequest`).

Design notes and limitations (NOT proven):

- The parent vault has no import-from-mnemonic API. The test's `restore_installation` fixture reproduces the vault's private on-disk layout using public `identity`/`storage` calls, then logs in via the real vault. A real adapter needs a reviewed parent restore API; this fixture breaks if that layout changes.
- Vault entries are sealed to the ACCOUNT encryption key: the test shows another installation of the same account can open copied sealed bytes. This protects against non-holders, not a hostile linked device; revoking such a device needs attestation revocation plus key rotation policy.
- Attestation expiry still compares the issuer's `iat`/`exp` with the receiver's clock (only `exp` is checked). Request freshness needs no clock, but large negative issuer skew can make an attestation appear expired early. Revocation sets are in-memory here; persistence/distribution of revocations and allowlists is not implemented.
- Endpoint ids are authenticated by iroh TLS; the request signature is by the same transport key and adds challenge/body binding, not a second factor.
- Loopback is not LAN confinement: no second machine, multihomed/VPN host, IPv6 or interface-policy test; DNS resolver is still constructed by iroh (unused without lookup/relays). Configuration and no-address failure are tested, not packet-captured egress denial. Request replay/wrong-challenge and response wrong-request-body checks pass; dedicated mutated-request-body/response-body and malformed/oversized-frame cases are not exercised by this suite.
- Next gate: actual authenticated missing-blob transfer using bounded synthetic manifests/content over the transport. Acceptance must reject wrong DID/endpoint, unapproved blobs, tampering and quota excess; verify missing/reused content and interruption/reconciliation without execution. See the report for the complete next-gate checklist. No iroh-blobs or MCP implementation, remote daemon execution, approval envelope, durable cancellation or attempt reconciliation is claimed here.

## Gate 4: two-process LAN peer (`src/lib.rs`, `src/bin/lan_peer.rs`, `tests/lan_peer_process.rs`)

Owner `osv-lan-identity`. The gate-3 logic now lives in the experiment library `src/lib.rs`: the LAN-only endpoint builder, attestation issue/verify, allowlist policy, framing, challenge protocol and LAN config report. `tests/lan_identity.rs` exercises that library; its 4 tests are unchanged in substance, plus one added assertion that a `0.0.0.0` bind is refused. `lan-peer` is an experimental binary over that library. Validation, all owner `osv-lan-identity`, offline, `-j 2`, through the shared coordinator:
- **Job 103** (`cargo test --offline -j 2`) passed: lib/bin unit tests 0, `lan_identity` 4/4, `lan_peer_process` 1/1, `parent_contracts` 10/10, doc-tests 0, with 0 failed and 0 ignored.
- **Job 105** (`--locked` test + all-target clippy) passed the same 15 tests. Clippy then failed on one `collapsible_if` in `tests/lan_peer_process.rs`, which I fixed.
- **Job 107** (`sh -c 'cargo test --offline --locked -j 2 && cargo clippy --offline --locked -j 2 --all-targets -- -D warnings'`) passed: 15 tests, 0 failed and 0 ignored, and clippy was clean.

**SYNTHETIC ONLY.** Every `lan-peer` installation recovers the same account from the PUBLIC BIP39 test vector `abandon … about`, so both machines share one DID without a real linking flow. Anyone can derive that account's keys, so the vault sealing of the transport key protects nothing here. Each installation still generates its own random iroh transport key and an account-signed courier attestation of it, stored in `DATA_DIR/attestation.json` and valid for 7 days. Never put real data in these directories.

```text
lan-peer init    --data-dir DIR                       # create installation, print endpoint id
lan-peer serve   --bind IP:PORT --data-dir DIR [--allow PEER_ATTESTATION.json]... [--max-connections N]
lan-peer connect --peer ENDPOINT_ID --addr IP:PORT --data-dir DIR --allow PEER_ATTESTATION.json [--bind LOCAL_IP:0]
```

- **No discovery, no relay.** The endpoint is built from `presets::Minimal` with relays disabled, address lookup cleared, portmapper off, minimal net report and exactly one socket. `serve` refuses to start if its config report shows any relay or lookup. Peers dial an explicit `ENDPOINT_ID` at `IP:PORT`.
- **Explicit interface binds.** `0.0.0.0` / `::` are refused. `connect` defaults its bind only for a loopback `--addr`; for a LAN address you must pass `--bind YOUR_LAN_IP:0`.
- **Allowlists on both sides, exchanged out of band as files.** The server admits only connections whose TLS-proven endpoint id matches a valid, unexpired attestation for the expected account and appears in its `--allow` files. The client checks that its `--allow` file attests exactly `--peer` before dialing, and iroh TLS then proves the server holds that key. Rejections happen before a challenge is issued.
- **Freshness.** The receiver issues a random challenge; the request and the response are both signed over the challenge, both endpoint ids and the body hashes. There are no timestamps.
- **Output and exit codes.** `connect` prints one JSON line and exits 0 (`ok`), 2 (usage), 3 (`rejected`: peer attestation, receiver policy or stale response) or 4 (transport/timeout, 30 s budget). `serve` prints one JSON event per line: `listening`, then `handled` / `rejected` / `connection_failed` / `error` / `timeout`. Each connection is limited to 30 s.

`tests/lan_peer_process.rs` spawns the built binary as separate processes on 127.0.0.1: `init` ×2, `serve`, then several `connect`s. It checks:
- one shared DID and three distinct endpoint ids;
- a relay-free, lookup-free loopback listener;
- an approved client succeeds, with a fresh response and a stable persisted key;
- a same-account but unallowlisted client is rejected `NotAllowlisted`, exit 3;
- a wrong `--peer` with the server's attestation is rejected before dialing, exit 3;
- a wrong `--peer` with a matching attestation fails the TLS handshake, exit 4;
- the server keeps serving afterwards;
- `serve --bind 0.0.0.0:0` exits 2.

### Try it: serve on the NixOS machine, connect from Arch

The binary is at `experiments/osvauld-foundation/target/debug/lan-peer` (debug, about 295 MB). It is dynamically linked against glibc: interpreter `/lib64/ld-linux-x86-64.so.2`, needing `libgcc_s.so.1`, `libm` and `libc`, with glibc symbols up to `GLIBC_2.34`. NixOS has no `/lib64/ld-linux-x86-64.so.2`, so it will not run there directly; see the NixOS options below. Replace `N_IP` and `A_IP` with the two machines' LAN addresses and pick a free UDP port (7842 here). iroh uses UDP/QUIC only.

1. On each machine, create an installation:
   - NixOS: `./lan-peer init --data-dir ~/lan-peer-n`
   - Arch: `lan-peer init --data-dir ~/lan-peer-a`

   Each prints its `endpoint_id` and writes `attestation.json`.
2. Exchange the attestation files out of band (USB stick or `scp`): Arch's `~/lan-peer-a/attestation.json` goes to NixOS as `a.json`, and NixOS's `~/lan-peer-n/attestation.json` goes to Arch as `n.json`. They contain public material only: a DID, an endpoint id and a signature.
3. Open UDP 7842 on the NixOS live system. Its default firewall is iptables-based, using the `nixos-fw` chain:
   ```sh
   sudo iptables  -I nixos-fw 1 -p udp --dport 7842 -j nixos-fw-accept
   sudo ip6tables -I nixos-fw 1 -p udp --dport 7842 -j nixos-fw-accept   # only if using IPv6
   ```
   This lasts until reboot. Remove it with the same commands using `-D` instead of `-I 1`. If the system uses the nftables firewall instead, add an equivalent accept rule for udp dport 7842 in the `nixos-fw` input chain. Arch needs no inbound port, because the client binds an ephemeral port and QUIC replies return on the same flow.
4. NixOS: `./lan-peer serve --bind N_IP:7842 --data-dir ~/lan-peer-n --allow a.json`. Note the `endpoint_id` in the `listening` line.
5. Arch: `lan-peer connect --peer N_ENDPOINT_ID --addr N_IP:7842 --bind A_IP:0 --data-dir ~/lan-peer-a --allow n.json`. Expect `"status":"ok","response_fresh":true` and exit 0, with a `handled` event on NixOS. Stop the server with Ctrl-C.

**Running on NixOS (not tested here, because no NixOS host was available):**
- **(a) Static musl build.** Preferred, but blocked: `rustup target list --installed` shows only `aarch64-unknown-linux-musl`, not `x86_64-unknown-linux-musl`. It needs approval to run `rustup target add x86_64-unknown-linux-musl` (a download). After that, `tasks run --who … -- cargo build --offline --locked -j 2 --release --target x86_64-unknown-linux-musl --bin lan-peer`. The `ring` C code would build with the host `gcc`/`clang` (no `musl-gcc` is installed), which is not yet verified.
- **(b) Run the glibc binary through the store loader on NixOS.** No install is needed, and it is offline if those store paths exist:
  ```sh
  L=$(dirname "$(ls /nix/store/*-glibc-2.*/lib/ld-linux-x86-64.so.2 | head -1)")
  G=$(dirname "$(ls /nix/store/*-gcc-*-lib/lib/libgcc_s.so.1 | head -1)")
  "$L/ld-linux-x86-64.so.2" --library-path "$L:$G" ./lan-peer init --data-dir ~/lan-peer-n
  ```
  The NixOS glibc must be at least 2.34.
- **(c)** Use `nix-shell -p patchelf` to patch the interpreter and rpath, or `programs.nix-ld.enable` on an installed system. Both need nixpkgs from the network or the store.
- **(d)** Build on NixOS from source with `nix-shell -p cargo rustc gcc`. This needs this source tree, the osvauld2 path dependencies and a populated crate cache, which the absolute `/home/abe/osvauld2` paths prevent.

**What this does NOT prove.** No real two-machine run has been done yet (loopback only). There is no LAN confinement proof and no multihomed/VPN or IPv6 test. (Source transfer was added later in phase 2; see below.) The account is synthetic and shared from a public mnemonic. Revocations and allowlists live in memory and in command-line files only. No command execution, approval envelope, cancellation or attempt reconciliation exists. Attestation expiry still compares the issuer's clock with the receiver's. A debug binary is not a reproducible, versioned package.

**iroh-blobs (pre-integration offline check; superseded by phase 2 below).**
- Cached `iroh-blobs 0.103.1` requires `iroh ^1.0.0` and `iroh-base ^1.0.0`, which semver-match the iroh 1.3.0 in use. All of its direct dependencies are in the offline cache, including `rand 0.10`, `irpc 0.17`, `bao-tree 0.16`, `redb 4` (optional, for fs-store) and `iroh-tickets 1.0`.
- A reference lockfile (`/home/abe/p2p_chat/Cargo.lock`, read only) resolves `iroh-blobs 0.103.1` together with `iroh 1.3.0`.
- Cached `iroh-blobs 0.98.0` needs iroh 0.96 and is NOT compatible.
- Not verified: a full offline resolve and build in this workspace. Its `fs-store` feature uses redb 4, while the parent `storage` crate uses redb 2; both versions would coexist.

## Source transfer (phase 2) (`src/source.rs`, `tests/source_transfer.rs`)

Owner `osv-blob-transfer`. Content-addressed transfer of an explicit source file list into a worker's iroh-blobs store, then an immutable checkout. It reuses the gate-4 transport unchanged: the same LAN-only iroh 1.3 endpoint (no relays, no discovery, explicit binds) and the same `Policy::admit` gate (account attestation + local allowlist, checked before any challenge). Nothing received is executed.

```text
lan-peer manifest    SOURCE_DIR --files-from0 LIST          # LIST: NUL-separated relative paths
lan-peer serve       --bind IP:PORT --data-dir DIR [--allow FILE]... --store STORE_DIR --checkouts CHECKOUT_DIR
lan-peer push-source --peer ENDPOINT_ID --addr IP:PORT --data-dir DIR --allow FILE \
                     --source SOURCE_DIR --manifest MANIFEST.json --checkout NAME [--bind IP:PORT]
```

Design decisions:

- **Hash format.** Each manifest file's `iroh_blobs_hash` is the lowercase hex of `BLAKE3(content)`. That is exactly the iroh-blobs raw-format (`BlobFormat::Raw`) blob hash. The test pins the published BLAKE3 values of `""` and `"abc"` and checks them against `iroh_blobs::Hash::new`. The top-level `iroh_blobs_hash` (a collection hash) stays `null`, because each file is its own raw blob.
- **Who hashes.** Python's stdlib has no BLAKE3, and the `blake3` module is not installed on this host. `worker_source.py` therefore fills the field only when `import blake3` succeeds; otherwise it stays `null`, never a substitute. The Rust `lan-peer manifest` command is the hashing path, because Rust already links iroh-blobs and its BLAKE3, so the two cannot disagree. It emits the same schema and the same SHA-256 receipt framing as the Python capture. The test checks that the receipt and every file's size, exec bit and SHA-256 are equal to `worker_source.py`'s for the same files. Selection policy stays in Python: `manifest` applies only structural path safety, not Python's credential/output denylist. Use `worker_source.attach_iroh_blobs_hashes(python_manifest, rust_manifest)` to merge. It refuses any difference in schema, receipt, path, size, exec bit or SHA-256.
- **Store layout.** Upstream `iroh_blobs::store::fs::FsStore` lives at the caller-given `--store` directory (its `blobs.db` plus data files). There is no default or home path. GC is off (the upstream default). Each received blob gets the named persistent tag `task-manager/source/<hex>`, so it survives restarts and repeated pushes stay idempotent. The "which of these do you have" query is `blobs().has(hash)` over the manifest's distinct hashes. The empty blob is always present in an iroh-blobs store, so it is never sent.
- **Protocol** (new ALPN `task-manager/source/0`; `serve` only accepts it when `--store`/`--checkouts` are given, and the ping ALPN is unchanged):
  1. `Hello{attestation}`, then admission (rejected before a challenge if unattested, wrong account or endpoint, or not allowlisted).
  2. `Challenge{nonce}`.
  3. `Request{body, sig}`. The body is JSON `{checkout, manifest}` and the signature is over `task-manager/source/request/v0`, the nonce, both endpoint ids and `SHA-256(body)`. The server then validates the manifest (paths, sort/duplicates, file/dir conflicts, hex hashes, one size per hash, receipt, limits) and checks that the checkout name is free.
  4. `Missing{hashes}`, the sorted hex hashes the store lacks.
  5. For each missing hash, in order, `Blob{hash, size}` followed by exactly `size` raw bytes. The server checks the hash is the next expected one and the size matches the manifest, BLAKE3-verifies the bytes and only then imports them. A mismatch answers `Rejected{HashMismatch}`, stops the stream and imports nothing for that blob.
  6. The server materializes, then sends `Response{body, sig}` signed over the response domain, nonce, both ids and both body hashes. The client verifies that freshness.

  Limits default to the Python CEILING: 20 000 files, 64 MiB per file, 512 MiB total, depth 32. The request frame is capped at 32 MiB and the session at 300 s.
- **Materializer.**
  - Validates the checkout name (`[A-Za-z0-9._-]{1,64}`, no leading dot) and the manifest.
  - Refuses an existing name, including a symlink or dangling symlink.
  - Fetches and BLAKE3-checks every blob from the store before touching disk.
  - Creates a private `.tmp-NAME-RANDOM` directory and writes through `openat` descent with `O_NOFOLLOW|O_DIRECTORY`, creating files with `O_CREAT|O_EXCL|O_NOFOLLOW`.
  - Sets modes 0755/0644 from the exec bit, then fsyncs.
  - Re-reads every file to verify size, mode, BLAKE3 and SHA-256.
  - Publishes with `renameat2(RENAME_NOREPLACE)`, so checkouts are immutable.
  - On any failure it removes only its own temp directory.

Validation (owner `osv-blob-transfer`, offline, `-j 2`, through the shared coordinator):

- Job 122 (red): the iroh-blobs fs store calls `tokio::task::block_in_place` but does not enable tokio's `rt-multi-thread` feature itself. This experiment now enables it.
- Job 123 (red): the new tests failed to compile because `source` did not exist.
- Job 124: a test borrow error.
- Job 125 (green, `cargo test --offline -j 2`): the first offline resolve, which updated `Cargo.lock` to `iroh-blobs 0.103.1`. It passed 19 tests: `lan_identity` 4, `lan_peer_process` 1, `parent_contracts` 10 and `source_transfer` 4, with 0 failed and 0 ignored.
- Job 128 (`sh -c 'cargo test --offline --locked -j 2 && cargo clippy --offline --locked -j 2 --all-targets -- -D warnings'`) passed the same 19 tests, and clippy was clean.
- Python: job 126 was red (3 new tests erroring, before implementation), and job 127 passed all 31 `tests/test_linux_worker_contracts.py` tests.

`tests/source_transfer.rs` covers:

- **Two-process loopback** (`serve` plus separate `init`/`manifest`/`push-source` processes, each with its own installation):
  - The first push sends every distinct non-empty blob (3 of 5 files, with one duplicate and one empty).
  - Editing one file sends exactly one blob.
  - Renames and moves send zero blobs.
  - Checkout trees, contents and modes match the source.
  - Pushing to an existing checkout name is refused with `Exists`.
  - A same-account but non-allowlisted peer is refused with `NotAllowlisted` before any transfer.
  - A `../` manifest path is refused with `UnsafePath` before any blob is requested.
  - No temp directories remain.
- **In process:**
  - A tampered blob (correct claimed hash, flipped byte) is rejected with `HashMismatch`. Neither blob is stored and no checkout is created; an honest retry then succeeds.
  - The materializer refuses `..`, absolute, `.`, empty, `//`, trailing slash, backslash, NUL and newline paths, duplicates, file/dir conflicts, unsafe checkout names, a symlinked destination (nothing is written through it), a wrong SHA-256 (caught by post-write verification) and an absent blob.
  - The source reader refuses symlinked files and directories and escapes.

Limitations (NOT proven):

- Loopback only. There is no second machine, no LAN confinement proof and no interrupted or resumed transfer.
- Whole blobs are buffered in memory: up to 64 MiB per blob, and the materializer holds all checkout contents at once. That is fine for bounded synthetic trees, not for large repositories.
- There is no bao/verified-range streaming. The iroh-blobs wire protocol (`get`/`push` with incremental verification and resumption) is not used, only its store and hash. Switching to it over a second stream of the same authenticated connection is the natural next step.
- The manifest travels as a JSON byte array, about 4x inflated, hence the 32 MiB frame cap.
- There are no store quotas, eviction, GC policy or per-peer accounting. Any allowlisted peer can fill the store up to the per-session limits, repeatedly.
- Checkout cleanup and retention are not implemented. Failed sessions can leave verified, tagged blobs in the store (content-addressed, so reusable).
- Temp-directory cleanup is path-based and best effort. The checkout root itself is opened following symlinks (caller's choice), and a hostile same-UID writer is out of scope.
- The `blake3` Python path is exercised only with a test double, because the module is not installed. The existing Python test that asserts `null` hashes would need adjusting on a host that has `blake3`.
- The account is still the synthetic public-mnemonic account, and allowlists and revocations are still files plus memory.

Next steps (out of scope here):

- **Execution envelopes.** A signed approval that binds coordinator/worker endpoint, attempt id, manifest receipt and checkout, argv/cwd contract, resource limits and output policy. Account membership never authorizes execution.
- **Remote job submission** into the worker's existing native admission and SQLite ledger, with durable attempt reconciliation (no replay or failover after disconnect).
- **Log streaming** with sequence numbers.
- **Artifact return** as content-addressed blobs in the reverse direction.
- **Cancellation semantics** across reconnects.
- **Transfer improvements:** bao streaming/resume and store quotas/GC.

## Stable sync and artifact return (phase 2b) (`src/sync.rs`, `src/prune.rs`, `src/artifact.rs`)

Owner `osv-blobs`, branch `feature/osv-blobs`. Goal: hash when a job comes, push only changed
files into a warm worker checkout, build there over the existing ssh `tasks run`, and bring
artifacts back the same content-addressed way. Nothing received is executed.

```text
lan-peer sync-push --peer ID --addr IP:PORT --data-dir DIR --allow FILE \
                   --source DIR --manifest FILE --checkout NAME [--bind IP:PORT]
lan-peer fetch     --peer ID --addr IP:PORT --data-dir DIR --allow FILE \
                   --checkout NAME --path REL [--path REL]... --store DIR --dest NEW_DIR [--bind IP:PORT]
```

- **Sync mode is a request kind on the existing source ALPN** (`task-manager/source/0`): the
  signed request body gains `"mode": "sync"`. The field is omitted for immutable requests, so
  phase-2 request bodies, signatures and the immutable materializer are unchanged (an existing
  name is still refused there). In sync mode the worker:
  - prechecks before requesting any blob: checkout name, manifest (paths, receipt, limits),
    every path acceptable as source by worker_source's rules, and no path through a
    marker-adjacent `target`/`build` (by the manifest's own markers);
  - takes `flock` on `CHECKOUTS/.lock-NAME` (waits up to 120 s, else `Busy`), so two syncs of
    one checkout never interleave, also across server processes;
  - creates `CHECKOUTS/NAME` (0700) if absent; refuses it if it is a symlink;
  - inspects each manifest file no-follow: same size, exec bit and BLAKE3 means **unchanged**
    (not rewritten: mtime and inode are kept; a drifted mode such as 0664 is fixed with
    `fchmod`, which keeps mtime);
  - fetches and BLAKE3-verifies every needed blob from the store before touching the checkout;
  - prunes with `prune.rs`, a line-by-line port of `offload_agent.py` + `worker_source.py`;
  - replaces changed/new files through `.tmp-sync-RANDOM` (`O_CREAT|O_EXCL|O_NOFOLLOW`, mode
    0644/0755, fsync) and `renameat` (a symlink at the name is replaced, never followed);
    descent refuses symlinked and marker-adjacent output directories;
  - re-reads and verifies every manifest file (size, mode, BLAKE3, SHA-256), then answers with
    signed counts: `created, files, unchanged, written, written_bytes, mode_fixed, deleted,
    deleted_paths (first 200), kept_non_source, blobs_received, bytes_received`.
- **Prune parity.** `tests/sync_checkout.rs` asserts the Rust rule constants equal the Python
  ones (by running Python), and runs the real `offload_agent.py apply` and `prune.rs` on two
  copies of one fixture covering every rule (top-level and marker-adjacent outputs, preserved
  cache links, excluded names/prefixes/suffixes, case-insensitivity, symlinks as leaves,
  emptied directories, C1/DEL control characters and a non-UTF-8 name). Pruned lists, kept
  counts and resulting trees must be identical.
- **Artifact return is a new ALPN** `task-manager/artifact/0` (served with `--store
  --checkouts`): the same `Hello` → admission → `Challenge` → signed `Request{checkout,
  paths}`; the worker validates paths (structural rules, inside the checkout, no-follow; a
  requested symlink is refused, symlinks/special files inside requested directories are
  skipped and listed; hardlinks allowed because Cargo hardlinks binaries), imports the files
  tagged `task-manager/artifact/<hex>`, and replies with a signed `{checkout, manifest,
  skipped}` (domain `task-manager/artifact/response/v0`). The client verifies the signature,
  validates the manifest itself, rejects any path outside the requested ones, sends
  `Missing{hashes}` for what its own FsStore lacks, checks each `Blob` header's order and
  size, BLAKE3-verifies before import, and materializes into a NEW directory with the
  immutable materializer. `fetch` holds an `flock` on `STORE/.lan-peer-fetch.lock`, because
  the store's database is single-process.

Validation (owner `osv-blobs`, offline, `-j 2`, through the shared coordinator): see the
report in `feedback/osv-blobs-*.md` for job ids. Limitations, not proven here: no resumable
or range-verified (bao) transfer; whole blobs buffered; no store quotas or GC; sync is not
atomic as a whole tree (a crash leaves a mix that the next sync repairs); the client side of
`tasks-offload --transport iroh` refuses in-tree symlinks; the account is still the synthetic
public-mnemonic account and attestations expire after 7 days.
