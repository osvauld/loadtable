# tasks-offload: SSH/rsync offload v0

Runs one command on a second Linux machine's **own** task manager, from inside a
local Git checkout. Stdlib-only Python 3.11+; needs `git`, `ssh` and `rsync`
locally and `python3`, `rsync`, `sha256sum` and an installed
`~/.local/bin/tasks` on the worker.

Status: in use against the real NixOS worker (192.168.1.7): p2pcore tests,
portable desktop builds and Android APKs (worker jobs 42, 45, 55). The
synthetic fake-worker suite below still covers the transport and edge cases.

## Usage

Run on the host under the host ledger, holding only an offload slot there:

```sh
cd /path/to/checkout/or/subdir
tasks run --offload --who OWNER -- /abs/path/scripts/tasks-offload \
  --worker abe@192.168.1.7 --ssh-key ~/.ssh/OFFLOAD_KEY --who OWNER \
  --cargo-fetch -- cargo test --offline --locked -j 2
```

### Host offload slot (`tasks run --offload`)

The host process only scans, hashes, transfers and waits on ssh, so it has its
own admission class instead of a run slot:

- **Own pool:** four `p2p-offload-{0..3}.lock` files beside the existing locks.
  It never takes the compile (`p2p-build.lock`), run (`p2p-run-N.lock`) or
  device locks, and they never wait for it. Four lets four checkouts/agents
  offload at once (one checkout is serialized by the client flock anyway) while
  bounding local ssh/python clients; the worker's own admission queues the
  real work. The host worker also gives offloads their own claim budget (one
  per slot), so waiting offloads don't use up the five general job threads.
- **Gates kept:** fail-closed PSI parsing; available memory at least
  `min(TASKS_MIN_AVAILABLE_MEMORY_MIB, 2 GiB)` (a small client, but the iroh
  transport holds whole blobs, up to 512 MiB, in memory); memory PSI full
  avg10 at most 0.5% (the host is thrashing); the cache-disk reserve
  (`TASKS_MIN_CACHE_FREE_GIB`, measured at `~/build-cache`, where fetched
  artifacts land; unmeasurable fails closed). The state-disk reserve still
  applies to every job as before.
- **Gates dropped:** CPU PSI (including `TASKS_RUN_CPU_SOME_MAX_PCT`), memory
  PSI some and I/O PSI. The client uses almost no CPU, and these are exactly
  what a local compile raises, i.e. when offloading helps most.
- **Allowlist:** the command (argv[0], optionally after `env NAME=value...`)
  must be named `tasks-offload` or `offload-portable-desktop.sh`; anything
  else is refused at submission, daemon approval and execution, so the
  ungated slot cannot run local heavy work. This is a guard rail against
  mistakes, not a sandbox.
- **Visibility:** `tasks list` shows resource `offload`; `tasks admission`
  lists the `p2p-offload-N` locks/holders, `offload_pressure_blocker`, and
  `thresholds.offload_min_available_memory_bytes`/`offload_slots`.
- **Compatibility:** stored as `"resource":"offload"` in the existing job
  configuration JSON (no SQLite schema change). A release without the class
  still lists such a job (resource `suite`) but refuses to approve or run it.
  `tasks run --run ... -- tasks-offload ...` keeps working unchanged; use it
  with an installed release that rejects `--offload`.

```
tasks-offload --worker USER@HOST [--ssh-key PATH] --who OWNER
              [--fetch RELPATH]... [--fetch-dest DIR] [--cargo-fetch]
              [--remote-run] [--checksum] [--dry-run]
              [--ssh-program P] [--rsync-program P] [--remote-python P]
              [--max-files N] [--max-bytes N] [--cancel-wait SECONDS]
              [--transport ssh|iroh] [--lan-peer PATH] [--iroh-data-dir DIR]
              [--iroh-allow FILE] [--iroh-addr IP:PORT] [--iroh-bind IP:PORT]
              [--iroh-store DIR]
              -- CMD [ARGS...]
```

