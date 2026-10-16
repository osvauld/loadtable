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

**What this does NOT prove.** No real two-machine run has been done yet (loopback only). There is no LAN confinement proof, no multihomed/VPN or IPv6 test, and no iroh-blobs transfer. The account is synthetic and shared from a public mnemonic. Revocations and allowlists live in memory and in command-line files only. No command execution, approval envelope, cancellation or attempt reconciliation exists. Attestation expiry still compares the issuer's clock with the receiver's. A debug binary is not a reproducible, versioned package.

**iroh-blobs (next gate, offline check only, not integrated).**
- Cached `iroh-blobs 0.103.1` requires `iroh ^1.0.0` and `iroh-base ^1.0.0`, which semver-match the iroh 1.3.0 in use. All of its direct dependencies are in the offline cache, including `rand 0.10`, `irpc 0.17`, `bao-tree 0.16`, `redb 4` (optional, for fs-store) and `iroh-tickets 1.0`.
- A reference lockfile (`/home/abe/p2p_chat/Cargo.lock`, read only) resolves `iroh-blobs 0.103.1` together with `iroh 1.3.0`.
- Cached `iroh-blobs 0.98.0` needs iroh 0.96 and is NOT compatible.
- Not verified: a full offline resolve and build in this workspace. Its `fs-store` feature uses redb 4, while the parent `storage` crate uses redb 2; both versions would coexist.
