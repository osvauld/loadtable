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
    "online": true,                   // optional; absent = true. false only in watcher offline markers
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
  } ],
  "stream": {                                     // added by the watchers only (see below);
    "transport": "iroh" | "ssh", "peer": "...",   // never printed by `tasks status --json`
    "seq": 12, "received_at": 1792216002.1
  }
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

## Remote snapshots: `status-watch` lines and markers (feature/status-stream)

The UI reads a remote machine from a subprocess that prints **one compact JSON
object per line** on stdout, forever: `lan-peer status-watch ...` (iroh) or
`scripts/status-watch-ssh ...` (ssh polling). Both print the same shapes:

- **Live snapshot**: the worker's `tasks status --json` document unchanged, plus
  `machine.online = true` and a top-level `stream` object
  `{transport, peer, seq, received_at}` (`seq` increases per received snapshot).
- **Marker** (synthetic, no data from the worker): `stream.marker` is set.

  ```jsonc
  {"version": 1, "taken_at": 1792216010.0,
   "machine": {"id": "nixos-worker", "online": false},
   "jobs": [],
   "stream": {"transport": "iroh", "peer": "<endpoint id or ssh destination>",
              "marker": "offline" | "status_error",
              "error": "stalled: no snapshot for 13s", "retry_in_s": 4.0 | null}}
  ```

  - `offline` (`machine.online = false`): the connection failed, was refused
    (e.g. `rejected: NotAllowlisted`), stalled (no frame for 3 x interval + 10 s)
    or ssh failed (exit 255 / timeout). The watcher retries after `retry_in_s`
    (backoff 1 s doubling to 30 s for iroh; interval doubling to 30 s for ssh;
    reset by the next snapshot) and keeps printing a marker per failed attempt.
  - `status_error` (`machine.online = true`): the worker is reachable but its
    status command failed, timed out, printed non-JSON or exceeded the size
    bound (4 MiB). Expected while the worker's installed `tasks` predates
    `tasks status --json`.
  - `machine.id` is the last id seen in a live snapshot, else `--machine-id`,
    else the dialed address/ssh host. Pass `--machine-id` (the worker's
    hostname) so the id is stable before the first snapshot.
  - `jobs: []` in a marker means **unknown**, not "no jobs": keep the last live
    view of that machine (greyed, "offline since ...") instead of clearing it.
- Readers must treat a missing `machine.online` as `true` (the local
  `tasks status --json` need not print it) and ignore `stream` if they don't
  need it.

## Offload linkage: the laptop-side job sidecar (feature/status-stream)

`tasks-offload` cannot change the daemon, so it publishes what it knows in a
sidecar file per **local** job, keyed by the local `tasks` job id:

`${XDG_STATE_HOME:-$HOME/.local/state}/tasks-offload/jobs/<local job id>.json`
(directory mode 0700; evaluated in the job's environment. The tasks worker
clears the environment and passes only `HOME` among these, so under
`tasks run` this is `$HOME/.local/state/tasks-offload/jobs/<id>.json`.)

```jsonc
{"version": 1, "local_job_id": 315, "pid": 1263688,
 "stage": "syncing" | "submitting" | "running" | "fetching" | "done",
 "started_at": 1792216000.0, "updated_at": 1792216030.5,
 "worker": "abe@192.168.1.7", "transport": "ssh" | "iroh", "device": null,
 "remote_job_id": 93 | null, "remote_phase": "passed" | null,
 "remote_state": "terminal" | "uncertain" | "not_submitted" | null, "exit": 0 | null,
 "sync_s": 1.0 | null, "run_s": null, "fetch_s": null,
 "durations": {"scan_s": 0.1, "sync_s": 1.0}}      // tasks-offload's summary durations so far
```

- Written (atomically: temp file in the same directory + rename) after the
  checkout lock is taken (`syncing`), before ssh `tasks run` (`submitting`),
  as soon as the remote job id is parsed (`running`, `remote_job_id` set),
  before `--fetch` (`fetching`) and at exit (`done`, with `exit` and all
  durations). Not written for `--dry-run`. A write failure is logged and never
  fails the offload. Files are small (< 1 KiB) and are not pruned.
- **Requires `TASKS_JOB_ID`** in the job's environment (a positive decimal
  integer; anything else is ignored with a warning). The installed `tasks`
  does not set it today. **feature/metrics must add it**: in `src/worker.rs`,
  after `sanitized(...)` builds the command for a job, set
  `TASKS_JOB_ID=<job id>` for every live command (it is not a credential and
  not in `PASSTHROUGH_ENV`, so it is set explicitly per job, not inherited).
  Without it `tasks-offload` writes no sidecar (summary `sidecar: null`).
- **Reader (daemon or UI) for a job with `resource == "offload"`**: read
  `$HOME/.local/state/tasks-offload/jobs/<id>.json` (and, if the reader's own
  `XDG_STATE_HOME` differs, that location too). Ignore a missing, unreadable,
  non-JSON or `version != 1` file, and a file whose `local_job_id` differs
  from the job id. Fill `jobs[].offload` from it:
  `{worker, remote_job_id, transport, sync_s, fetch_s}` (copy the keys;
  additional keys such as `stage`/`run_s` may be shown but are not part of the
  v1 `offload` object). While the job is live, the file can change at any
  time: re-read it each sample; a rename guarantees a complete document.
- Link to the worker side as the rules above say: worker job id
  `offload.remote_job_id` on machine `offload.worker`, owner `WHO@<laptop
  hostname>`.

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
