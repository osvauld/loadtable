# tasks-offload: SSH/rsync offload v0

Runs one command on a second Linux machine's **own** task manager, from inside a
local Git checkout. Stdlib-only Python 3.11+; needs `git`, `ssh` and `rsync`
locally and `python3`, `rsync`, `sha256sum` and an installed
`~/.local/bin/tasks` on the worker.

Status: built and tested against a synthetic fake worker only (see Validation).
No real worker has been contacted. The first real run needs explicit permission.

## Usage

Run on the host under the host ledger, holding only a run slot there:

```sh
cd /path/to/checkout/or/subdir
tasks run --run --who OWNER -- /abs/path/scripts/tasks-offload \
  --worker abe@192.168.1.7 --ssh-key ~/.ssh/OFFLOAD_KEY --who OWNER \
  --cargo-fetch -- cargo test --offline --locked -j 2
```

```
tasks-offload --worker USER@HOST [--ssh-key PATH] --who OWNER
              [--fetch RELPATH]... [--fetch-dest DIR] [--cargo-fetch]
              [--remote-run] [--checksum] [--dry-run]
              [--ssh-program P] [--rsync-program P] [--remote-python P]
              [--max-files N] [--max-bytes N] [--cancel-wait SECONDS]
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

Last stdout line is a one-line JSON summary: `worker`, `remote_dir`,
`remote_cwd`, `remote_job_id`, `remote_phase`, `remote_state`
(`terminal`/`uncertain`/`not_submitted`/`dry_run`), `exit`, `synced_files`,
`synced_bytes`, `excluded_files`, `deleted_tracked`, `pruned` (first 200),
`pruned_count`, `fetched`, `fetch_dest`, `cancel_requested`, `dry_run`,
`durations` (`scan_s`, `sync_s`, `cargo_fetch_s`, `run_s`, `fetch_s`, `total_s`).

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
97 files and ran no ssh. No real worker, real remote `tasks` daemon, NixOS
host, real ssh/known_hosts or Cargo build was exercised.