| Option | Meaning |
|---|---|
| `--who OWNER` | `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`; the remote job owner is `OWNER@<local hostname>`. |
| `--fetch REL` | Path relative to the **remote checkout root** (e.g. `target/release/foo`), copied back after a passed job. Repeatable. |
| `--fetch-dest DIR` | Default `~/build-cache/offload/OWNER/REMOTE_JOB_ID/`, keeping `REL`'s directories. Refused if inside the local checkout. |
| `--cargo-fetch` | `cargo fetch --locked` on the worker first (online, outside the remote ledger) so `--offline` commands work. |
| `--remote-run` | Submit with remote `tasks run --run` (run-only slot; the command must not compile). Default is the remote compile slot. |
| `--checksum` | rsync `--checksum` instead of size+mtime quick check. |
| `--dry-run` | Build the manifest and print the plan; runs no ssh/rsync at all. |
| `--ssh-program`, `--rsync-program` | Transport seam for tests; also env `TASKS_OFFLOAD_SSH`, `TASKS_OFFLOAD_RSYNC`. |
| `--transport iroh` | Experimental: move files with `lan-peer` (iroh blobs) instead of rsync; the command still runs over ssh. See "iroh transport" below. Default `ssh`. |
| `--lan-peer PATH` | `lan-peer` binary (iroh); also env `TASKS_OFFLOAD_LAN_PEER`. Default `lan-peer` on `PATH`. |
| `--iroh-data-dir DIR` | This host's `lan-peer` installation (from `lan-peer init`). |
| `--iroh-allow FILE` | The worker's `attestation.json`. Its `endpoint_id` is the peer that is dialed; `lan-peer` verifies the attestation before dialing. |
| `--iroh-addr IP:PORT` | The worker's `lan-peer serve --bind` address, e.g. `192.168.1.7:7842`. |
| `--iroh-bind IP:PORT` | This host's LAN address, e.g. `192.168.1.25:0`. Required unless `--iroh-addr` is loopback. |
| `--iroh-store DIR` | Local iroh-blobs store for `--fetch`. Default `~/build-cache/offload/.iroh-store`. |

Last stdout line is a one-line JSON summary: `worker`, `remote_dir`,
`remote_cwd`, `remote_job_id`, `remote_phase`, `remote_state`
(`terminal`/`uncertain`/`not_submitted`/`dry_run`), `exit`, `synced_files`,
`synced_bytes`, `excluded_files`, `deleted_tracked`, `pruned` (first 200),
`pruned_count`, `fetched`, `fetch_dest`, `cancel_requested`, `dry_run`,
`durations` (`scan_s`, `sync_s`, `cargo_fetch_s`, `run_s`, `fetch_s`, `total_s`),
`transport`, and with `--transport iroh` also `iroh`: `peer`, `addr`, `sync`
(`files`, `blobs_sent`, `bytes_sent`, `created`, `unchanged`, `written`,
`mode_fixed`, `deleted`, `kept_non_source`) and `fetch` (`files`,
`blobs_received`, `bytes_received`, `skipped`).

Exit status: the remote job's exit (from the remote `job N PHASE (exit C)`
line: 0 passed, 130 cancelled, else the stage exit); 2 usage/source refusal;
255 transport failure **or** uncertain remote state (check `remote_state`);
130/143 after cancelling on SIGINT/SIGTERM; 1 if a fetch failed.

## What happens

1. **Source set.** `git rev-parse --show-toplevel` (works with an unborn HEAD),
   then `git ls-files -z -c -o --exclude-standard`: tracked (index), dirty and
   untracked-not-ignored files. Every path goes through `worker_source`'s rules
   (`check_path`, `open_root`, `open_parent`, marker-based `target`/`build`):
   credentials, VCS, tool caches, logs/db/keys and build outputs are skipped and
   counted in `excluded_files`. Paths are opened no-follow from directory
   descriptors; a path through a symlinked directory, a symlink whose target is
   absolute or leaves the tree, special files and non-UTF-8 names are refused.
   In-tree relative symlinks are synced as links. Tracked-but-deleted files are
   counted in `deleted_tracked`. Bounds: 50000 files, 1 GiB/file, 4 GiB total
   (`--max-files`, `--max-bytes`). Each file is SHA-256 hashed streaming (no
   contents held in memory) into a manifest of path, type, size, mode, exec bit.
