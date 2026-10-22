# Linux ↔ Mac iroh transport check (no SSH, no execution)

This is the first connection check, NOT a Mac worker or remote-build acceptance.
The regular Mac daemon/CLI still refuses local runtime before peer startup.
`crates/peer/examples/status_probe.rs` embeds the **existing** peer library's LAN
endpoint, allowlist, signed status handshake, status hub and server accept loop.
It does not implement a second transport or bypass the daemon guard.

Only `task-manager/status/0` is advertised. All allowed identities get observer
permissions; jobs/source/artifact services are absent and their ALPNs cannot
negotiate. The diagnostic snapshot explicitly says `probe.kind=transport-only`
and `execution_enabled=false`, with local OS/architecture. It is not the daemon's
machine/admission snapshot and must not be presented as live worker/UI status.
No arbitrary command is executed. Relays, discovery and port mapping are disabled.
Both machines must have direct LAN/routed UDP reachability. No firewall/service
changes are performed; diagnose/request permission rather than changing them.

## 1. Linux: build from a stable checkout of origin/dev-macos

Use a separate worktree if the current checkout is busy. Do not switch/reset a
checkout while another build is running. Fetch the published branch normally.
From that worktree:

```sh
tasks list
tasks admission
tasks run --who linux-iroh-status -- \
  cargo build --locked -p task-manager-peer --example status_probe -j 2

umask 077
PROBE_DIR=$(mktemp -d "${TMPDIR:-/tmp}/tasks-iroh-status.XXXXXX")
PROBE="$PWD/target/debug/examples/status_probe"
"$PROBE" init "$PROBE_DIR/peer.key"
```

`init` prints ONLY the public endpoint ID. Send that public ID to the Mac
operator through a trusted channel. Never paste/read/send `peer.key`; it is a
new random 0600 disposable identity. Existing files are never overwritten.
Keep these shell variables for the watch command. Do not reuse production keys.

## 2. Mac: allow the Linux identity and serve briefly

The Mac bootstrap uses the operator-approved direct Cargo path (`-j 4`), since
no installed tasks exists. The example must first be built locally:

```sh
cargo build --locked -p task-manager-peer --example status_probe -j 4
```

Use a private disposable directory (`umask 077; mktemp -d`), or the existing
probe session prepared by the Mac operator. Its public ID can be derived again
without printing the secret by starting a bounded server. Never run init over an
existing key or touch `~/.config/tasks` to configure this diagnostic.

```sh
# MAC_KEY is the disposable key, MAC_IP the actual LAN interface address.
# LINUX_ID is the verified public ID printed in step 1.
target/debug/examples/status_probe serve "$MAC_KEY" "$MAC_IP:7843" 600 "$LINUX_ID"
```

The ready JSON prints the public endpoint ID, actual bound address, zero relay/
lookup counts, allowed-peer count and `execution_enabled=false`. With no ID
arguments the server is deny-all, useful only for bind/refusal checks. It never
trusts an identity merely because it connects or claims a name. Duration is
required (1–3600 seconds); it exits on expiry or Ctrl-C. Keep it running during
step 3. Use an unused UDP port; do not stop an existing service to take its port.

## 3. Linux: request three signed diagnostic snapshots

```sh
# Set LINUX_IP to the LAN source address shown by: ip -4 route get "$MAC_IP"
# MAC_ID is the verified public endpoint ID supplied by the Mac operator.
"$PROBE" watch "$PROBE_DIR/peer.key" "$LINUX_IP:0" "$MAC_ID" "$MAC_IP:7843" 3
```

Expect three JSON snapshot events and exit 0. Wrong/unlisted client identities
must be refused; the requested server identity is authenticated by iroh/TLS.
A status error, timeout or missing snapshots is nonzero. There is a bounded
120-second total watch deadline and at most ten requested snapshots. No SSH,
public discovery or implicit relay fallback is involved. Keep logs outside Git.

To check the reverse direction, let the first server end, swap roles and use
the same verified disposable public IDs. Prefer one live endpoint per identity.
The status observer pairing cannot authorize builds, source writes or fetching
artifacts. This check leaves private test keys in the disposable directories;
it does not install credentials or modify existing pairings. Retain them only
for the agreed test session; do not commit them.

## What this proves / what remains

Native loopback tests exercise actual iroh UDP/QUIC, signed status exchange,
unknown-identity refusal and disabled jobs/source/artifact ALPN negotiation.
A successful interface bind alone is NOT evidence that Linux can reach the Mac.
Record actual cross-machine results separately; no Linux handshake is claimed
until step 3 really runs. Mac firewall prompts require the owner's decision.

After connection: integrate the minimum Mac worker adapters and run isolated
submit/approve/admission/log/cancel/recovery checks, then request a task-manager
build from Linux. UI linking/launch is a subsequent explicit check. Do not
remove runtime guards or lower reserves merely because transport works. No
built binary replaces/restarts its running worker automatically. Tinline,
Xcode/iOS setup and signing are separate later milestones.
