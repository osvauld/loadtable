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
                 "cpu_busy_cores": [ ], "mem_used_bytes": [ ] },
    "cargo_jobs_max": 2               // optional (feature/mcp-placement): admission's effective Cargo
                                      // jobs cap; absent in older snapshots = unknown
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
    "progress": { "crates_compiled": 212, "crates_expected": 310 | null,   // cargo only; null otherwise
                  "test_targets_built": 3 },      // optional (feature/log-search); absent until one is seen
    "warmth": "cold" | "warm" | "unknown",
    "warmth_reason": "no build output for this checkout on this machine",
    "estimate": {                                 // null if no history: the UI shows "unknown"
      "total_s": 420.0, "remaining_s": 180.0, "samples": 6,
      "basis": "median of 6 warm runs of this kind on this machine"
    },
    "result": {                                   // finished jobs only
      "exit_code": 0, "wall_s": 0.0, "cpu_user_s": 0.0, "cpu_sys_s": 0.0,
      "peak_rss_bytes": 0, "crates_compiled": 0, "warmth": "warm",
      "test_targets_built": 9, "broad_test_build": false   // optional (feature/log-search)
    },
    "offload": {                                  // laptop-side record of an offload, else null
      "worker": "abe@192.168.1.7", "remote_job_id": 93, "transport": "ssh"|"iroh",
      "sync_s": 1.0, "fetch_s": null
    },
    "compiling": [ {                              // optional (feature/log-search): executing jobs only
      "tool": "cargo", "proc": "rustc", "unit": "profile_sync", "crate": "profile_sync",
      "kind": "lib"|"bin"|"test"|"build-script"|"build-script-run"|"doctest"|"doc"|"cargo",
      "pid": 4242, "elapsed_s": 12.4, "cpu": 0.98 | null, "rss_bytes": 0
    } ],
    "broad_test_build": true                      // optional: only present (true) for a broad cargo test
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

## feature/metrics: how the local daemon fills the snapshot

No field above was renamed, removed or retyped. The notes below pin down
meanings the sketch left open; readers written against the sketch still work.
Example documents generated from the Rust types (`src/metrics/types.rs`) are
`tests/fixtures/snapshot/metrics-{laptop-busy,worker-busy,idle}.json`
(synthetic hosts/owners/paths; regenerate with
`cargo test --lib metrics::tests::fixture_tests::write_fixtures -- --ignored`).

Transport. `tasks status --json` prints the document as ONE line. It asks the
daemon with a new socket action `{"version":2,"action":{"type":"status"}}`; the
reply is `{"version":2,"error":null,"status":{...snapshot...}}`, a separate
shape from the `Response` of the existing actions (all of which are
unchanged). An older daemon answers that action with an `error` (unknown
variant); the client reports that the running daemon predates `status`.
`tasks status` without `--json` prints a short text summary.

Machine.
- `id`: `/proc/sys/kernel/hostname`. `role`: `TASKS_MACHINE_ROLE=host|worker`
  in the daemon's environment, else `worker` if `~/offload/checkouts` exists
  (the tasks-offload landing directory), else `host`.
- `cpu_busy_cores`: `/proc/stat` aggregate delta, (total - idle - iowait) /
  total x logical CPUs; null until two samples. `history` keeps <= 150 points.
