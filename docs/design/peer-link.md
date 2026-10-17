# Peer link: daemon-to-daemon forwarding over iroh

Status: implemented on branch `feature/peer-iroh`, tested hermetically (two real daemons on
loopback, see "Tests"). **Not deployed, not run between the laptop and the worker.** The
LAN end-to-end run is a script for the operator: `scripts/peer-lan-e2e.sh`.

Architecture: agents → (later, MCP) → the local `tasks` daemon; daemon ↔ daemon over iroh.
This replaces `scripts/tasks_offload.py` + ssh for running a command on another machine.

## Components

| Where | What |
|---|---|
| `crates/peer` (`task-manager-peer`) | The iroh component, moved from `experiments/osvauld-foundation`: LAN-only endpoint, config/allowlist, signed session handshake, ALPNs `status/0`, `jobs/0`, `source/0`, `artifact/0`, file selection, in-place sync, prune rules, materializer. No dependency on the coordinator. |
| `src/peer/runtime.rs` | Started by `tasks daemon` **only when a peer config exists**, in its own OS thread and tokio runtime. Hosts the endpoint, the local bridge socket `<state>/peer.sock` and a 1 s watcher. It is a *client* of the daemon's own `taskd.sock`; the unix-socket request threads never run iroh code. |
| `src/peer/host.rs` | Receiving side of `jobs/0`: a peer's submission becomes an ordinary submission on the daemon socket. |
| `src/peer/forward.rs` | Sending side: one session per bridge connection (sync, submit, attach, re-attach, fetch). |
| `src/peer/bridge.rs` | `tasks peer-forward`, the command of a forwarded job. |
| `src/peer_link.rs` | Job contract (`ForwardArgs`), the seam `forwarded_spec(spec, peer, ...)`, the ledger link `PeerLink`, `Action::Peers` reply types. |

The `lan-peer` experiment is **kept unchanged** (`experiments/osvauld-foundation`): it still
builds as its own workspace and still serves `tasks-offload --transport iroh` and the UI's
`lan-peer status-watch`. It is superseded by the daemon for anything new; see "What is now
redundant".

## Configuration

`TASKS_PEER_CONFIG`, else `$XDG_CONFIG_HOME/tasks/peer.toml`, else `~/.config/tasks/peer.toml`.
No file: the peer link is disabled (`tasks peer list` says so); nothing binds. `tasks peer init
--bind IP:PORT` writes a template and a new key (`peer.key`, 0600, 64 hex; a group/other
readable key is refused). Changes apply when the (idle) daemon restarts.

```toml
secret_key = "peer.key"            # relative to this file
bind = "192.168.1.10:7842"         # explicit interface address; 0.0.0.0 / :: refused
# checkouts = "~/offload/checkouts"  # where host peers' synced checkouts land
# store = "/path"                   # iroh-blobs store (default <state>/peer/blobs)
# reattach_timeout_s = 3600         # how long a lost link stays UNCERTAIN before giving up
# connect_timeout_s = 10
# status_interval_s = 2
# heartbeat_s = 5                   # attach streams: heartbeat after this much silence
# link_stall_s = 20                 # no frame for this long = link lost

[[peer]]
name = "worker"                    # [A-Za-z0-9._-]{1,64}
endpoint_id = "<its `tasks peer id`>"
addr = "192.168.1.7:7842"          # where to dial it (role worker/both)
role = "worker"                    # worker | host | both | observer
# approve = "auto"                 # inbound jobs from a host peer: auto | manual
```

Roles describe the **peer**: `worker` runs jobs I forward (inbound it may only read my status);
`host` forwards jobs to me (status, jobs, source sync, artifact fetch); `both`; `observer`
(status only). Unknown endpoints and role mismatches are refused before a challenge is issued
and logged in `taskd.log` as `peer: REFUSED <who> on <alpn>: <reason>`.

**Why the allowlist and not the account attestation.** The experiment admitted a peer when it
showed a courier attestation from the synthetic, publicly derivable test account AND was in a
local allowlist. The attestation proved nothing a holder of the public mnemonic could not forge,
and it pulled machine-local path dependencies on the osvauld parent into the build. The
TLS-proven endpoint id (iroh handshake) plus an explicit per-machine allowlist is the gate that
actually decided admission there, and it is the only one here. Account binding/revocation is
future work, not silently claimed.

## Protocols (all ALPNs share the experiment's framing)

`Hello{attestation: ignored}` → allowlist + role check → `Challenge{nonce}` → `Request{body,
sig}` signed by the requester's endpoint key over (domain, nonce, both ids, SHA-256(body)) →
`Response{body, sig}` signed over (domain, nonce, both ids, both body hashes). Wire shapes of
`status/0`, `source/0` and `artifact/0` are the experiment's.

* `task-manager/status/0`: the daemon's own `tasks status --json` snapshot, pushed every
  `status_interval_s` while someone is subscribed (sampled via the daemon socket).
  `tasks peer status-watch NAME` prints the peer's stream as JSON lines (same live/marker
  shapes as `lan-peer status-watch`), usable as `TASKS_UI_REMOTE_CMD`.
