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
  them the worker refuses to start and `tasks start-worker`/`tasks run`/
  `tasks admission` fail closed with the exact missing file, e.g.
  `/proc/pressure/memory: No such file or directory (os error 2). Admission
  requires kernel pressure stall information (PSI): a kernel with CONFIG_PSI=y,
  booted with psi=1 if it was built with CONFIG_PSI_DEFAULT_DISABLED=y.` There
  is no PSI-less mode. Check with `cat /proc/pressure/cpu`; on such kernels add
  `psi=1` to the kernel command line (NixOS: `boot.kernelParams = [ "psi=1" ];`).
- `git` on the daemon's `PATH`. Every live command runs
  `git rev-parse --show-toplevel` in its cwd to place cache-backed output
  links. A missing `git` currently fails the job (see the audit in
  `feedback/tm-worker-portability-*-fresh-host.md`).
- The toolchain used by your commands (cargo/rustup etc.) on the **daemon's**
  `PATH`. The client's `PATH` is not forwarded; admitted commands get only
  `HOME, PATH, RUSTUP_HOME, CARGO_HOME, XDG_RUNTIME_DIR, LANG` from the daemon.

Created on first use, no manual setup needed: the state directory
`~/.local/state/tinline-task-manager-native` (or `$XDG_STATE_HOME/...`), the
admission lock directory (below), and cache output roots under
`~/build-cache/p2p_chat/tasks/checkouts/` for Cargo checkouts in a Git work tree.
These names are retained compatibility namespaces, not Tinline dependencies.

## Admission lock directory (`TASKS_LOCK_DIR`)

The flock files (`p2p-build.lock`, `p2p-run-N.lock`, `p2p-device-*.lock`) live in
`/tmp/claude-$UID` by default. That exact path, and so the same lock inodes, is
shared with existing clients, scripts and the Tinline Python adapters
(`buildlock.py` etc.), so the default never changes. `TASKS_LOCK_DIR=/abs/dir`
moves it for one daemon/worker:

- It must be an absolute path other than `/`, without `.`/`..` components. An
  invalid value fails closed (`start-worker`, the worker and `tasks admission`
  report it).
- A missing directory is created with mode 0700 (only the last component; its
  parent must exist). An existing one is used as is but refused if it is a
  symlink, owned by another user or world-writable. Nothing chmods, moves or
  deletes it or its lock files.
- **Only processes that use the same directory are coordinated.** Legacy
  clients that hard-code the default (Tinline's Python helpers) are not
  coordinated with a worker that uses another directory. `tasks admission`
  warns whenever the inspected directory is not the default.

How SSH clients see the same directory as the daemon: clients never take these
locks themselves; only the daemon's worker does, using the daemon's environment.
Each worker records its directory in `<state>/admission-lock-dir.json` at start,
and `tasks admission` in any shell inspects the recorded directory
(`lock_dir.source = "worker-record"`). If the shell's own `TASKS_LOCK_DIR` (or
the default, when unset) differs, it adds a warning, because a daemon
**autostarted** from that shell would use the shell's value. So: set
`TASKS_LOCK_DIR` in the unit, start the unit before clients connect, and either
leave it unset everywhere or export the same value in the SSH environment.

### /tmp cleaning and `PrivateTmp`

- `systemd-tmpfiles` ages `/tmp` (the default `tmp.conf` uses 10 days).
  An idle lock file (or the whole `/tmp/claude-$UID`) can be removed while no one
  holds it. A later client recreates it, but a process still holding an old,
  unlinked inode would no longer exclude a new one, which breaks the "stable
  flock inode" guarantee. On a worker, either exclude the directory:

  ```
  # /etc/tmpfiles.d/tasks-locks.conf  (x = never age-clean this path or its contents)
  x /tmp/claude-*
  ```

  or set `TASKS_LOCK_DIR` to a directory that is never cleaned, e.g.
  `%h/.local/state/tasks-locks` in the unit (accepting that legacy default-path
  clients on that host are then not coordinated). A tmpfs `/tmp` is also emptied
  on reboot; that is harmless because no lock survives a reboot either.