- `pressure`: PSI avg10 values (null if PSI is unreadable). `blocker`:
  admission's compile-gate pressure blocker under the worker's recorded
  thresholds (disk-reserve waits are not included here; they appear in the
  waiting job's `queue.waiting_for`).
- `slots`: always `p2p-build`, `p2p-run-0..3`, `p2p-offload-0..3`, plus every
  `p2p-device-*` lock file present. `busy` comes from `/proc/locks` (the
  sampler never probes a lock, so it cannot steal a slot even briefly);
  `holder_job`/`holder_who` from the native holder note `OWNER:job-ID` when its
  process is alive (legacy holders: `holder_job` null, `holder_who` their text).
  Additive `holder_processes` (omitted when empty): when a slot is busy and no
  live note names its holder, the processes that really hold the flock, as
  `[{ "pid": 2298, "cmd": "sccache", "tasks_job": 154 | null }]` (bounded
  `/proc/PID/fdinfo` scan; `tasks_job` is the `TASKS_JOB_ID` in its
  environment, i.e. the job that started it). A waiting job's
  `queue.waiting_for` then reads e.g. `compile slot held by pid 2298 (sccache)
  from job #154, which has ended (the process outlived its job)` instead of
  `an untracked holder`, which remains only when no holder is visible.

Jobs.
- Order: by id ascending; all non-terminal jobs (including unapproved
  proposals) plus the newest 50 terminal ones.
- `command` and `kind_key` are the same string: `env`/`NAME=value` prefixes,
  `-j N`/`-jN`/`--jobs[=]N`, `--locked`, `--offline` dropped (before `--`
  only); the program becomes its basename without a Cargo `-<16 hex>` hash;
  absolute and `./`, `../`, `target/` argument paths become basenames; inside
  `sh -c TEXT` the same rules apply per whitespace word; a `tasks-offload`
  client becomes `[offload] ` + its inner command. Flag order is kept. Typed
  suites (self-suite, Android trial) use `"<suite> <configuration>"` and `argv: []`.
- `resource`: from the recipe (`compile` for typed suites); `device` is the adb
  serial of a device job.
- `checkout`: `spec.source`, except a worker path under
  `.../offload/checkouts/<id>/<sub>` whose `<id>.identity` names the laptop
  `toplevel`: then `toplevel/<sub>`. `checkout_name`: basename of that
  toplevel, else of the nearest directory holding `.git`, else of the path.
- `queue` (null unless waiting): `queued` -> `approval (tasks approve-command ID)`;
  `approved` -> `worker pickup` (or `worker not running (tasks start-worker)`);
  `preparing` -> the worker's "Waiting for disk reserve/target lease" detail,
  else null; `waiting_build`/`waiting_run` -> `position` is the 1-based place
  among live admission wait notes of the same slot label in admission's own
  FIFO order (wait-file mtime, then path); `waiting_for` is, in admission's
  order of checks: the gate's pressure blocker text; else `compile slot held by
  #N` / `all 4 run slots busy (#a, #b, ...)` / `device SERIAL slot held by #N`;
  else `behind #N in the LABEL queue`; else `admission check`.
- `live` (building/running only): one sampling thread, one pass every 2 s.
  Each job's tree is read directly (`/proc/PID/task/*/children` from the
  supervisor, then each member's `/proc/PID/stat`); one full `/proc` walk every
  15th tick (30 s) finds group orphans, and every tick if the kernel lacks the
  children files. Measured (`metrics::tests::sampler_cost`, debug build): a
  tick with 4 running jobs and a 304-row ledger costs 2.6 ms CPU on the worker
  and 6.4 ms on the laptop (<= 0.33% of one core). Members = the job's stage supervisor (`tasks __stage`, recorded in
  the ledger) and all its descendants by parent pid, plus every process in a
  process group of one of those (keeps orphans that stayed in the job's
  admission group). A process that left the group/session AND is no longer a
  descendant (a double-forked adb or sccache server) is NOT counted: it
  outlives the job and serves others. `cpu_cores` = delta of
  utime+stime+cutime+cstime over members / interval (reaped children count);
  `rss_bytes` = summed RSS (shared pages counted per process: an overcount);
  the counts include the three small `tasks` wrapper processes.
  Limitation: with sccache as rustc wrapper, cache-miss compiles of dependency
  crates run inside the sccache server, so a compile job's `cpu_cores` misses
  them (the machine total does not).
- `progress` (Cargo commands, live jobs only; finished jobs carry
  `result.crates_compiled`): `crates_compiled` counts `Compiling ` lines in the
  job log as they arrive; `crates_expected` = median of earlier successful runs
  of the same `kind_key` on this machine: cold runs if this job is cold, else
  warm runs; null without such history.
- `warmth`: (1) before output placement the worker checks the profile's output
  directory (`target[/TRIPLE]/debug|release|PROFILE`, following tasks' cache
  link; `--target-dir`, `CARGO_TARGET_DIR=`, `--manifest-path` respected): no
  `.fingerprint` entries -> `cold`, else `unknown`; written to
  `jobs/ID/warmth.json`. (2) Compiling lines vs R = median crate count of
  earlier cold runs of this kind (else the largest count of any successful
  run): more than 20% of R -> `cold`; build finished with <= 20% of R ->
  `warm`; build finished with 0 crates -> `warm`; otherwise `unknown`.
  Non-Cargo commands and offload clients stay `unknown` with that reason.