2. **Sync.** Remote dir `~/offload/checkouts/<sha256(hostname NUL toplevel)[:16]>/`.
   Offloads of one checkout are serialized by an exclusive `flock` on
   `${XDG_STATE_HOME:-~/.local/state}/tasks-offload/locks/<id>.lock`, held
   from sync until the client exits, so a second sync never rewrites the
   worker copy under a running job; the waiter logs `waiting for another
   offload of this checkout`.
   The client installs `~/.local/lib/tasks-offload/agent.py` (from
   `scripts/offload_agent.py`) and a copy of `worker_source.py` beside it,
   only when their SHA-256 differs. `agent prepare` creates the dir and binds it
   to this host+toplevel (`<id>.identity` sibling; a mismatch is refused).
   `rsync -a -s --from0 --files-from=LIST` copies the listed paths. `agent apply`
   then prunes worker files absent from the manifest **only if** worker_source
   would treat them as source; `target`, `android/build`, `android/app/build`,
   marker-adjacent `target`/`build` and every excluded name are never pruned or
   descended. It then verifies every manifest entry (type, size, exec bit,
   SHA-256). Mismatches are re-sent once with `--checksum`; a second mismatch
   refuses to run ("checkout changed during sync").
3. **Run.** `ssh W 'cd "$HOME"/offload/checkouts/<id>/<subdir> && exec
   "$HOME"/.local/bin/tasks run --who OWNER@host [--run] -- <shlex-quoted argv>'`.
   The local subdirectory of the toplevel is mapped to the same remote subdir.
   Output is streamed as the remote client emits it (note: installed `tasks run`
   prints phase changes live but the job log only at the end). The job id is
   parsed from `tasks: submitted job N;` on stderr.
4. **Cancel.** On SIGINT/SIGTERM the client immediately launches
   `ssh W '"$HOME"/.local/bin/tasks cancel N'` for its own job id only, in its
   own session, then waits (`--cancel-wait`, default 120 s) for the remote
   terminal line and exits 130/143. The installed host worker escalates
   SIGTERM to SIGKILL after ~300 ms; the detached canceller still completes, but
   the client's summary may then be lost (query the worker). If no job id was
   seen within 10 s of the signal, nothing is cancelled and the state is reported
   uncertain.
5. **Disconnect.** If ssh ends without the remote terminal line, the client
   prints `UNCERTAIN: remote job N ...` with `tasks logs N` to run on the worker,
   exits 255 and **never resubmits**.
6. **Fetch.** After a passed job only, each `--fetch` path is rsync'd from the
   remote checkout into the destination. Never into the local checkout.

## iroh transport (experimental, phase 2b)

`--transport iroh` replaces rsync, the helper install and the prune/verify
helper with content-addressed transfer through `lan-peer`
(`experiments/osvauld-foundation`). Selection, path rules, the checkout id, the
per-checkout client lock, the ssh `tasks run`, cancellation and uncertainty
handling are unchanged.

1. **Selection** is the same (`git ls-files` + `worker_source` rules). In-tree
   symlinks are refused with this transport (its manifest carries regular files
   only); use `--transport ssh` for such checkouts.
2. **Hashing.** The selected files are listed (NUL-separated) and hashed by
   `lan-peer manifest` (BLAKE3 = iroh-blobs raw hash). The result is merged into
   a `worker_source` manifest with `attach_iroh_blobs_hashes`, which refuses any
   difference in path, size, exec bit or SHA-256 from the client's own scan
   ("checkout changed while hashing").
3. **Sync.** `lan-peer sync-push --checkout <id>` sends only the blobs the
   worker's store lacks, then the worker updates its stable checkout
   `<checkouts>/<id>` in place under a per-checkout lock: unchanged files (same
   BLAKE3, size and exec bit) are not rewritten, so their mtime is kept and
   Cargo stays incremental; changed files are replaced atomically (no-follow
   temp + fsync + rename); files absent from the manifest are pruned with the
   same rules as `offload_agent.py` (`target/`, marker-adjacent outputs,
   excluded names and cache links are never deleted). Every file is verified
   after writing. The worker's signed reply names the checkout's absolute path;
   the ssh command `cd`s there (it must end in `/<id>`).