- `PrivateTmp=true` (and `ProtectHome`, `ProtectSystem=strict` or a mount
  namespace) gives the daemon its own `/tmp`. Its workers would then lock a
  different `/tmp/claude-$UID` than other daemons, legacy clients and
  `tasks admission` in SSH shells, so admission would silently stop excluding
  them. The template unit deliberately sets none of these. The draft
  `nix/worker-module.nix` service sets `PrivateTmp = true` and must not be reused
  for this daemon unless it also sets `TASKS_LOCK_DIR` outside `/tmp` (with the
  legacy-client caveat above).

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
| `TASKS_MAX_CARGO_JOBS` | largest explicit Cargo jobs | 2 | 1 – 64 |
| `TASKS_RUN_CPU_SOME_MAX_PCT` | CPU PSI some avg10 limit for **run** slots only | 40 | 40 – 100 |

`TASKS_MAX_CARGO_JOBS` only widens what a command may request explicitly
(`-j N`, `-jN`, `--jobs N`, `--jobs=N`, or `env CARGO_BUILD_JOBS=N cargo ...`).
Commands that do not ask still get the injected `CARGO_BUILD_JOBS=2` on every
host, so raising the cap never changes default behaviour. Unset, a request
above 2 is refused with `trial safety: Cargo jobs must be 1 or 2 (default 2)`;
with e.g. 8, above 8 is refused with
`trial safety: Cargo jobs must be 1..=8 (TASKS_MAX_CARGO_JOBS)`. Set it in the
unit, like the reserves: the daemon checks it at approval and the worker at
execution, each from its own environment. The submitting client checks early
against the worker's recorded value (see below), so the first `-j` above 2 on
a fresh host is accepted only after a worker has started once with the unit's
setting. Shell scripts (`sh -c ...`) are not parsed and are not capped.

PSI gates are fixed: CPU some ≤ 40%, memory some/full ≤ 1%/0.5%, I/O some/full
≤ 20%/10% (avg10). Swap does not count toward MemAvailable. These are trial
heuristics, not reservations or quotas. The only exception is the opt-in
`TASKS_RUN_CPU_SOME_MAX_PCT`. It raises the CPU PSI limit for **run** slots
(`--run`, the run stage of Cargo tests, and the run admission of `--device`
commands) so light run-only jobs are not queued behind a CPU-bound compile.
It cannot go below 40, never changes compile admission, and never relaxes the
memory reserve, memory PSI or I/O PSI gates. Unset, behaviour is unchanged.
`tasks admission` reports it in `thresholds` and the resulting run-slot gate as
`run_pressure_blocker`. A blocked run slot names the raised limit, e.g.
`CPU PSI some avg10 above 80% (run slots, TASKS_RUN_CPU_SOME_MAX_PCT)`.

Each worker records its effective values in `<state>/admission-thresholds.json`
when it starts. `tasks admission` reports `thresholds` (with `source`:
`worker-record`, `environment` or `default`) and computes `pressure_blocker`
with them, so a client shell without the variables still sees what the worker
applies (including `max_cargo_jobs`). If the shell's own `TASKS_MIN_*`,
`TASKS_MAX_CARGO_JOBS` or `TASKS_RUN_CPU_SOME_MAX_PCT` differ, a warning is listed. The
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
socket or checkout than the daemon (see "Admission lock directory" above).

## Finding what uses /tmp and cache space

`tasks tmp-usage` is a read-only inventory of `/tmp`, `~/build-cache`,
`~/.cache` and the task state (or the absolute paths you pass). It groups by
top-level entry (expanding `/tmp/claude-$UID`, the per-checkout output cache
and job directories), reports allocated size, file count, age, owner uid, the
filesystem type (RAM-backed tmpfs is flagged), and hints: queue jobs whose cwd
is inside, the checkout an output cache or scratch directory belongs to, and
own processes whose cwd is inside. It never deletes, moves or writes anything,
does not follow symlinks or cross mounts, and stops at `--max-files`
(default 500000) or `--max-seconds` (default 20), marking results truncated.
Hints are not ownership proof; confirm with the owner before removing anything,
and never remove admission lock files. `--json` gives the full report.

## Portable desktop builds from the worker

A plain worker build does not run on another distro: its interpreter is
`/nix/store/…-glibc-2.42/lib/ld-linux-x86-64.so.2` and its RUNPATH points into
`/nix/store`. `scripts/offload-portable-desktop.sh` builds p2p_chat's
`p2p-desktop` on the worker and makes it portable. Run it from the root of your own
p2p_chat clone (`git clone --local`), never a checkout another agent is editing:

