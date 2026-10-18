# Node setup policy

Status: draft (2026-10-17). Owner: the task-manager lead session. Applies to every machine
that runs a `tasks` daemon (the laptop controller and every worker peer).

## Goal

Any node, set up from scratch, rebuilt, or just rebooted, reaches a correct, verified state
without anyone remembering steps, and stays there. When it is not correct, `tasks` says
exactly what is wrong and how to fix it, and keeps work away from that node rather than
letting jobs hang.

## The four rules

1. **Declared, not hand-made.** Everything a node needs comes from one declaration: the
   NixOS module (`nix/module.nix`) for workers, and the matching install script for
   non-Nix hosts. That covers packages and tools, daemon environment, compiler-cache
   server, SDKs, cache paths and limits, firewall ports and users. A step done by hand over SSH
   is a bug in the declaration. Setup scripts live in the repo, never in `/tmp` (a tmpfs is
   wiped on reboot).
2. **Verified by `tasks` itself.** The daemon checks the declaration's promises at start and
   periodically (health monitor). Each check has a stable alert id, evidence and a fix
   line. `tasks doctor` runs them on demand.
3. **Refuse, fix, escape.** A job that needs a capability the node lacks is refused or
   routed elsewhere at submit, with the fix named. It is never admitted to hang. Every
   refusal has an explicit escape flag (compute-discipline policy).
4. **Survives restarts.** A reboot of any node must leave the queue running, keep every
   finished result, and make interrupted work visible and easy to retry.

## Required checks (from real incidents)

| Check | Incident it prevents | Where it lives |
|---|---|---|
| Execution runs after daemon start (no manual `tasks start-worker`) | 2026-10-17: laptop rebooted, queue sat `approved` until a manual start | daemon |
| Every slot-lock holder is a live job; otherwise name pid/cmd/origin job and don't block | 2026-10-17: sccache server spawned inside job #154/#155 inherited the `p2p-build.lock` lease and blocked the worker slot for its 10 min idle timeout, repeatedly | fix/lease-leak + health |
| Compiler-cache server runs outside jobs (declared user service, no idle exit) | same | module + health |
| Declared capabilities present: libclang/bindgen env, JDK, Android SDK, cargo-ndk, required tools (incl. htop) | job 456 "Unable to find libclang"; htop only via `nix-shell -p` | module + health capability check |
| Cache and state disk free above thresholds; per-checkout `target/` eviction by age | laptop build cache at 91%: 30 checkouts × 7–12 GB despite sccache | health disk + eviction policy |
| Shared prefixes (libvpx/libaom installs) only built by owners; per-branch flags get per-branch prefixes | worker job 40 rebuilt shared libvpx as decoder-only | policy + script guard |
| Offloaded results survive a controller outage | worker #152 passed while laptop was down; laptop job #507 became `interrupted` | offload/peer |
| Interrupted jobs never read as success; offloaded logs keep head + tail (not only the last ~460 lines) | log-search 498/522/536: interrupted job showed `EXIT 0`; clippy result and panic lost from the tail-only log | worker/offload |
| Daemon's own I/O after restart is bounded | laptop `tasks worker` wrote ~4.9 GB at start on btrfs and tripped the I/O PSI gate | daemon/ledger |
| Monitoring is bounded (no unbounded `du` over caches) | ad-hoc `du` over 142 GB added I/O pressure on both nodes | health disk scan |
| Heavy work defaults to the least-loaded capable node | 2026-10-17: four agents queued on an I/O-bound laptop while the worker sat idle | placement/reroute; agent briefs until deployed |

## Adding a node

1. Pick the host template (`nix/template/worker.nix` or the installer ISO) and set only
   host facts (name, disk, SSH key, cache size).
2. `nixos-rebuild switch` (or install). Nothing else by hand.
3. `tasks doctor` on the node must be all green; the controller then shows it as a
   capable peer. Any red line names its fix.

## Ownership

- Declaration: feature/worker-package (95e2ff2: sccache server service, operatorTools,
  bindgen env, fresh-install template/ISO, `tasks peer add`; evaluated only, not built
  or deployed; worker 192.168.1.7 not yet migrated to the module).
- Checks, alerts and `tasks doctor`: feature/health-alerts (3b3021d: slot-holder,
  compiler-cache, declared_capability via TASKS_DECLARED_CAPABILITIES, bounded disk scan,
  `tasks doctor`; disk-reserve unit bug being fixed).
- Lease leak: fix/lease-leak (a5efac8: worker pre-starts sccache outside the job; real flock
  holder named via /proc fdinfo with pid/cmd/origin job; not deployed).
- Restart survival (execution auto-resume, interrupted never reads as success, offload
  result reconciliation, head+tail logs, restart I/O burst): fix/restart-survival.
- Target eviction by age: not yet assigned.