4. **Run** over ssh exactly as v0 (`tasks run` on the worker). No command is
   executed over iroh: there is no authority model for it yet.
5. **Fetch** after a passed job: `lan-peer fetch --checkout <id> --path REL...
   --store STORE --dest DEST`. The worker validates the paths (inside the
   checkout, no symlinks; symlinks inside requested directories are skipped and
   listed), imports them into its store and answers with a signed manifest; this
   host requests only hashes missing from `--iroh-store`, BLAKE3-verifies each,
   and materializes into `DEST`, which **must not exist** (default
   `~/build-cache/offload/OWNER/REMOTE_JOB_ID`, as v0). An unchanged artifact
   fetched again moves zero blob bytes.

Worker side: `lan-peer serve --bind LAN_IP:7842 --data-dir DIR --allow
CLIENT_attestation.json --store ~/offload/iroh-store --checkouts
~/offload/checkouts` (template unit: `deploy/worker/lan-peer.service`). Using
`~/offload/checkouts` makes the iroh-synced checkout the same directory, with the
same warm `target/`, as v0's rsync checkout. UDP 7842 must be open on the LAN
interface (`nix/taskworker-host.nix`).

Differences from v0: no `<id>.identity` binding is written (that is done by the
ssh helper, which this transport does not install); a 64-bit checkout-id
collision is not detected. The account is the experiment's SYNTHETIC public
test-vector account; attestations expire after 7 days. Limits are the
experiment's (20 000 files, 64 MiB per file, 512 MiB total), smaller than v0's,
and whole blobs are held in memory.

## Security model: SSH v0 vs. the iroh plan

v0 relies entirely on the existing OpenSSH trust: key-only auth with
`BatchMode=yes` (no password prompts), the user's `known_hosts` for host
identity, and LAN reachability. Anyone who can SSH as the worker user can do
far more than this tool; the tool adds no authority of its own. It contacts
only the given `--worker`, runs only the composed `tasks run`/`tasks cancel`/
helper commands, and quotes argv with `shlex`, which assumes a POSIX-compatible
login shell on the worker (bash/zsh; fish is unsupported). The remote command
runs with the worker user's full rights in a live directory: no sandbox.
Excluding credential-looking names is defense in depth, **not** a secret
scanner: review what the checkout contains. SHA-256 here is a local integrity
check, not iroh-blobs/BLAKE3 content addressing and not an approval receipt.

The iroh plan (REMOTE-WORKER-HANDOVER.md, experiments/osvauld-foundation)
replaces the `Transport` class with: distinct per-installation keys bound to
the account, explicit LAN-only endpoints with public relays/discovery disabled,
a task-specific ALPN, per-job authorization bound to the exact manifest and
execution spec, missing-blob transfer instead of rsync, and isolated
per-attempt materialization. None of that exists in v0.

## Limitations

- **Host-specific environment.** Only argv is forwarded; the remote tasks
  daemon's environment applies. Paths like `VPX_DIR=/home/abe/build-cache/...`
  or other host-local absolute paths/env vars will not exist on the worker. Pass
  needed non-secret inputs explicitly (`-- env K=V cmd`), valid on the worker.
- **No remote cache-output placement.** The remote checkout has no `.git`, so
  the worker's `tasks` does not place cache-backed `target` links; Cargo writes
  a real `target/` inside the remote checkout dir. It is preserved across syncs
  and never pruned; reclaiming it is a manual, separately authorized step.
- **Excluded inputs.** worker_source rules also skip `.cargo/` (e.g.
  `.cargo/config.toml`), `.config/`, `.env*`, top-level `build`/`dist`/`out`/
  `sdk`/`cache` and `*.db`/`*.log`/`*.key` etc. A build that needs these fails
  on the worker; check `excluded_files` and `--dry-run` output first.