```sh
tasks run --run --who OWNER -- /path/to/task-manager/scripts/offload-portable-desktop.sh --who OWNER [--jobs 6]
tasks run --run --who OWNER -- .../offload-portable-desktop.sh --who OWNER --preflight   # deps only, no compile
.../offload-portable-desktop.sh --check-only BINARY                                        # local checks only
```

The recipe, one remote compile job (`sh -c`, so `-j` is not capped by the worker:
`--jobs` allows 1..8):

1. Static codecs, like release packaging: `scripts/build_libaom.py` and
   `build_libvpx.py` (pinned, cached under `~/build-cache/p2p_chat/` on the worker),
   then `AOM_DIR_<triple>`/`VPX_DIR_<triple>` with `AOM_STATIC_REQUIRED=1
   VPX_STATIC_REQUIRED=1`. On NixOS libaom installs into `lib64/`, while
   `crates/video/build.rs` wants `lib/`, so the recipe points it at a symlink shim in
   `target/portable/aom-prefix`.
2. `cargo build --locked --release -p desktop --target-dir target/distribution`, with
   unchanged RUSTFLAGS. That keeps one build cache.
3. Post-link, on a copy: `patchelf --set-interpreter /lib64/ld-linux-x86-64.so.2
   --remove-rpath target/portable/p2p-desktop`, then a check that there is no
   RPATH/RUNPATH and no dynamic aom/vpx. `--fetch target/portable/p2p-desktop` brings it
   back to `~/build-cache/offload/OWNER/REMOTE_JOB/`.
4. Local checks: the interpreter, no RPATH or `/nix/store` in the dynamic section, `ldd`
   with no "not found" or `/nix/store`, the highest `GLIBC_`/`GLIBCXX_` version
   needed is no newer than the host's, and a smoke start. The smoke start is
   `LD_BIND_NOW=1 p2p-desktop --portable-smoke-not-an-option`, which must exit 1
   with the usage line. It forces every symbol to resolve, and it exits before the
   app makes a data dir, a window, a keyring or a network call.

The worker's glibc is 2.42 and its gcc is 15.3, older than the Arch host's, so
this is the safe direction. A binary built the other way round would not load.

What the desktop crate needs **at build time** on Linux (also probed by
`--preflight`): pkg-config `alsa` (alsa-sys, linked), `meson` + `ninja`
(webrtc-audio-processing "bundled"; abseil is downloaded from its meson wrap and
linked statically when pkg-config has no `absl_base`), libclang for bindgen
(`LIBCLANG_PATH`; NixOS never has it on a search path), `cmake` (opusic-sys, libaom),
`nasm` (rav1d, libaom), `cc`/`c++`, `python3`, plus `patchelf`. The worker module
adds `alsa-lib alsa-lib.dev meson patchelf`. Until that rebuild, and always for
libclang, the recipe gets them from the same channel with a user-level
`nix-build '<nixpkgs>' -A …`, which needs no root.

**At run time**, the binary links `libstdc++.so.6`, `libasound.so.2`,
`libgcc_s.so.1`, `libm`, `libc` and the loader. X11/xcb, xkbcommon, wayland,
EGL/GL and Vulkan are dlopened by soname (x11-dl, tiny-xlib, x11rb `dl-libxcb`,
wayland-sys `dlopen`, xkbcommon-dl, khronos-egl/ash `libloading`). D-Bus (tray,
keyring, notifications, portal) is pure Rust. Don't install X11/xkbcommon `.pc` files on
the worker: x11-dl and tiny-xlib would embed their `/nix/store` libdir as a
fallback dlopen path. That is harmless, because bare sonames are tried first,
but it isn't needed. (The laptop's own dev build also links `libaom.so.3` and
`libabsl_strings.so` from Arch. The portable build does not.)

Validated: host job 233 / worker job 42, p2p_chat 66566cb, `--jobs 6`, 392 s
remote run. That run took meson/alsa/patchelf from the `nix-build` fallback.
`ldd` resolved everything to `/usr/lib`, the binary needs GLIBC ≤ 2.39 and
GLIBCXX ≤ 3.4.30, and the smoke start passed. `strings` still shows some
`/nix/store` paths: the old RUNPATH text stays unreferenced in `.dynstr`, and some
C++ header paths remain in assertions. These are not load paths. Not validated:
opening the GUI, audio, camera, calls, or any other distro than this Arch host.
