# Idle budget: daemon CPU and window memory (fix/idle-cost)

What an idle node may cost, why it cost more, and how to measure it. Proposed for the
node policy ([node-setup-policy.md](node-setup-policy.md)); health can check the numbers
later from `/proc` (the daemon's and the window's own `stat`/`status`), without any
change to the protocol.

## What was wrong (laptop kurukkan, release 1a9830e, 2026-10-18)

Daemon (pid 832539, 6 h old): 937 CPU s, 725 of them system time; its long-lived threads
(health-scan, health, lease-audit, reconcile, sampler) only ~30 s. The rest was request
threads. Measured in isolation (`scripts/idle_bench.py`, 730 synthetic finished jobs,
release build):

| request | laptop before | worker before | worker after | reply before | reply after |
|---|---|---|---|---|---|
| `list` | 166 ms (139 ms sys) | 304 ms (201 sys) | 0.33 ms | 0.9 MB | 0.9 MB (unchanged) |
| `logs ID` | 166 ms | 304 ms | 0.33 ms | 0.93 MB | 34 KB (its job + 32 KiB tail) |
| `status` | 11 ms | 17.3 ms | 2.7 ms | 37 KB | 37 KB |
| `job ID` | 2.3 ms | 2.7 ms | 0.33 ms | 1 KB | 1 KB |
| `list_active` (new) | - | - | < 0.1 ms | - | 89 B (nothing unfinished) |
| idle, no client | 0.03 s/min | 0.10 s/min | 0.09 s/min | | |
| `status` every 2 s (one window) | - | 0.78 s/min | 0.25 s/min | | |
| + `list` every 2.5 s (release window) | 4.13 s/min (6.9 % core) | - | - | | |

Causes, in order of cost:

1. **Unbuffered reply writes.** `serde_json::to_writer` straight onto the `UnixStream`
   issues one `write(2)` per JSON token: ~0.14 s of system time per 1 MB listing. Fixed with
   a 64 KiB `BufWriter` (all replies).
2. **Every `list`/`logs`/`start-worker`/`stop-worker`/`drain` re-read, re-parsed and
   re-serialised the whole history plus all runtime estimates** (814 rows, 334 KB). Fixed by
   `listing::Listing`: jobs, estimates and their JSON are kept per ledger generation
   (`PRAGMA data_version` for other connections' commits + `total_changes()` for the
   daemon's own), so `list` is a memcpy until the ledger changes. `logs` now carries only
   its job; the worker switches only the unfinished jobs (their clients only count
   approved/active jobs).
3. **The per-request reconcile check** (when no worker holds `worker.lock`, i.e. most of
   the time on an idle laptop) parsed all jobs on every request just to find active ones.
   It now asks the cached listing first.
4. **Polling clients.** On the live laptop the steady load was three stale agent shell loops
   (`until tasks logs N 2>/dev/null | grep -q "^job N passed"; do sleep 15..30; done`, about
   20 h old) that can never end because the phase line goes to stderr: ~9 full-listing
   `logs` requests per minute, ~1.5 of the measured 2.1 CPU s/min. The release window adds a
   `list` every 2.5 s and a `tasks status --json` subprocess every 2 s while it is drawn.

Window (pid 832717, `tasks ui`, 6 h old): VmRSS 2.8 GiB, **VmHWM 5.6 GiB**; 47 glibc malloc
arenas of 64 MiB each, all resident (2.6 GB of the 2.8). Its main thread got no CPU time
at all for minutes at a time (the window was hidden; a Wayland window that is not shown gets
no frame callbacks, so egui's `update` does not run), while the two feed threads (local
`status`, remote `status-watch-ssh` every 2 s) kept sending parsed snapshots into unbounded
`mpsc` channels that only `update` drained. Each parsed snapshot holds every job twice
(typed plus its raw `serde_json::Value`): ~557 KiB resident for a 96 KB, 51-job document
(`ui::tests::parsed_snapshot_memory_cost`, ignored measurement), so two undrained feeds grow
~1.9 GiB/h. When the window was shown again the
backlog was drained and freed at once (the 5.6 GiB peak); glibc kept the fragmented arenas
resident (2.8 GiB). While hidden RSS hardly moves (+2 MiB/h measured) because the backlog
first reuses that freed arena memory, so RSS alone understates the growth: flat (+2 MiB/h)
for the first 35 min of sampling, then a steady +12.6 MiB/min (~0.75 GiB/h) from 17:50 to
18:10 with the main thread still idle. That matches one undrained 86 KB worker feed
(~557 KiB x 1800/h = ~0.98 GiB/h predicted).

Fixed: a latest-only `Mailbox` per feed (newest snapshot + a later error; same end state as
replaying everything), a demand-driven local feed (no fetch while the last snapshot was not
taken: a hidden window costs the daemon nothing), `status` requested in-process instead of a
subprocess, `list_active` instead of `list` for the window's approvals poll, and a stable
egui id for the raw-JSON header of an expanded row (its id included the JSON length, leaving a
new collapsing/scroll state in egui's memory every refresh).

## Proposed budgets (release build, a few hundred to ~1000 jobs of history)

| Budget | Value | Measured after (worker) | Check |
|---|---|---|---|
| Daemon CPU, idle, no clients | <= 0.2 CPU s/min (0.3 % core) | 0.09 | `/proc/PID/stat` utime+stime delta over 10 min |
| Daemon CPU with one window open | <= 0.5 CPU s/min | 0.25 | same, window drawn |
| `status` | <= 10 ms CPU | 2.7 ms | `scripts/idle_bench.py --budget-status-ms 10` |
| `list_active`, `job` | <= 2 ms CPU, reply <= 64 KiB | < 0.4 ms, 89 B / 1 KB | `--budget-list-active-ms 2 --budget-job-ms 2` |
| `list` (cache hit) | <= 20 ms CPU | 0.33 ms | bench |
| Window RSS | <= 400 MiB after 24 h, growth <= 20 MiB/h after the first hour, drawn or hidden | not measured live (see limitations) | `/proc/PID/status` VmRSS and VmHWM, hourly |

A cache miss (the first `list` after any ledger commit) still parses the whole history once:
tens of ms at ~730 jobs. That is per change, not per request.

## How to measure

```sh
# Release daemon, isolated (private state, locks, HOME, no peer config), synthetic history:
python3 -I scripts/idle_bench.py --bin target/release/tasks --jobs 730 --json out.json \
  [--ui-list] [--budget-idle-cpu-s-per-min 0.2 --budget-status-ms 10 --budget-list-active-ms 2]
# Regression tests (debug-tolerant thresholds): reply sizes, cache invalidation, idle CPU
cargo test --locked --test idle_budget
# Window: bounded hand-over and flat egui memory over 300 refreshes with open rows
cargo test --locked --bin tasks -- mailbox_tests widget_memory_stays_flat
```

## Not done here

- Push instead of poll for the local window (a `status` watch on the daemon socket; the
  remote side already streams via `lan-peer status-watch`). With the cached listing and the
  demand-driven feed, a drawn window costs ~0.25 CPU s/min, so this is no longer urgent.
- `tasks logs` still prints the `job N passed` line on stderr only; waiting loops that grep
  stdout never end (feedback/status-stream-324-348.md asked for it too). Prefer
  `tasks run` (it waits) or `tasks list --who OWNER --json`.
- `MAX_REPLY` is 4 MiB: at ~1.5-2 KB per job a plain `list` fails at roughly 2000-2500 jobs
  of history. Retention or a paged `list` is needed before that.