- **Submodules / nested repos unsupported** (`--recurse-submodules` cannot be
  combined with `-o`); they are refused with a clear error.
- **Android/Gradle unsupported**: no SDK/NDK/Gradle provisioning, devices or
  emulators on the worker. Rust cargo check/test/clippy/build is the target.
- **Not a snapshot.** The checkout must stay stable during scan+sync; post-sync
  verification catches changes, but a live edit can still race the run itself.
  The remote dir is shared by consecutive offloads of the same checkout:
  do not run two offloads of one checkout concurrently.
- `--cargo-fetch` runs outside the remote ledger and needs `cargo` on the
  non-interactive ssh PATH (on NixOS: system profile, not only a dev shell).
- Under host `tasks run`, the caller's environment (e.g. `SSH_AUTH_SOCK`) is not
  inherited: use `--ssh-key` with a key ssh can read non-interactively, or an
  `~/.ssh/config` `IdentityFile`. `known_hosts` must already trust the worker.
- Worker requirements: `python3` (NixOS: add it to the system or user profile),
  `rsync`, `sha256sum`, and an installed `~/.local/bin/tasks` with the same `run`/`cancel` CLI output.
- Transport failure detection relies on ssh keepalives (15 s x 4).

## Validation

`tests/test_tasks_offload.py` (synthetic only): an ssh shim that ignores the
host and runs the remote command with `sh -c` under a temporary fake HOME, real
local rsync through the shim, and a fake `~/.local/bin/tasks` mimicking the
installed client's `run`/`cancel` output and exit codes (format taken from
`src/bin/tasks/main.rs`). Shared jobs, owner `offload-client`, run-only:

- Job 114 (red, before the client existed): 48 run, 15 failed, 33 passed.
- Job 115 (green): `env PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover
  -s tests -p 'test_*.py' -v`: 51 passed (19 offload + 32 existing), 0 failed,
  0 skipped.

A `--dry-run` against this checkout (worker `abe@example.invalid`) selected
97 files and ran no ssh. That suite exercises no real worker; real runs since
then are listed under Status and in the iroh validation below.

### iroh transport validation (owner `osv-blobs`)

Synthetic tests: Python red job 234, green job 243 (66 tests, 0 skipped,
including a real `lan-peer` loopback offload). Experiment crate: jobs 226/237
(sync red/green), 237/241 (artifacts red/green), 241/242 (CLI red/green; 28
tests and warnings-denied all-target clippy).

**Two real machines** (2026-10-17): client 192.168.1.25 (Arch) and worker
192.168.1.7 (NixOS, Wi-Fi, UDP 7842 open on wlo1). Release `lan-peer` built by
host job 244, copied to the worker's `~/.local/bin`. Synthetic installations
`~/lan-peer-client` and `~/lan-peer-worker`; `serve` ran as a temporary
`nohup` process and was stopped afterwards. This repository's checkout was
126 files / 1.64 MB.

| Step (host job / worker job) | Sync | Blobs / bytes sent | Run | Fetch |
|---|---|---|---|---|
| First offload, `--cargo-fetch`, `cargo test --locked -j 8` (247/47) | 1.91 s | 126 / 1 636 646 | 186.2 s (cold build) | — |
| No change, same test (248/48) | 0.47 s | 0 / 0 (126 unchanged) | 5.2 s (no rebuild: mtimes kept) | — |
| One-file edit, test + all-target clippy (249/49) | 0.40 s | 1 / 15 148 | 21.6 s | — |
| Revert edit, `--fetch` 31.9 MB test binary (250/50) | 0.43 s | 0 / 0 (1 written from store) | 0.9 s | 7.40 s, 1 blob / 31 927 720 B |
| Same fetch again (251/51) | 0.34 s | 0 / 0 | 1.0 s | 0.71 s, 0 blobs / 0 B |
| v0 ssh/rsync, no change, same checkout (252/52, 253/53) | 1.17–1.19 s | rsync | 1.0 s | — |

Both fetched copies and the worker file have the same SHA-256. The `tasks`
binary (196 MB) cannot be fetched: it exceeds the experiment's 64 MiB per-file
limit.
