# One queue across machines

Status: design (2026-10-18). Part of the [node setup policy](node-setup-policy.md). Builds on
the daemon-to-daemon link in [peer-link.md](peer-link.md).

## Goal

You or an agent submit on either machine with `tasks run -- CMD` (or MCP `submit`). The
manager decides where it runs. When a slot frees on any machine, the next job that machine
may run goes there. Nobody types `--offload`, `tasks-offload`, `--worker` or `--ssh-key`.

## Where it stands (2026-10-18)

- **Transport today:** every cross-machine operation is SSH, driven by Python:
  `tasks-offload` runs `ssh worker tasks run --follow`, rsync moves source and artifacts,
  `status-watch-ssh` polls `tasks status --json` every 2 s for `tasks ui`, and result
  recovery and cancel also go over SSH.
- **The iroh link** (`crates/peer`, `src/peer/`) is built and ships in the installed
  release 1a9830e on both machines, with `status`, `jobs`, `source` and `artifact` ALPNs,
  `tasks run --peer`, placement (`--anywhere`), reroute and MCP `submit` on top. It is
  **off**: neither machine has a peer config, and it has only run over loopback.
- **Relay and discovery are off** in code (`RelayMode::Disabled`; the runtime refuses to start
  if any relay or lookup service is present). Keep it that way: LAN only.
- **Firewall:** the worker already allows UDP 7842 on `wlo1` (opened for the earlier
  `lan-peer` experiment, nothing listens there now). A worker daemon bound to
  `192.168.1.7:7842` needs no NixOS change. The laptop only dials out.

## Pairing

All through the binary, no scripts:

1. On each machine: `tasks peer init --bind <its LAN IP>:7842` (new key, readable only by the
   user), then `tasks peer id`.
2. Each side adds the other with `tasks peer add` (id, address, role: the worker is `worker`
   to the laptop, the laptop is `host` to the worker).
3. Restart each daemon when idle (`tasks drain`, then the restart).
4. `tasks peer list` on both; one job each way to compare with the SSH path.

Later the peer entries move into the machine policy file (`peers::`), written by the NixOS
module ([policy-file.md](policy-file.md)).

## Queue model: free slots pull

Built today: placement picks a machine at submit (sooner expected finish), and reroute moves
a waiting job every 20 s only if another machine finishes it at least 2× and 5 min sooner.
That is push plus correction. The target is pull:

- **One cluster queue.** A job joins one queue whichever machine it was submitted on. The
  daemon it was submitted to owns it, so it survives restarts there.
- **Free slots pull.** Each daemon publishes its free slots over the `status` link. When a
  slot frees on any machine, that machine takes the oldest job it may run: it has the
  capabilities and devices the job needs, the job is not pinned elsewhere, and its source is
  not too costly to move.
- **Claims, not copies.** Taking a job is a claim on its job id (reroute already moves jobs by
  id), so a job never runs twice. If the machine running it is lost, the job returns to the
  queue as interrupted, never as success.
- **Rules from the policy file.** Prefer the worker for builds (the laptop is short on disk);
  laptop-only jobs (USB phone installs, the GUI build) stay on the laptop; per-agent fairness
  so one agent cannot fill every slot; `--pin` keeps a job where it is.

## Transfers

Today's peer transfer path would be slow and memory-hungry on large trees, and doubles disk
use on the receiver:

- every file is read whole into memory (64 MiB per file, 512 MiB per sync), one at a time,
  with no streaming or resume;
- received data is first imported into the iroh blob store, then written to the checkout or
  destination: two copies on the receiving machine;
- neither the blob store nor synced checkouts have a quota or cleanup.

Target:

- stream file to file with iroh-blobs' verified streaming and range requests (resume after a
  dropped link, flat memory), several transfers in parallel, only missing pieces sent;
- no second copy on a machine with `keep_copies: false`: source is served straight from the
  working tree, artifacts stream straight to their destination and only when a step needs
  them (for example an APK for a USB install);
- build outputs stay on the machine that built them; with builds defaulting to the worker,
  most of the laptop's per-checkout `target/` folders are no longer needed;
- quotas and age-based cleanup for stores and checkouts, from the machine file's `storage::`;
- a `tasks` transfer benchmark, so link speed is measured, not guessed. The worker is on
  Wi-Fi (`wlo1`); a wired link may matter more than any code change.

## Labels

Show checkout state and cache state separately, e.g. `first build in this checkout · cache 99%
hits` or `rebuilding video (edited) · deps cached`, instead of `cold`.

## Before the Python/SSH path can go

From [peer-link.md](peer-link.md), plus the above: pairing and a LAN run; in-tree symlinks;
`--cargo-fetch`; `--fetch-dest`; the one-line JSON summary scripts parse;
`offload-portable-desktop.sh`; streaming transfers; quotas; `tasks ui` on
`tasks peer status-watch` instead of `status-watch-ssh`; account binding and key revocation
beyond allowlist files.

## Order of work

1. Pair the machines; LAN run of the existing link; compare one job each way with SSH.
2. Submit without SSH: `tasks run --peer` and `--anywhere` as the default path for agents
   (MCP `submit` registered for agent sessions).
3. Streaming transfers with no second copy, quotas, benchmark.
4. Replace placement-at-submit plus reroute with the pull queue.
5. Labels.
6. Close the parity gaps and delete `scripts/tasks_offload.py` and the other stopgaps.