- `estimate` (non-terminal jobs; null without history): median of the newest
  <= 20 successful runs of the same `kind_key` with the same warmth; else of
  any warmth, said in `basis`. Durations are stage execution seconds (queue
  and admission waits excluded). `remaining_s` = max(0, total - finished stage
  seconds - time since the current stage was admitted). History includes every
  ledger row written before metrics existed ("backfill", read-only): they have
  argv and stage seconds, and count as warmth `unknown`.
- `result` (terminal jobs): `exit_code` of the last stage; `wall_s` = stage
  execution seconds (waits excluded); `cpu_user_s`/`cpu_sys_s` from
  getrusage(RUSAGE_CHILDREN) around the command's wait in its own per-job
  `__command` process (exact for every waited-for descendant, correct with
  concurrent jobs; daemonized processes excluded); `peak_rss_bytes` = max of
  the sampler's largest summed RSS and the largest single process
  (ru_maxrss); `crates_compiled` from the job log. Null = not measured (e.g.
  jobs from before metrics, or run by an older worker).
- `offload`: always null from feature/metrics (feature/status-stream fills it).

Persistence. A finished local command gets `metrics` inside its existing
execution JSON (`{"warmth","warmth_reason","wall_s","cpu_user_s","cpu_sys_s",
"peak_rss_bytes","crates_compiled"}`), merged at JSON level by the daemon
(other keys untouched, never overwritten) once the job is terminal. No SQLite
schema change; older releases ignore the key. Per-stage usage and start
warmth also stay as small files in the job directory (state dir, not Git).

## feature/log-search: live compile attribution and test scope (additive, optional)

All new fields are optional with serde defaults and are omitted when absent, so older
readers and the fixtures are unchanged (the contract test checks that).

- `jobs[].compiling` (executing jobs): the compiler processes in the job's process tree at
  the last 2 s sample, largest `elapsed_s` first, at most 32. The sampler adds no `/proc`
  walk: it already reads each member's `/proc/PID/stat`; the comm field (now kept in
  `ProcStat`) decides whether `/proc/PID/cmdline` is read, only for `rustc`, `rustdoc`,
  `clippy-driver`, `sccache`, `cargo`, `cargo-clippy` and `build-script-*`. Attribution
  (`metrics::compile`, Cargo/rustc only for now): the first of the leading three argv words
  whose basename is the compiler (so `sccache rustc ...`, `clippy-driver rustc ...` and an
  interpreter-run script `/bin/sh .../rustc ...` work); `unit`/`crate` = `--crate-name`
  (`rustc -vV` and other runs without it are not compiles); `kind` = `test` with `--test`,
  `build-script` for `build_script_*`, `lib` for `--crate-type lib|rlib|dylib|cdylib|
  staticlib|proc-macro`, else `bin`; `rustdoc --test` is `doctest`; a running
  `target/.../build/X-HASH/build-script-build` is `build-script-run` of `X`; a `cargo`
  process lists its `-p` packages (`unit` may be empty). `cpu` = cores over the last sample
  interval (lifetime average on first sight); `rss_bytes` that process's RSS.
  Limits: with sccache, a cache-miss compile runs inside the sccache server (outside the job)
  and shows as the sccache client; compiles shorter than the sample interval are missed.
- `progress.test_targets_built` / `result.test_targets_built` (Cargo): distinct test
  binaries named by the log's `Running ...`/`Executable ...` lines plus `Doc-tests X`, or the
  number of distinct `rustc --test` crates seen live, whichever is larger. Persisted in
  `execution.metrics.test_targets_built` (additive) when the job finishes.
- `broad_test_build`: on a job only when true; in `result` / `execution.metrics` true or
  false for `cargo test` commands. A `cargo test` with a name filter (before or after `--`)
  and no target selector (`--lib`, `--test T`, `--tests`, `--bin B`, `--bins`, `--doc`,
  `--example(s)`, `--bench(es)`, `--all-targets`) builds every test target of the package
  (`metrics::testscope`). The submission policy (`src/policy.rs`, README "Policy") refuses
  or warns about it at submit.

