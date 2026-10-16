# Worker host: running `tasks` on a second Linux machine

Status: **template, never installed or validated on a worker host.** Nothing in
this repository installs the unit, enables lingering, copies a binary or
restarts a daemon. The installed runtime on the main host
(`~/.local/bin/tasks` there) is a separate pinned release and is not affected.

Remote clients SSH in and run `tasks run --who OWNER -- CMD` on the worker,
so the worker applies its own native admission (memory/PSI/disk gates and
flock slots). Admission on another machine never controls this host.

## Prerequisites on the worker

- A `tasks` binary built **on/for this host** from this source. A binary built
  on another distribution may not start (NixOS has no `/lib64` loader unless
  nix-ld is enabled). Build it through the worker's own queue once one exists,
  or directly the first time, with `cargo build --offline --locked -j 2`.
- Kernel PSI enabled: `/proc/pressure/{cpu,memory,io}` must exist. Without
  them the worker refuses to start (admission fails closed).
- `git` on the daemon's `PATH`. Every live command runs
  `git rev-parse --show-toplevel` in its cwd to place cache-backed output
  links. A missing `git` currently fails the job (see the audit in
  `feedback/tm-worker-portability-*-fresh-host.md`).
- The toolchain used by your commands (cargo/rustup etc.) on the **daemon's**
  `PATH`. The client's `PATH` is not forwarded; admitted commands get only
  `HOME, PATH, RUSTUP_HOME, CARGO_HOME, XDG_RUNTIME_DIR, LANG` from the daemon.

Created on first use, no manual setup needed: the state directory
`~/.local/state/tinline-task-manager-native` (or `$XDG_STATE_HOME/...`), the
admission lock directory `/tmp/claude-$UID`, and cache output roots under
`~/build-cache/p2p_chat/tasks/checkouts/` for Cargo checkouts in a Git work tree.
These names are retained compatibility namespaces, not Tinline dependencies.

## Admission thresholds

Read from the environment of the daemon/worker process (set them in the unit).
Each value is a plain whole number: no sign, spaces, decimals or suffix.
Invalid or out-of-range values are rejected, never clamped: `tasks start-worker`
(and therefore `tasks run`) reports the error, and an admission check that
cannot resolve its configuration fails closed.

| Variable | Unit | Default | Accepted range |
| --- | --- | --- | --- |
| `TASKS_MIN_AVAILABLE_MEMORY_MIB` | MiB of MemAvailable | 8192 (8 GiB) | 1024 – 1048576 |
| `TASKS_MIN_CACHE_FREE_GIB` | GiB free on cache/target disks | 20 | 1 – 65536 |
| `TASKS_MIN_STATE_FREE_MIB` | MiB free on the state disk | 256 | 64 – 1048576 |

PSI gates are fixed and not configurable: CPU some ≤ 40%, memory some/full ≤
1%/0.5%, I/O some/full ≤ 20%/10% (avg10). Swap does not count toward
MemAvailable. These are trial heuristics, not reservations or quotas.

Each worker records its effective values in `<state>/admission-thresholds.json`
when it starts. `tasks admission` reports `thresholds` (with `source`:
`worker-record`, `environment` or `default`) and computes `pressure_blocker`
with them, so a client shell without the variables still sees what the worker
applies. If the shell's own `TASKS_MIN_*` differ, a warning is listed. The
record reflects the **last** worker start; a changed unit applies only after the
worker restarts (it exits by itself when idle).

## Install (operator steps, not automated)

```sh
install -Dm755 /path/to/built/tasks ~/.local/bin/tasks
install -Dm644 deploy/worker/tasks-daemon.service ~/.config/systemd/user/tasks-daemon.service
$EDITOR ~/.config/systemd/user/tasks-daemon.service   # thresholds, PATH
loginctl enable-linger "$USER"   # or users.users.<name>.linger = true on NixOS
systemctl --user daemon-reload
systemctl --user enable --now tasks-daemon.service
~/.local/bin/tasks admission
```

Start the unit **before** clients connect. If a client reaches the worker first,
`tasks` autostarts a daemon with that SSH session's environment; the unit then
exits with "another daemon owns this state directory" and is not restarted in a
loop. Check `systemctl --user status tasks-daemon` and `tasks admission`.

Stopping the unit stops only the daemon (`KillMode=process`), never the worker
or admitted job process groups. It is not cancellation; inspect `tasks list`.

## Client usage over SSH

`~/.local/bin` may not be on the non-interactive SSH `PATH` (NixOS does not add
it). Use the absolute path, and `cd` into the checkout on the worker, because
commands run in a live cwd:

```sh
ssh worker 'cd /home/abe/src/project && ~/.local/bin/tasks run --who OWNER -- cargo test --offline --locked -j 2'
```

Do not run the unit with `PrivateTmp`, `ProtectHome` or a different
`TASK_MANAGER_STATE_DIR`: clients would then see a different lock directory,
socket or checkout than the daemon.
