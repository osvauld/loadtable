# Node setup policy

Status: draft (2026-10-17, revised 2026-10-18). Owner: the task-manager lead session. Applies
to every machine that runs a `tasks` daemon (the laptop controller and every worker peer).
Companion designs: [policy-file.md](policy-file.md) (the HUML file that holds tools,
capabilities, recipes and workflows) and [cluster-queue.md](cluster-queue.md) (one queue across
machines over iroh).

## Goal

Any node, set up from scratch, rebuilt, or just rebooted, reaches a correct, verified state
without anyone remembering steps, and stays there. When it is not correct, `tasks` says
exactly what is wrong and how to fix it, and keeps work away from that node rather than
letting jobs hang.

## The rules

1. **Declared, not hand-made.** Everything a node provides and needs comes from one
   declaration. The machine's part is the machine policy file
   (`~/.config/tasks/policy.huml`), which the NixOS module (`nix/module.nix`) writes for
   workers and a one-time bootstrap writes for non-Nix hosts. The project's part is the
   project policy file (`.tasks/policy.huml`, kept in the repo). Together they cover tools,
   capabilities, cache paths and limits, thresholds, peers, recipes and workflows (see
   [policy-file.md](policy-file.md)). A step done by hand over SSH is a bug in the
   declaration.
2. **Everything goes through the `tasks` binary.** No Python and no shell scripts in the
   path of setup, submission, transfer, deploy or monitoring. The only exception is the first
   install of the binary on a machine (the NixOS module, or one bootstrap command on other
   hosts). Today's scripts (`scripts/tasks_offload.py`, `offload_agent.py`,
   `worker_source.py`, `status_watch_ssh.py`, `linux_worker_setup.py`,
   `setup-linux-worker.sh`, `deploy/install-*.sh`) are stopgaps: they get no new features and
   are deleted once the binary covers them.