## `health` (feature/health-alerts; additive, optional)

`health` is an optional top-level field (serde default; absent from older daemons and
before the first check pass, so readers must treat it as optional). It reaches the
controller over the peer status stream unchanged, so the controller sees each worker's
disks, alerts and capability marks.

```jsonc
"health": {
  "node": "nixos", "checked_at": 1792216000.0, "interval_s": 30.0,
  "alerts": [ {                                   // active, most severe first
    "id": "disk_reserve:kurukkan:/home/u/build-cache",   // KIND:NODE[:SUBJECT], stable
    "kind": "disk_reserve", "severity": "info"|"warn"|"critical",
    "node": "kurukkan", "origin": "kurukkan", "summary": "...", "evidence": ["..."],
    "fix": "...", "first_seen": 0.0, "last_seen": 0.0, "cleared_at": null,
    "count": 3, "needs_human": true, "check": "disk"
  } ],
  "recent": [ /* up to 10 recently cleared alerts, same shape */ ],
  "disks": [ {
    "filesystem": "/home/u/build-cache", "roots": ["cache=/home/u/build-cache", "..."],
    "total_bytes": 0, "free_bytes": 0, "reserve_bytes": 0, "reserve_class": "cache"|"state",
    "growth_bytes_per_h": null, "time_to_full_s": null,
    "top": [ { "path": "...", "bytes": 0, "checkout": null } ],
    "scanned_at": null, "scan_truncated": false
  } ],
  "scan": { "at": 0.0, "elapsed_s": 0.0, "cpu_s": 0.0, "entries": 0, "truncated": false,
            "max_entries": 300000, "max_s": 15.0, "runs": 1 },
  "capabilities": [ {                             // missing-capability marks
    "node": "worker", "capability": "libclang", "family": "libclang", "job": 456,
    "remote_job": 77, "kind_key": "cargo build", "line": "Unable to find libclang: ...",
    "marked_at": 0.0, "expires_at": 0.0, "source": "forwarded job log"
  } ]
}
```

Rules:
- Thresholds: `TASKS_HEALTH_*` environment variables, else the `health::` section of the
  policy file (`src/policy.rs`, `deploy/policy.huml`), else defaults (`health::config`).
- Each daemon runs the checks on its own thread every `TASKS_HEALTH_INTERVAL_S` (30 s),
  never on a socket thread. Free/total come from statvfs on every pass; top consumers come
  from a bounded read-only `tmp_usage` scan (`TASKS_HEALTH_DISK_SCAN_S`, default 600 s,
  bounded by `TASKS_HEALTH_DISK_SCAN_FILES`/`_MAX_S`, never follows symlinks) on a
  second low-priority thread. Growth and time to full come from a small history in
  `<state>/health/disk-history.json`.
- A machine publishes only the alerts it raised; the controller (`tasks alerts`, MCP
  `alerts`/`cluster_status`/`submit`, the dashboard banner) merges its own and each peer's.
- Capability marks: see `src/health/capability.rs` (pattern table in `classify.rs`).
  Placement `anywhere` skips a peer with a mark the command needs; a named peer gets a note.
- `health.probes` (additive): capability probes of the daemon's environment,
  `{name, declared, ok, detail, fix}`. `TASKS_DECLARED_CAPABILITIES` (comma list from the
  node's declaration, e.g. `libclang,jdk,android-sdk,cargo-ndk,htop`) marks what the node
  promises; a declared probe that fails raises `declared_capability:NODE:NAME` and adds a
  transient mark (source `declared capability probe`) to `health.capabilities`.
- Node-setup alert ids (docs/design/node-setup-policy.md): `slot-holder:NODE:LOCK` (a
  slot-lock held with no live job; evidence: the `/proc/locks` entry plus the holder
  processes from fix/lease-leak's `admission::holders` scan, i.e. the slot's
  `holder_processes` when the snapshot has them, worded like `waiting_for`: pid, command,
  originating `TASKS_JOB_ID` and "which has ended"), `compiler-cache:NODE:sccache` (sccache server started inside a
  job, holding a `p2p-*.lock` descriptor, or not running when `RUSTC_WRAPPER` is sccache),
  `declared_capability:NODE:NAME`. `tasks doctor` prints them with fix lines.
