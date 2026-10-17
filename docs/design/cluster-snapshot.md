# Cluster snapshot (v1): the contract between metrics, status stream and UI

One JSON document describes one machine's live state. `tasks status --json`
prints it for the local machine; the worker publishes the same document over
iroh (`task-manager/status/0`, read-only); the egui UI merges the local snapshot
and each remote one into its view. Three branches build against this file in
parallel, so change it only by agreement (see "Ownership").

All times are Unix seconds (f64 where sub-second matters); bytes are u64; CPU is
in "cores" (1.0 = one logical CPU fully busy). Unknown values are `null`, never
guessed. Unknown fields must be ignored by readers (forward compatible).

```jsonc
{
  "version": 1,
  "taken_at": 1792216000.25,
  "machine": {
    "id": "kurukkan",                 // hostname; matches the `@host` owner suffix
    "role": "host" | "worker",
    "logical_cpus": 8,
    "cpu_busy_cores": 5.4,            // whole machine, over the last sample interval
    "mem_total_bytes": 0, "mem_available_bytes": 0,
    "swap_total_bytes": 0, "swap_used_bytes": 0,
    "pressure": { "cpu_some_avg10": 0.0, "memory_some_avg10": 0.0,
                  "memory_full_avg10": 0.0, "io_some_avg10": 0.0 },
    "blocker": null | "memory PSI full avg10 above 0.5%",   // admission's current pressure blocker
    "slots": [ { "kind": "compile"|"run"|"offload"|"device", "name": "p2p-build",
                 "busy": true, "holder_job": 91 | null, "holder_who": "view-step-up@kurukkan" | null } ],
    "history": { "interval_s": 2.0,   // ring buffer, newest last, <= 150 points (5 min)
                 "cpu_busy_cores": [ ], "mem_used_bytes": [ ] }
  },
  "jobs": [ {
    "id": 91,
    "owner": "view-step-up@kurukkan",
    "phase": "waiting_build" | "building" | "running" | "passed" | "failed" | "cancelled" | ...,
    "resource": "compile" | "run" | "offload" | "device",
    "device": "9BWAY1LSDC" | null,
    "checkout": "/home/abe/p2p_chat/.claude/worktrees/view-step-up",  // laptop path, also for worker jobs (from the .identity file)
    "checkout_name": "view-step-up",
    "command": "cargo test --release -p video",   // short display form: paths, -j, --locked, env NAME=... removed
    "kind_key": "cargo test --release -p video",  // grouping key for history/ETA (same normalisation)
    "argv": [ ],                                  // full argv, for the details view
    "submitted_at": 0, "started_at": null, "finished_at": null,
    "queue": { "position": 1, "waiting_for": "compile slot held by #91" | "memory pressure" | null },
    "live": {                                     // null when not running
      "cpu_cores": 7.8, "rss_bytes": 0, "processes": 14,
      "history": { "interval_s": 2.0, "cpu_cores": [ ], "rss_bytes": [ ] }
    },
    "progress": { "crates_compiled": 212, "crates_expected": 310 | null },  // cargo only; null otherwise
    "warmth": "cold" | "warm" | "unknown",
    "warmth_reason": "no build output for this checkout on this machine",
    "estimate": {                                 // null if no history: the UI shows "unknown"
      "total_s": 420.0, "remaining_s": 180.0, "samples": 6,
      "basis": "median of 6 warm runs of this kind on this machine"
    },
    "result": {                                   // finished jobs only
      "exit_code": 0, "wall_s": 0.0, "cpu_user_s": 0.0, "cpu_sys_s": 0.0,
      "peak_rss_bytes": 0, "crates_compiled": 0, "warmth": "warm"
    },
    "offload": {                                  // laptop-side record of an offload, else null
      "worker": "abe@192.168.1.7", "remote_job_id": 93, "transport": "ssh"|"iroh",
      "sync_s": 1.0, "fetch_s": null
    }
  } ]
}
```

Rules:
- `jobs` holds every live job plus the newest 50 finished ones.
- A laptop offload job and its worker job are linked by `offload.remote_job_id`
  (laptop side) and owner `WHO@kurukkan` + `checkout` (worker side). The UI shows
  the pair once, as the worker job, with the laptop job's sync/fetch times.
- Live sampling: every 2 s, sum over the job's process group (and its
  descendants that left the group, by session/parent walk) from `/proc`:
  CPU from utime+stime deltas, memory as summed RSS (documented overcount for
  shared pages). Daemon-side, in memory only; nothing new is written per sample.
- Persisted per finished job (in the existing execution JSON, backward
  compatible, no schema change): `result.*` and `warmth`, `crates_compiled`.

Ownership:
- `feature/metrics`: everything the local daemon computes (machine, slots,
  jobs, live, progress, warmth, estimate, result) and `tasks status --json`.
- `feature/status-stream`: `lan-peer` ALPN `task-manager/status/0` (worker
  serves its `tasks status --json`, read-only, ~2 s push) and
  `lan-peer status-watch` (laptop: JSON lines on stdout), ssh fallback
  (`ssh worker ~/.local/bin/tasks status --json` polling), and the laptop-side
  offload linkage fields.
- `feature/ui-v2`: the egui UI, consuming snapshots only (local via the daemon
  socket/`tasks status --json`, remote via `lan-peer status-watch` subprocess
  or the ssh fallback), developed against fixture files in `tests/fixtures/snapshot/`.