* `task-manager/source/0` (role host/both), sync mode only: manifest of the checkout →
  `Missing{hashes}` the receiver's blob store lacks → only those blobs, BLAKE3-verified →
  in-place update of `checkouts/<id>` under the per-checkout `flock` (unchanged files keep
  mtime/inode, changed ones are replaced by rename, absent ones pruned with
  `offload_agent.py`'s rules, outputs never touched) → signed counts. The request carries the
  sender's `{host, toplevel}`; the checkout name must equal `checkout_id(host, toplevel)` and is
  recorded in `<id>.identity` with the peer's endpoint. A checkout owned by another peer is
  refused.
* `task-manager/jobs/0` (role host/both): `submit` (idempotent per key), `attach` (log from
  an offset + phase changes + heartbeats + `Done{phase, exit_code}`), `cancel`, `lookup`. A peer
  sees and cancels only jobs it submitted.
* `task-manager/artifact/0` (role host/both): files of the peer's own checkout, by path,
  content-addressed into a NEW local directory.

File selection (sender): `git ls-files -z -c -o --exclude-standard` in the Git top level, then
the worker_source rules (credential/cache/output names excluded and reported; invalid paths
errors), no-follow descent with the build-marker output rule, tracked-but-deleted skipped,
in-tree symlinks/submodules/special files refused. Same as `tasks-offload` (the iroh path of
which also refused symlinks). Checkout id: `sha256(hostname NUL toplevel)[:16]`, so a forwarded
job reuses the worker's warm checkout of the same laptop path.

## Forwarding

`tasks run --peer NAME [--run | --device SERIAL] [--fetch REL]... -- CMD` (also `enqueue`):

1. The client asks the daemon (`Action::Peers`) whether the peer link is enabled and NAME is a
   worker/both peer, then submits a **local** job: `local.command`, resource **offload** (the
   offload slots; no compile/run slot here), argv `tasks peer-forward --state DIR --peer NAME
   --who OWNER --resource compile|run|device:S [--fetch REL]... -- CMD`. Normal approval,
   admission, log, cancellation. The local device denylist applies to the whole argv. The
   inner command's Cargo `-j` is the peer's business (validated there).
   Seam for placement/MCP: `peer_link::forwarded_spec(&inner_spec, peer, fetch, state, exe)`.
2. At admission, the worker runs the bridge, which sends only its `TASKS_JOB_ID` to
   `peer.sock`. The daemon reads the forward arguments from the approved ledger record (the
   bridge cannot widen them), checks that the job is active and targets this state dir, then:
   takes the same per-checkout client lock as `tasks-offload`
   (`~/.local/state/tasks-offload/locks/<id>.lock`), syncs, and submits with a key
   `host:job:random` written to `<state>/peer/links/<job>.json` first.
3. The receiving daemon validates exactly like `tasks enqueue` (argv bounds, nested helpers,
   its own Cargo `-j` cap, its own device denylist incl. `ZD22257P6G`), submits on its own
   socket with owner `WHO@sender-host` and cwd `checkouts/<id>/<sub>`, records the key
   (`submitting` before the insert, `submitted` + id after), and approves + starts its worker
   only if the peer is `approve = "auto"` (the equivalent of the ssh `tasks run` that
   `tasks-offload` sends). With `manual`, the job waits for `tasks approve-command` there.
4. The sender attaches by remote id and streams the peer's job log to the bridge, which
   prints it on stdout (so it lands in the local job log); notes go to stderr (`tasks peer:`).
5. Result: passed 0; failed with the peer's exit code; cancelled on the peer 130 (local job
   fails, link `remote_phase: cancelled`); peer interrupted 255; refused by the peer
   (validation, denylist, unknown checkout) 2; peer offline before submission 255 with "nothing
   was submitted"; still UNCERTAIN after `reattach_timeout_s` 255; fetch failure 1.