3. **Verified by `tasks` itself.** The daemon checks the declaration's promises at start and
   periodically (health monitor). Each check has a stable alert id, evidence and a fix
   line. `tasks doctor` runs them on demand. The checks themselves come from the policy file
   (each capability's probe), so they are the same on every machine and the binary carries no
   tool-specific knowledge.
4. **Refuse, fix, escape.** A job that needs a capability no eligible node has is refused at
   submit, with the fix named. It is never admitted to hang. Every refusal has an explicit
   escape flag (compute-discipline policy).
5. **Survives restarts.** A reboot of any node must leave the queue running, keep every
   finished result, and make interrupted work visible and easy to retry. Settings live in the
   policy file, which the daemon reads itself, not in a unit's environment, so a daemon is
   correct however it was started.
6. **One daemon per machine, from the installed release.** A client never starts a daemon of
   its own on a host where a systemd unit runs `tasks`, and never starts one from a binary
   other than the installed release. Health flags a daemon whose binary is not the installed
   one, or that runs outside its unit.
7. **LAN only.** Machines talk daemon to daemon over iroh on the LAN: relay and address
   discovery are always off, each endpoint binds one explicit LAN address and dials its
   peers' configured addresses. SSH remains only as an operator's admin login.
8. **A machine short on space keeps no copies.** Build outputs live where the build ran
   (by default the worker). Transfers stream straight to their destination with no second
   copy in a blob store. Every store and synced checkout has a quota and age-based cleanup
   declared in the machine policy file and checked by health.

## Required checks (from real incidents)

| Check | Incident it prevents | Where it lives |
|---|---|---|
| Execution runs after daemon start (no manual `tasks start-worker`) | 2026-10-17: laptop rebooted, queue sat `approved` until a manual start | daemon (restart-survival, deployed 1a9830e) |
| Every slot-lock holder is a live job; otherwise name pid/cmd/origin job and don't block | 2026-10-17: sccache server spawned inside job #154/#155 inherited the `p2p-build.lock` lease and blocked the worker slot for its 10 min idle timeout, repeatedly | lease-leak + health + lease audit (deployed) |
| Compiler-cache server runs outside jobs (declared user service, no idle exit) | same | module + health |
| Declared capabilities present, checked by probes from the policy file | job 456 "Unable to find libclang"; htop only via `nix-shell -p`; the laptop's libclang probe accepted `libclang-cpp.so` (the wrong library) | policy file + health capability check |
| A failed offloaded job marks the machine that ran it, not the one that submitted it | 2026-10-18: worker job 456's libclang failure was marked against kurukkan, so placement avoided the laptop (which has libclang) for 24 h | health capability check |
| A memory or disk gate above what the machine physically has is a configuration error, not "pressure" | 2026-10-18: a daemon without the unit's `TASKS_MIN_AVAILABLE_MEMORY_MIB=1024` used the 8 GiB default on a 7.6 GiB worker and blocked every job, while the alert said "never lower the gates" | health + policy file |
| A daemon runs from the installed release and, where a unit exists, under that unit | 2026-10-18 install: the install script's own probe started a stray worker daemon without the unit's environment (no memory gate, no LIBCLANG); on the laptop an old `tasks ui` restarted the old daemon right after the switch | daemon start + health + install path |
| A job that is alive but makes no progress is flagged and does not hold a slot indefinitely | 2026-10-18: worker #184's Gradle hit `OutOfMemoryError` and never exited; it held the compile slot 61 min until cancelled | health watchdog (unassigned) |
| Admission considers memory and cores, not only one compile slot | worker #184 used ~3 of 12 cores while others queued behind it for up to an hour | admission (unassigned) |
| Restarting a daemon cannot race admission | design gap found while investigating #184 | `tasks drain` (deployed) |
| Cache and state disk free above thresholds; per-checkout `target/` eviction by age | laptop build cache at 91%: 30 checkouts × 7–12 GB despite sccache; worker `~/offload` 81 G | health disk + eviction policy (unassigned) |
| Shared prefixes (libvpx/libaom installs) only built by owners; per-branch flags get per-branch prefixes | worker job 40 rebuilt shared libvpx as decoder-only | policy file + guard (unassigned) |
| Offloaded results survive a controller outage | worker #152 passed while laptop was down; laptop job #507 became `interrupted` | offload/peer (deployed) |
| Interrupted jobs never read as success; offloaded logs keep head + tail | log-search 498/522/536: interrupted job showed `EXIT 0`; clippy result and panic lost from the tail-only log | worker/offload (deployed) |
| Daemon's own I/O at start is measured separately from its jobs' | laptop `tasks worker` showed ~4.9 GB at start: it was reaped children's I/O, not the daemon's | worker (deployed) |
| Monitoring is bounded (no unbounded `du` over caches) | ad-hoc `du` over 142 GB added I/O pressure on both nodes | health disk scan |
| Heavy work goes to a capable node with a free slot without the submitter choosing | 2026-10-17/18: agents queued on an I/O-bound, space-short laptop while the worker sat idle (e.g. call-stats-panel #699) | cluster queue ([cluster-queue.md](cluster-queue.md)) |
| Job labels say what they mean | "cold" shown for a fresh checkout while sccache hit 99.7% | display |

## Adding a node

1. Pick the host template (`nix/template/worker.nix` or the installer ISO) and set only
   host facts (name, LAN address, disk, cache size).
2. `nixos-rebuild switch` (or install). This writes the machine policy file, including the
   peer identity and allowlist. Nothing else by hand.
3. `tasks doctor` on the node must be all green; the other machines then show it as a
   capable peer. Any red line names its fix.

## Ownership

- Declaration: feature/worker-package (merged; worker 192.168.1.7 not yet migrated to the
  module).
- Checks, alerts and `tasks doctor`: feature/health-alerts (merged, deployed).
- Lease leak and lease audit: fix/lease-leak, fix/restart-survival (merged, deployed).
- Restart survival and `tasks drain`: fix/restart-survival (merged, deployed 1a9830e).
- Policy file schema and moving tool/capability knowledge out of Rust: unassigned
  ([policy-file.md](policy-file.md)).
- Pairing, streaming transfers, cluster queue, retiring the Python/SSH path: unassigned
  ([cluster-queue.md](cluster-queue.md)).
- Stuck-job watchdog, memory/core admission, target eviction, shared-prefix guard:
  unassigned.
