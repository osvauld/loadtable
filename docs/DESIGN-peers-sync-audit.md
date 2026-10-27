# Design: peer identity, git sync over iroh, audit and attribution, scheduling

Status: decided 2026-10-22 (§12 records the answers). Nothing here is built yet. The daemon, peer
forwarding state machine, CLI and MCP crates already exist; this replaces their transport,
sync and trust layers and adds audit, attribution and priority.

## 1. Goals

- Machines talk only over iroh: direct on the LAN, through a relay otherwise. No SSH,
  no `tasks-offload`, no worker reached by IP.
- Trust is an identity, not an endpoint. A machine is a DID with a signing key; its iroh
  node key is bound to that DID. Pairing is a ticket you copy from one machine to the other,
  like p2p_chat contacts and osvauld2 invites.
- Sync sends only what changed, reads only what changed, and ties every forwarded job to an
  exact git tree id.
- Every stall (I/O, memory, CPU) and every peer action can be explained afterwards from the
  ledger: who, what, how many bytes, which job.
- Priority classes and a short lane, decided from measured history rather than flags.

Non-goals for now: more than one human owner, untrusted third-party workers, hermetic
builds, Git LFS and submodules (refused with a named reason).

## 2. Identity

Reuse the osvauld identity contract (osvauld2 `identity/`, p2p_chat `crates/identity`):

| key | curve | used for |
|---|---|---|
| signing | Ed25519 | signatures on tokens, tickets and audit rows; the DID |
| device | Ed25519 | the iroh endpoint key (node id) |
| encryption | X25519 | not needed yet; derived anyway so the contract matches |

- One identity per machine, generated on first `loadtable daemon serve` from 32 random bytes
  through the same BIP39 + HKDF derivation (`osv/identity/*/v1`), so the existing crates'
  golden tests apply and the mnemonic can be backed up (`loadtable identity show --mnemonic`).
- Stored in the daemon's state dir, mode 0600, never in argv or logs. macOS: same file;
  Keychain later if wanted.
- `did:key:z…` is the machine's stable name in the ledger, audit log and status; the
  human name ("kurukkan", "nixos") is a label.

Decided (D1): carry our own copy for now, in a `tasks-identity` crate: the same
derivation contexts and DID encoding as osvauld2/p2p_chat, with their golden vectors copied
into tests so the copies cannot drift unnoticed. Moving to a shared crate later changes no
key or DID.

## 3. Grants and roles

Follow osvauld2 `courier` tokens: a node-signed claim `{iss, aud, sub, role, scope, nonce,
iat, exp, binds}` where `binds` pins the holder's device key (iroh node id).

Roles a machine can grant another:

| role | allows the holder to |
|---|---|
| `submit` | send jobs to me, sync checkouts to me, fetch outputs of its own jobs |
| `observe` | read my status, queue and logs of its own jobs |
| `admin` | pair, revoke and change my peer policy (only the owner's other machines) |

"I may send jobs to the worker" is the worker granting the laptop `submit`. Two-way use
needs a grant each way, made by one pairing (§4) that asks for both.

- Every live grant is a ledger row; revoking deletes nothing, it marks the row revoked and
  writes an audit row. A revoked or expired grant fails the next request, and live
  connections holding it are closed.
- Grants expire (default 1 year) and are re-issued silently over a live connection before
  expiry; an offline machine past expiry needs a new pairing.

## 4. Pairing ticket

```
loadtable pair invite --role submit,observe [--both-ways] [--ttl 15m]   # on the worker
  -> tasks1.eyJ2ZXJzaW9uIjox…   (one URL-safe word; QR optional)
loadtable pair accept tasks1.eyJ…                                         # on the laptop
```

Ticket body (JSON, versioned prefix `tasks1.` so a newer ticket is refused by name):
`version, node_did, device_public_key, node_id, name, relay, direct_addrs, invite_token`.
The invite token is signed by the inviter and carries role, scope, a single-use nonce and
an expiry (default 15 minutes).

Redeem:
1. The laptop dials `node_id` (direct addrs first, then the relay) and checks the endpoint
   key equals the ticket's `device_public_key`.
2. It sends the invite token, its own DID, device key and a signature over a fresh
   challenge from the worker (proof it holds the DID's key).
3. The worker checks the token signature, expiry and that the nonce is unspent, persists
   the nonce as spent, mints the grant bound to the laptop's device key, and records the
   peer. With `--both-ways` the laptop mints the reverse grant in the same exchange.
4. Both sides write audit rows. The ticket is now useless.

Like p2p_chat's ticket rotation, `loadtable pair reset` invalidates every unredeemed ticket.

## 5. Connection

- One iroh endpoint per daemon, key = device key, ALPN `tasks/1`. Relay: iroh's default
  relays to start; a self-hosted relay URL in machine policy later (D2).
- Discovery: the ticket's direct addresses and relay; afterwards each side remembers the
  other's latest addresses, plus iroh DNS/pkarr discovery so a moved machine is found.
- Admission happens before any request is read: iroh authenticates the node id, then the
  first frame presents the grant; the receiver checks signature, expiry, revocation and
  that `binds.device` equals the connection's node id. Each request is checked against the
  grant's role.
- The status snapshot shows per peer: DID, name, path (direct / relay), round-trip time,
  grant expiry. Relay-only links are fine for logs and status; placement charges sync cost
  by measured throughput, so a slow relay path naturally keeps big syncs local.
- Removes: SSH, `tasks-offload`, `status_watch_ssh.py`, the worker's `authorized_keys`
  entry (user step), and the old `lan-peer` blob store.

## 6. Sync: git over iroh

### Snapshot on the origin
1. Copy the checkout's index to a private temp index (`GIT_INDEX_FILE`), run
   `git add -A` into it. Git's stat cache means only changed files are read and hashed.
2. `git write-tree` gives the tree id of the exact working tree, uncommitted changes
   included. Gitignored files are not in it.
3. New objects go to a tasks-owned object dir (`GIT_OBJECT_DIRECTORY`) that lists the
   repo's own objects as an alternate, so the user's repo, index, refs and stash are never
   written.

The job records `tree`, the HEAD commit it was based on and the dirty-file count.

### Transfer
- The worker keeps one bare mirror per (peer DID, repo) and one working copy per
  (peer DID, checkout path), as today's store does.
- Negotiation: the worker reports the trees/commits it already has for that checkout; the
  origin runs `git pack-objects --thin --revs` excluding those, streams the pack over one
  iroh stream with a byte limit; the worker runs `git index-pack --fix-thin`.
- Apply: `git read-tree` + `git checkout-index` into the working copy inside the existing
  per-checkout lock; files that left the tree are removed only if they were tracked before,
  so build outputs stay (same rule as now). A sync while a remote job of that checkout is
  live is refused `Busy` (already built).
- Extra inputs: gitignored files that must travel (e.g. `local.properties`-style config)
  come from an explicit allowlist in the repo run policy, sent as hashed blobs. A secret
  denylist (names plus a content check for key/token patterns) refuses rather than skips
  silently.
- Refused with a named reason: LFS pointers, submodules, files over a size cap.

### Outputs and cleanup
- Outputs (APKs, binaries) come back over the existing hashed, bounded fetch, written to
  a temp file and renamed into place.
- `--fetch REL` (job spec `fetch`, paths relative to the checkout root): when a forwarded job
  passes, the origin's forward driver fetches each path inside one transfer slot, before the
  end of the job is published. Results (fetched with bytes and sha256, missing, too large,
  failed) go to the job log, a sidecar next to it that `get` reads, and one `data.fetched`
  audit row per file. Files may be written through the placed output links and no other
  symlink. The job's phase never changes; `loadtable run` exits 1 if a passed job lost a file.
- Retention: the worker drops a working copy idle for N days (default 7) and runs
  `git gc --prune=<age>` on its mirrors weekly; the origin prunes its object dir of trees
  older than the newest job per checkout. Every deletion writes an audit row with bytes
  freed.

This replaces the peer crate's `manifest`, `sync` and `store` modules; `CheckoutLocks`,
`Forward`, `outputs` and the receiver stay. Decided (D3): implement with `gix`
(gitoxide, pure Rust, so no C toolchain and the macOS cross-check keeps working) behind
`Snapshot`/`PackTransfer` traits: status with the index stat cache, object and tree
writing, pack generation, pack receipt and tree checkout. A step where gix proves to have
a real gap may fall back to the `git` command behind the same trait, named in the code.
`git2` (libgit2) is not used: it needs a C build and cannot make thin packs.

### As built (step 3)

`crates/tasks-peer/src/sync/`. Where the build differs from or fills in the text above:

- **Snapshot** (`GixSnapshot`, gix 0.73 pinned). There is no per-sync copy of the user's index.
  The tasks store keeps one bare repository per *repo* (all worktrees of a repo share it;
  `objects/info/alternates` names the user's object directory) and one index per checkout, the
  snapshot cache, rewritten after every successful snapshot. A file is read only when its `lstat`
  differs from the cache, or from the user's index when the cache has no entry. The tracked set is
  the user's index; untracked, non-ignored files come from gix's ignore-aware directory walk.
  Blobs are the exact bytes on disk (no clean filters, so no CRLF conversion or LFS); an entry
  whose mtime is within a second of the snapshot is recorded unverified (git's racily-clean rule)
  and read once more next time.
- **Wire**. `SyncOffer` (checkout, repo) answers with the worker's recent snapshots
  (`SyncHave`: commit, tree, extras tree). If the worker holds the exact tree nothing is
  packed. Otherwise the origin wraps the tree in a commit whose parent is the worker's newest
  snapshot it still has (so `pack-objects --thin` has an edge base to delta against), builds the
  pack excluding everything the worker's snapshots reach, and sends it as `SyncPackChunk`
  frames of at most 128 KiB over the connection's request/reply, hashing as it goes. `SyncApply`
  carries tree, extras tree, commit, HEAD, dirty count, pack length and SHA-256. The pack is not
  a dedicated iroh stream: one transport trait serves every message, and a stream needs a trait
  change in the fake and iroh adapters (recommended for step 4 with the transfer cgroup; chunks
  cost one round trip each).
- **Extras** are a second tree (`extras_tree`) of the allowlisted ignored files, sent in the same
  pack and applied through its own index, so a file that leaves the allowlist is removed and
  nothing else ever is. `[sync] extra = [...]` in `.loadtable/run-policy.toml` (`tasks-policy`).
- **Worker**. `<peer>/mirrors/<repo>.git` (bare) and `<peer>/<checkout>/{tree,index,
  index.extras,state.json}`. Apply is `read-tree --reset -u` into `tree/`, so a file is removed
  only if the previous index listed it, a file a job modified is restored, and untracked build
  outputs stay; untracked files or directories in the way of a path the new tree adds are
  cleared first (not through symlinks). Refs `refs/tasks/<checkout>/<ms>` keep the last eight
  snapshots of a checkout alive in the mirror.
- **gix and the `git` command**. gix: opening repos, the index, the directory walk with ignore
  rules, blob/tree/commit writing, object existence, pruning walks, mirror creation. `git`
  command (named in `git_cli.rs`): `pack-objects --thin` (no thin-pack generation in gix-pack),
  `index-pack --fix-thin` (gix-pack needs its `streaming-input` feature, which `gix` only enables
  with a network client), `read-tree --reset -u` (gix has no two-way tree update that restores
  and removes by index), refs, `ls-tree`, `diff-tree` and `gc`. Both machines therefore need
  `git` on `PATH` for transfer; nothing links libgit2 or a C library.
- **Gate**. `SyncGate::enter(SyncPhase)` is called before snapshotting, pack building and apply;
  the default admits at once. Step 4 implements it on admission so that transfers wait for the
  I/O gate. Not wired to admission yet.
- **Retention**. `SyncStore::sweep` (hourly, from the scheduler's maintenance): working copies
  idle `sync.idle_days` (7), mirrors no copy uses, `git gc` per mirror every `sync.gc_days` (7).
  `OriginSync::prune`: loose objects not reachable from the newest snapshot of any checkout.
  Each deletion is a `SyncEvent::Deleted` with bytes freed, audited as `data.retention`; sync
  traffic is `data.sync_sent` / `data.sync_received`, refusals `data.sync_refused`.

## 7. Logs

- A job's stdout/stderr go through a pipe into its log file; no fsync per write (already
  so in v2). Hard size cap per job with the watchdog (already built), and log files are
  deleted with the job row by retention.
- Forwarded jobs: the worker's log is mirrored by byte offset; a reconnect resumes at the
  offset. The origin writes the mirrored bytes the same way, no fsync.
- Ledger: SQLite WAL with `synchronous=NORMAL` (already so); audit rows share it.

## 8. Audit log

An append-only `audit` table in the ledger. Each row: time, actor DID (machine) and owner
(agent/person), action, target (job id, peer DID, checkout, path), details JSON, and
`prev_hash` = hash of the previous row, so silent edits break the chain and
`loadtable audit verify` finds them. Rows are signed by the machine's key in batches.

Recorded actions:
- pairing: ticket minted, redeemed, refused (and why), grant issued, renewed, revoked;
- connections: open, close, refused (bad grant, revoked, wrong node id), path changes;
- jobs: submitted, forwarded (peer DID, tree id), received from a peer, started, ended,
  cancelled (by whom, reason), priority raised (by whom, reason), refused by policy;
- data: sync bytes sent/received and tree id; outputs fetched (path, hash, bytes);
  retention deletions (path, bytes);
- pressure: every gate trip with the attribution snapshot (§9).

`loadtable audit [--since] [--peer] [--job] [--action] [--json]` reads it; the UI shows it.
Retention: 90 days by default, chain kept intact by recording a checkpoint row.

## 9. Attribution of stalls

### Linux: one cgroup per job
- The daemon runs as a systemd user service with `Delegate=yes` and creates
  `tasks.slice/job-<id>` per job (and `tasks.slice/transfer` for sync/fetch work).
- From each job's cgroup the daemon reads: `io.stat` (bytes, ops per device),
  `io.pressure`, `memory.peak`, `memory.pressure`, `cpu.stat`, `cpu.pressure`. These are
  stored with the job and shown in `get`/`list`/UI.
- Process-group leases and cancellation keep working; killing a cgroup (`cgroup.kill`)
  becomes the last-resort kill that cannot miss an escaped child.
- Without delegation (no systemd, containers) the daemon falls back to today's process
  groups and says so in status.

### macOS
No cgroups or PSI. Per-process disk bytes from `proc_pid_rusage` (`ri_diskio_*`), memory
pressure level from the kernel (`kern.memorystatus_vm_pressure_level`), summed over each
job's process tree by the sampler. Attribution is per process tree, not exact.

### When a gate trips
Record the machine-wide pressure, the per-job figures above, and the top 10 processes
outside any job by I/O and RSS over the last sample window (Firefox, the UI, a sync).
That answers "what caused this stall" even when no job ran. The UI shows the last trips.

### Transfers count
Sync and fetch run in the `transfer` cgroup and take a transfer slot. Unlike today's
offload slot, the transfer slot respects the I/O gate, because a sync is local disk work.

### As built (step 4)

`tasks-platform` (`cgroup/`, `sample.rs`), `tasks-runner` (`meter.rs`), `tasks-daemon`
(`attrib.rs`), `tasks-ledger` (schema v7). Where the build differs from or fills in the text:

- **Cgroup layout.** The daemon finds its cgroup in `/proc/self/cgroup` (the parent when
  `DelegateSubgroup=daemon` already put it in a `daemon` leaf), creates `tasks.slice`, moves itself
  into `daemon` (no-internal-processes rule) and enables `+io +memory +cpu` as far as the cgroup
  offers them, first in the delegated cgroup, then in `tasks.slice`. A controller that is
  missing or refused leaves its figures `None`; nothing else changes. Setup fails before anything
  moves when the first `mkdir` is refused, and the reason becomes the status line
  `attribution: process tree (no cgroup delegation: ...)`.
- **Placing a job: a shell, not `unsafe`.** `Command::pre_exec` and `clone3(CLONE_INTO_CGROUP)`
  need `unsafe`; writing the child's pid from the parent leaves a window in which the child can
  fork children that stay outside. The child is therefore started as `/bin/sh -c 'echo $$ >
  cgroup.procs; exec "$@"'`: it joins first and then becomes the command (same pid, group
  leadership and start time, so leases and the PID-reuse guard are unchanged). A failed write is
  silent and the command still runs; the missing membership shows as figures of quality
  `process_tree`. A program that does not exist now ends as exit 127 from the shell instead of a
  spawn error, for jobs that are placed in a cgroup.
- **Figures.** `JobFigures` (`tasks-core`): read/write bytes and ops from `io.stat`, `memory.peak`
  (or `memory.current` before kernel 5.19), `cpu.stat` usage, and the PSI `total=` stall times of
  `io`, `memory` and `cpu` (cumulative microseconds, so they are integers and exact). The runner
  samples every 2 s, the daemon writes them to the ledger at most every 10 s and always at the
  end, read after the last process is gone and before the cgroup is removed. The quality
  (`cgroup` or `process_tree`) is part of the stored value. Without a cgroup the figures are the
  sum over the job's process tree from the process table (Linux `/proc/PID/io`; macOS
  `sysinfo`, whose per-process disk bytes come from `proc_pid_rusage`): a process that exits
  between two samples loses what it did after the last sample, and stall times are not available.
- **Kill order.** Cancel, timeout and cleanup still Term then Kill the process group first. Only if
  something remains in the job's cgroup (other than the unreaped leader) does `cgroup.kill` run,
  then the same grace. A leader that survives the group kill is also finished with `cgroup.kill`.
  Empty job cgroups are removed after the job; at start the daemon kills and removes the ones a
  previous daemon left.
- **Trips.** `pressure.trip` audit rows, at most one per resource (`io`, `cpu`, `memory`) per
  `attribution.trip_interval_secs` (60); the next row says how many it dropped. Sources: `gate`
  (admission refused a waiting job), `psi` (machine pressure at or over
  `attribution.trip_io_pct` 10 / `trip_cpu_pct` 60, or memory level warn), `transfer` (a sync phase
  waited). The row holds the machine pressure, every running job's figures, the transfer
  figures and the top 10 outside processes by disk bytes in the last window
  (`attribution.sample_secs`, 5) and by RSS. A process is recorded as pid and program base name
  only; the process table type has no field for arguments or environment.
  `loadtable stalls` is `loadtable audit --action pressure.trip` with the evidence laid out; the UI
  has a Pressure events tab. A process whose I/O the OS hides (another user's, on Linux) is
  counted as unreadable, not listed.
- **Transfers.** `SyncGate` is implemented by `TransferGate` on top of admission: new slot kind
  `transfer` (`slots.transfer`, default 1) with the memory floor, the state reserve and the I/O
  gate (`pressure.transfer.max_io`, default 40) but no CPU gate. It waits without a time limit
  (a sync during a stall should not add to it) and ends the wait at shutdown. `git` children
  join `tasks.slice/transfer`; the in-process work (gix snapshot, apply) cannot be moved into a
  cgroup, so the daemon's own `/proc/self/io` delta while a slot is held is counted as
  `inproc_read_bytes`/`inproc_write_bytes` (approximate: it includes anything else the daemon does
  at the same time).
- **Not done: a sync stream.** The pack still crosses as `SyncPackChunk` request/reply frames. A
  stream needs a new method on the `Connection` trait and implementations in the fake and the
  iroh adapter with their own tests, and nothing in attribution depends on it; it is the next
  step for transfer speed, not for accounting.

## 10. Scheduling: priority and the short lane

Queue order: `(class, effective wait, submit time)`.

| class | for | how a job gets it |
|---|---|---|
| `blocking` | an agent or person is waiting on the result now | `--priority blocking --reason TEXT`; audited |
| `normal` | default | — |
| `background` | warm-ups, long suites, sweeps | `--priority background` or the repo run policy |

- Aging: a waiting job's effective class rises one step after a configurable wait
  (default 20 min), so background work never starves.
- Fairness: within a class, round-robin by owner, so one agent's 30 queued jobs don't
  block another agent's one.
- Short lane (exists in admission today): admission is decided by the timing history:
  predicted duration under the limit (default 60 s) and predicted peak memory under a
  cap. Short jobs skip the CPU gate but keep the memory floor and disk reserve (built).
  A short job that overruns is not killed; its history updates, so the next run of that
  recipe is not short. No flag lets an agent claim "short".

### As built (step 5)

`tasks-core` (`priority.rs`), `tasks-admission` (`order.rs`), `tasks-timing` (`short.rs`),
`tasks-ledger` (schema v8), `tasks-placement`, `tasks-daemon` (`queue_estimate.rs`, `queue_view.rs`,
`place.rs`, `status.rs`), CLI, MCP and UI. Where the build differs from or fills in the text:

- **The order, exactly.** `tasks_admission::sequence` takes the waiters, the clock and when each
  owner last started a job, and returns the order `decide` walks (and status, list positions and
  the estimates use). Each waiter's class is its *effective* class: after every `queue.aging_mins`
  (20) of waiting it rises one class, background to normal only, never to blocking (that class is a
  person's claim with a reason, not a reward for waiting). It enters the higher class at its submit
  time plus one aging period per step, and "effective wait" is the time since it entered its
  class. Classes go strictly in the order blocking, normal, background. Inside a class the owners
  take turns: among owners that still have waiters, the one that started a job least recently
  (never started comes first) gets its oldest waiter (by when it entered the class, then submit
  time, then job id); that owner then goes to the back of the rotation; ties between owners go to
  the one whose next waiter is older, then the owner name. A held waiter (checkout busy, drain)
  keeps its place and its owner's turn, so the checkout-ownership rule of R-230 is unchanged.
  Last-start times live in daemon memory; after a restart every owner is "never started".
- **Storage.** `JobSpec.priority` and `priority_reason` (serde defaults, left out when normal)
  carry the class through the API, plans, peers and the ledger's spec column; `Store::set_priority`
  rewrites that column for a queued or waiting job and refuses any other with `NotQueued`.
  `StepHints.priority` is the run policy's per-step hint (`priority = "background"` in a step;
  `blocking` is refused in policy and in plans). Plans and suites take a class for every step
  (`--priority` on `steps` and `suite run`); a step with none keeps its hint.
- **Audit.** `job.priority`: owner, job, `priority`, `from` (changes only), `reason`, `via`
  (`submit` or `prioritize`). Recorded by the audited ledger wrapper, so every path is covered.
  Only the owner may `prioritize`, as for cancel.
- **Short lane.** The waiter's prediction comes from history only: duration is the worst of the
  latest three non-cancelled runs of the recipe on this machine, and needs at least two passed
  runs; peak memory is the worst known `figures.memory_peak` of the latest three runs, the
  latest run having measured one (a new `mem_peak` column on whole-job samples, schema v8, filled
  when the job finishes from its final figures). Either unknown means not short. The limits are
  `slots.short_job_max_secs` (60) and `slots.short_job_max_memory_mib` (1024). Using the worst of
  three rather than the median is what makes one overrun enough to end short-lane use.
- **Placement.** A machine whose admission gate for the job is closed never wins over one that
  could start the job at once (`Score::ready_now`); with no such machine, or with the peer named,
  the old arithmetic (extra wait for pressure) applies. A peer reports only its compile gate, so
  its pressure counts for compile-slot jobs. The local wait for a new job now follows the queue
  order and counts only the jobs ahead of it (before, the job counted itself, which charged an
  idle local machine the "unknown wait" and favoured peers).
- **Forwarded jobs the peer holds back.** The local row stays `Running` (R-015); `JobView.
  remote_wait` and a `WaiterView` with `on_peer` say it waits, with the peer's reason from its
  last reported phase. Status lists it under waiting, not running.
- **Not done.** The class is not sent to a peer's queue (a forwarded job is ordered by the
  peer's own rules); `prioritize` on a job already forwarded changes only the local record.

## 11. Workloads and test parallelism (decided)

1. Split `cargo test` by target across machines: build the feature (a plan runs `--lib`
   and each `--test X` as separate jobs, placed independently, results merged into one
   report), enabled per suite in the repo run policy (opt-in). Per-target timings decide
   whether a split pays off; the policy can suggest enabling it.
2. Placement: the agent states intent (`--pin`, `--peer`, `--anywhere`), the repo run
   policy gives per-step hints, machine policy only caps (slots, RAM, I/O, disk).
3. Cache warmth is a placement input: the expected finish time includes warmth from
   history for each machine, so a warm machine wins unless a cold idle one would still
   finish sooner.
4. No per-agent quotas for now; round-robin by owner within a class only.

## 12. Decisions

- D1: own copy of the identity code for now (`tasks-identity`, golden vectors shared).
- D2: iroh's public default relays; self-hosted relay later.
- D3: `gix` library, with a named `git`-command fallback only where gix has a real gap.
- D4: yes, daemon as a systemd user service with `Delegate=yes` on both Linux machines
  (the worker's unit edit in `nix/taskworker-host.nix` is the user's step).
- D5: yes, audit retention 90 days with the hash chain.
- D6: yes, grants 1 year (renewed while connected), tickets 15 minutes and single use.

## 13. Build order

1. Identity + grants + pairing + iroh connection with admission (peer crate `trust`
   module, daemon wiring, `loadtable pair`/`peers`/`identity` commands). Remove SSH paths.
2. Audit table and `loadtable audit` (ledger schema bump), wired to pairing and forwarding.
3. Git sync (`Snapshot`, `PackTransfer`, worker mirrors, retention), replacing
   manifest/sync/store.
4. Attribution: per-job cgroups, macOS sampler, gate-trip snapshots, transfer slot.
5. Priority classes, aging, owner round-robin; short lane from history.
6. Placement by expected finish time including cache warmth; opt-in `cargo test` split
   across machines with merged results.

Each step lands with regression tests named after the R-ids it covers; new behaviours get
new R-ids in `docs/REGRESSIONS.md`.

### As built (first real trial, after step 5)

Regressions R-352 to R-372 (`docs/REGRESSIONS.md` section 20).

- **Pack stream.** `Connection::open_upload(head)` / `Upload::write` / `Upload::finish` and
  `Handler::accept_upload` / `Handler::upload` (all with refusing defaults) carry the sync pack on
  one stream. Wire: `Body::SyncPackStream { checkout, session }` as the first frame; the receiver
  answers `Ok` (or `Refuse`) before any data, the sender then writes the pack as raw bytes and
  finishes, and the receiver answers `SyncPackAck { received }` after reading to the end. The
  receiving side applies `max_pack_bytes` (`SyncStore::put_stream`, reading one byte past the limit
  to refuse an overlong stream and removing the partial file), idle reads time out after 120 s, and
  `SyncApply` still checks length and SHA-256 against the spool. The origin hashes while writing.
  An abandoned upload resets the stream, so it can not look like a complete shorter pack. **Older
  peer:** a receiver without the variant answers `Refuse(Malformed)` at the head (R-372), the origin
  then sends `SyncPackChunk` frames exactly as before (that path is kept for this reason only;
  `Busy` is not a fallback). The origin does not cache the peer's ability: it asks once per sync.
- **Path RTT and reply time.** `rtt_ms` was always the transport's number (iroh `Path::rtt` of the
  selected path, read after the status request), a QUIC estimate that moves with load and path
  switches (hence 199 ms then 87 ms on one LAN link). It is now labelled *path RTT*. New
  `reply_ms` is the wall time of the status request itself (handler and scheduler included), shown
  as *reply time*. Both are in `PeerView`, `peers`, status and MCP status.
- **Swap and CPU.** `MemorySnapshot` carries swap size/use, cumulative swap-in/out bytes and CPU
  busy/total counters; `HostRates::between` turns two readings into percent and bytes per second
  (a counter that went back gives none). Linux reads `/proc/meminfo`, `/proc/vmstat`,
  `/proc/stat`. macOS reads `sysctl vm.swapusage`, `vm_stat` (Swapins/Swapouts) and, for CPU, a
  process-wide `sysinfo` reading integrated into counters (sysinfo needs two readings 200 ms apart;
  the first has none). The daemon keeps the base reading and replaces it every two seconds.
  `PressureView`, `PeerMachineView` and `PressureSummary` (what peers report) gained optional
  fields with serde defaults.
- **Placement line.** `Request::reason()` (default, `--pin`, `--peer X`, `--anywhere`); the
  estimate shows only with run history and a known queue, else "no estimate (...)". A job received
  from a peer logs "sent here by a peer" instead of a placement choice it did not make.
- **Submit warnings, tail offset, class.** `Response::Submitted.warnings`,
  `Response::LogTail.end_offset` (both skipped when empty/unknown), and
  `Forwarder::set_class` before `start` (the `prioritize` of a job that waits for its offload slot
  now reaches the peer's queue; the class itself already travelled inside `JobSpec`).
- **gix 0.90** (was 0.73, features now `sha1 index dirwalk excludes attributes`; no C toolchain, no
  network client). The upgrade cost one changed call. In process now (`sync/gix_objects.rs`):
  `list_refs`, `update_refs` (one `edit_references` transaction), `tree_files`, `tree_contains`,
  `diff_tree` (equal subtrees skipped by id). Still on the `git` CLI: `pack-objects --thin`
  (gix writes packs but has no thin-pack generation against excluded bases), `index-pack
  --fix-thin` (gix-pack can do it only behind `streaming-input`, which gix enables only with its
  network clients; a direct `gix-pack` dependency plus progress types for no change in
  behaviour), `read-tree --reset -u` (the two-way update that keeps untracked outputs) and `gc`
  (no repack/prune in gix). Git children still join the transfer cgroup; the in-process work counts
  in the daemon's own read/write figures.