6. Links: live in `<state>/peer/links/<job>.json` (sender) and
   `<state>/peer/inbound/<endpoint>/<hash>.json` (receiver); once terminal, both ledgers get
   `execution.peer` (`direction`, `peer`, `peer_endpoint`, `peer_job_id`, `owner`, `key`,
   `remote_phase`, `remote_state`, `sync_s`, `fetch_s`) via a JSON-level merge like
   `metrics`. The sender's snapshot fills `jobs[].offload = {worker: NAME, remote_job_id,
   transport: "iroh", sync_s, fetch_s}`, so the UI pairs the two jobs as for `tasks-offload`;
   the receiver's job shows the sender's checkout path through `<id>.identity`.

### Failure semantics

* **Lost link mid-run** (no frame for `link_stall_s`): remote state `uncertain` in the link
  record and a `tasks peer: UNCERTAIN ...` note; the local job keeps running (not failed). The
  session re-attaches **by remote id** from the last delivered log offset (1 s doubling to 5 s);
  nothing is resubmitted. After `reattach_timeout_s` without success: exit 255, `uncertain`.
* **Lost submit reply**: retried with the same key; the receiver returns the existing job
  (`found the already submitted job N; not resubmitted`). A receiver that crashed between its
  intent record and the ledger insert reconciles by exact spec match (one candidate adopted,
  several refused as UNCERTAIN, none submits). Never two jobs per key.
* **Receiving daemon restart**: its worker keeps running jobs; the sender sees a lost link,
  then re-attaches when the endpoint is back (same key/port, records on disk).
* **Sending daemon restart**: the bridge (owned by the local worker, not the daemon) keeps the
  job alive, reconnects to the new daemon's `peer.sock` and re-attaches from the byte it last
  printed. A session that dies before the remote id is known looks the key up on the peer first.
* **Sending worker loss** (bridge killed without a cancel): the local job is reconciled as
  interrupted as usual; the watcher marks the link `orphaned` and does NOT cancel the remote
  job (no automatic replay or cancel on uncertainty).
* **Cancel**: local `tasks cancel` → the bridge asks the daemon to cancel the remote job before
  the SIGKILL grace ends, and the watcher repeats the remote cancel every second until the peer
  reports a terminal phase (covers a bridge killed first). The peer's worker stops the job's own
  process group. A cancel before submission stops the session before it submits.

## Socket compatibility

New action `{"type":"peers"}` answered with its own reply shape (`PeersResponse`), like
`status`. Older daemons answer "unknown variant `peers`"; `tasks peer list` and
`tasks run --peer` then say the daemon predates peer forwarding and submit nothing. Existing
actions and replies are unchanged; `execution.peer` is an additive optional key (older
clients ignore it). No SQLite schema change.

## Tests

Hermetic, in `cargo test --workspace` (no network beyond loopback, relay/discovery off):

* `tests/peer.rs` — two real `tasks daemon`s with their own HOME, state, socket, lock dir and
  config, real workers and admission (fixtures relax only the memory/disk reserves and the
  run-slot CPU gate):
  `status_stream_reaches_an_allowlisted_peer_and_refuses_an_unknown_one`,
  `forwarded_jobs_run_on_the_peer_stream_logs_mirror_exit_codes_and_link_both_ledgers`,
  `source_sync_sends_only_missing_blobs_prunes_deletions_and_honours_path_rules`,
  `cancel_on_either_side_propagates`,
  `lost_link_is_uncertain_then_reattaches_and_restarts_reattach`,
  `denylisted_devices_are_refused_by_the_peer_and_unreachable_peers_are_clear_errors`,
  `submit_is_idempotent_per_key_and_scoped_to_the_submitting_peer` (in-process host endpoints
  against a real daemon: same key twice = one job; another peer cannot see/attach/cancel it or
  use its checkout).
* `tests/peer_protocol.rs` — frozen pre-change protocol types: old client ↔ new daemon, new
  client ↔ old daemon (unknown-variant fallback, nothing submitted).
* `crates/peer` — config/allowlist, wire shapes, selection vs the Python rules and checkout
  id, in-place sync, symlink/excluded refusals, per-checkout lock, prune parity with
  `offload_agent.py` (runs Python).

LAN end to end: `scripts/peer-lan-e2e.sh` (operator-run; see its header).

## What is now redundant, and what is missing before deleting `tasks_offload.py`

Redundant for a forwarded job: ssh execution (`ssh worker tasks run`), rsync + the installed
`offload_agent.py` helper (prune/verify), `lan-peer sync-push`/`fetch`, the job sidecar written
by the client (the daemon keeps the link), `status-watch-ssh` / `lan-peer status-watch` for the
UI (`tasks peer status-watch NAME`), the `lan-peer serve` unit on the worker.

Still missing before `scripts/tasks_offload.py` can go:

* deployment of a peer-capable `tasks` on both machines and a worker unit with the peer config
  (and the firewall's UDP port; today only 7842, used by `lan-peer serve`);
* in-tree symlinks (`--transport ssh` syncs them; the peer manifest carries regular files only);
* `--cargo-fetch` (online `cargo fetch --locked` on the worker before an offline build);
* `--fetch-dest` (fetch goes to `~/build-cache/offload/WHO/LOCAL_JOB_ID`);
* `offload-portable-desktop.sh` still calls `tasks-offload`;
* the one-line JSON summary on stdout that scripts parse (the bridge prints the log and notes);
* large trees: whole blobs are buffered in memory (64 MiB per file, 512 MiB per sync; Python
  allowed 1 GiB / 4 GiB) and there is no bao streaming/resume;
* a real account binding/revocation for endpoints (allowlist files only).

## Limitations

* Loopback only so far; no LAN, multihomed, VPN or IPv6 run. Endpoint binds one explicit IPv4/
  IPv6 address, no relay, no discovery.
* Admission of the bridge uses an offload slot; the forwarded command's admission is the peer's.
* The re-attach window, heartbeat and stall are per-daemon settings, not per job.
* A bridge killed without SIGTERM while the remote job runs leaves it running (`orphaned`).
* `approve = "manual"` jobs wait on the peer; the sender shows "WAITING FOR APPROVAL".
* Link records are not pruned (small JSON per forwarded job).
* No store quotas or GC for the blob store; checkouts are not cleaned up.
