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
tasks run --offload --who OWNER -- /path/to/task-manager/scripts/offload-portable-desktop.sh --who OWNER [--jobs 6]
tasks run --offload --who OWNER -- .../offload-portable-desktop.sh --who OWNER --preflight   # deps only, no compile
.../offload-portable-desktop.sh --check-only BINARY                                        # local checks only
```

`--offload` holds a host offload slot (own lock pool, no CPU PSI gate; see
`scripts/README-offload.md`). With an installed `tasks` that rejects it, use `--run`.

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

## Test phone on the worker

Goal: an Android test phone plugged into the worker's USB, driven by agents
through the worker's own `tasks` ledger: build, install and logcat all run on
the worker, and each device job holds that phone's exclusive device slot
(`p2p-device-SERIAL.lock`) there. The test phone is a **Pixel 3a, serial
`9BWAY1LSDC`**.

Status: designed and unit-tested **without a phone**. On the worker only
`adb version`/`adb devices` (empty list) and the adb server lifetime were
exercised (worker jobs 76, 77, 81, 82). USB access, authorization, install and
logcat on a real phone are unverified until the phone is plugged in there.

### Never-use devices (denylist)

`tasks` refuses the user's personal phone, moto g52 `ZD22257P6G`, on every
host. The entry is built in (`src/device_denylist.rs`) and cannot be removed.
More serials can only be **added**: `TASKS_DENIED_DEVICES` (comma/space
separated, in the environment of the `tasks` process that checks: the unit's
`Environment=` for the daemon/worker, your shell for the client) and
`~/.config/tasks/denied-devices` (`$XDG_CONFIG_HOME/tasks/denied-devices`;
one serial per line, `#` comments). A malformed entry fails closed: every
command validation reports it until it is fixed. Matching ignores case and
treats `:` like `_` (the lock-name form).

Where it is enforced:

- `tasks run|enqueue --device SERIAL` (client submission), daemon approval
  (`approve-command`), worker claim, admission and execution (each re-parses the
  recipe), and the device lock itself (`admission::Kind::Device`);
- any live command whose argv **mentions** a denied serial, whatever its
  resource: `adb -s ZD22257P6G ...`, `-sZD22257P6G`, `ANDROID_SERIAL=...`, or the
  serial inside `sh -c` text;
- `tasks phone-test` (explicit `--device` refused; automatic selection skips
  denied phones) and the internal install step; `tasks devices` shows them as
  `DENIED`;
- `tasks-offload --device SERIAL` and `tasks-offload ... -- CMD` (same list,
  checked on the client before anything is synced or submitted).

The job's own environment never reaches these checks: they run in the client,
daemon and worker before the command exists.

Limit: the denylist sees text, not intent. `adb` without `-s` (or a script that
reads a serial from a file, or Gradle's `installDebug`, which installs on
**every** connected device) can still reach a phone it does not name. So:
always pass `-s SERIAL`, use `--device SERIAL` for anything that talks to a
device, and never run adb or Gradle install tasks under `--run` or the compile
slot. Device jobs set `ANDROID_SERIAL` to their locked device (below), which
covers adb, ddmlib and Gradle install tasks that are not given a serial.

### adb on the worker

- **Binary:** the SDK's `~/Android/Sdk/platform-tools/adb` (runs through
  nix-ld), not nixpkgs' `android-tools`. It is the same release as the
  laptop's (1.0.41, 37.0.1-15733141). Two adb builds on one host fight over the
  server on port 5037 ("adb server version doesn't match this client;
  killing..."): a second binary would kill the server under another device job.
  The daemon's `PATH` has no platform-tools, so commands call
  `"$ANDROID_HOME"/platform-tools/adb` (`ANDROID_HOME` is passed through from the
  unit). Optionally add `%h/Android/Sdk/platform-tools` to the unit's `PATH`
  (template `tasks-daemon.service`) so plain `adb` works; the commands below do
  not rely on it.
- **USB permission:** systemd ≥ 258 tags adb/fastboot interfaces
  `ID_DEBUG_APPLIANCE=android` and grants access only via `uaccess`, i.e. to the
  user of an active **seat** session. This worker is headless (`loginctl`: no
  seat) and the tasks daemon is a lingering user service, so uaccess never
  applies. nixpkgs 26.05 removed `programs.adb` and `android-udev-rules` for
  that reason. `nix/taskworker-host.nix` therefore adds a group `adbusers`, puts
  `abe` in it and gives USB device nodes `GROUP="adbusers", MODE="0660"` for any
  `ID_DEBUG_APPLIANCE=="android"` device plus vendors 18d1 (Google/Pixel) and 22b8
  (Motorola). A group, not `OWNER="abe"`, so another worker user can be added
  later without editing rules.
- **The daemon must have the group.** Supplementary groups are fixed when abe's
  systemd user manager starts (at boot, through lingering), and `tasks-daemon`
  and every job inherit them. After the switch the running daemon still lacks
  `adbusers` until `user@1000.service` restarts (this also stops all user units
  and their running jobs, so do it when `tasks list --active` is empty) or the
  worker reboots. A daemon autostarted from a new ssh login would get the group
  from that login, but the unit owns the daemon normally. Check:
  `grep Groups /proc/$(systemctl --user show -p MainPID --value tasks-daemon)/status`.
- **Authorization:** the phone must trust the worker's adb key
  (`~/.android/adbkey` on the worker, created by the first adb server start).
  On first connection the phone shows "Allow USB debugging?": accept it on the
  phone with "Always allow from this computer". Until then `adb devices` lists
  it as `unauthorized`.

### adb server lifetime under `tasks`

Observed on the worker (job 76): the first adb client forks the server
(`adb -L tcp:5037 fork-server server`), which calls `setsid`, so it leaves the
job's process group and session and is reparented to the user manager. Job
cancellation (SIGTERM/SIGKILL to the owned group) therefore never stops it.
Worse, it inherited the job's admission lock descriptor (fd 9 →
`p2p-run-0.lock`) and kept `run-0` busy with no holder after the job had
passed, until `adb kill-server` (job 77). In a device job it would hold the
device lock the same way, blocking that phone for every later job.

So the release with the denylist also changes `--device` jobs (`src/worker.rs`
`prepare_device_command`):

- before the command starts, the worker runs `adb start-server` itself, with
  every descriptor above stderr close-on-exec (`device::without_inherited_fds`).
  The server holds no lock and outlives the job, shared by later device jobs.
  The job log notes `adb server already running` or the start-server output;
  a failure is a logged warning, and the command then reports its own error.
  Job 81 checked the same mechanism (`close_fds=True`) on the worker: the
  server had its own session and 0 lock descriptors, and all run slots were free
  after the job.
- `ANDROID_SERIAL` is set to the locked device (wireless `host_port` lock names
  are mapped back to `host:port`). `ANDROID_SERIAL` is deliberately NOT in
  `PASSTHROUGH_ENV`: the client's environment is never forwarded, and a
  daemon-wide value would be one host-wide default device that ignores each
  job's lock.

Rules for device jobs: don't run `adb kill-server`, `adb start-server`,
`adb -a`, or `adb reconnect` in a job; a server that a job's own command
restarts holds that job's locks again. To recover from a stray server (`tasks
admission` shows a slot busy with no holder and `ls -l /proc/$(pgrep -f 'adb -L')/fd`
shows a `.lock`), run `adb kill-server` in a `--device` job, and say so in your
feedback. The laptop's long-running adb server was started outside `tasks` and
holds no lock descriptors.

### Setup (user, once)

1. From the laptop, in this checkout: `deploy/worker/enable-test-phone.sh`.
   It stages `nix/taskworker-host.nix` on the worker, shows the diff, then (on
   your confirmation, with your sudo password on the ssh tty) backs up
   `/etc/nixos/taskworker.nix` to `.pre-test-phone`, installs the module, runs
   `sudo nixos-rebuild switch` and `udevadm trigger`, offers to restart the user
   manager when the worker queue is idle, and verifies. The module also adds
   `htop` and `kitty.terminfo` (TERM=xterm-kitty over ssh). Read-only checks:
   `deploy/worker/enable-test-phone.sh --verify`. The module was evaluated in
   the worker's real configuration without switching
   (`deploy/worker/check-test-phone-module.sh`, worker job 85).
2. Deploy the `tasks` release that contains the denylist and device-job
   preparation on **both** machines (worker and laptop), and the matching
   `tasks-offload` on the laptop. Until then `tasks run --device` on the worker
   works but has no `ANDROID_SERIAL`/adb prestart, and only the new
   `tasks-offload` refuses the denied serial.
3. Plug in the Pixel 3a, enable USB debugging, run the check below and accept
   the authorization prompt on the phone.

### Agent commands

Run from your OWN checkout (the worker copy is keyed by host+path, so the same
checkout always maps to the same worker directory), under a local offload slot:

```sh
OFF="tasks-offload --worker abe@192.168.1.7 --ssh-key $HOME/.ssh/id_ed25519_nixos_worker --who OWNER"

# Is the phone there, ready and authorized? (device slot, read-only)
tasks run --offload --who OWNER -- $OFF --device 9BWAY1LSDC -- \
  sh -c '"$ANDROID_HOME"/platform-tools/adb -s 9BWAY1LSDC devices -l'
```

Build and install from a p2p_chat checkout, preferably as two offloads from
`android/`. The build holds the worker's compile slot and no device; the
install holds only the device slot (plus a run slot) for seconds:

```sh
cd /path/to/your/p2p_chat/android
tasks run --offload --who OWNER -- $OFF -- \
  ./gradlew --no-daemon --console=plain :app:assembleDebug -Pabis=arm64-v8a \
  "-Dorg.gradle.jvmargs=-Xmx3g -Dfile.encoding=UTF-8" -Pandroid.useAndroidX=true \
  -Pandroid.nonTransitiveRClass=true -Pkotlin.code.style=official -PrustRelease=true
tasks run --offload --who OWNER -- $OFF --device 9BWAY1LSDC -- \
  sh -c '"$ANDROID_HOME"/platform-tools/adb -s 9BWAY1LSDC install -r app/build/outputs/apk/debug/app-debug.apk'
```

The APK stays in the worker checkout between the two offloads:
`android/app/build` is never pruned by a sync (`offload_agent.py` `PRESERVED`
plus the marker rule beside `build.gradle(.kts)`; synthetic test
`test_apk_built_by_one_offload_survives_the_next_sync_for_a_device_install`).
On the worker, the `app-debug.apk` built by job 45 is still in its checkout
(no later sync of that checkout has happened yet). Don't
edit the checkout between the two steps, or you install an APK of older source.
Gradle on the worker signs with the laptop's debug key (copied to the worker's
`~/.android/debug.keystore`, user-approved), so `install -r` updates an app
installed from the laptop without a signature mismatch. No re-sign step is
needed for APKs built on the worker from now on. APKs built there before the
key was copied still need the old local re-sign with the laptop keystore.

One-shot variant (simpler, but it holds the phone's device slot for the whole
Gradle build, and a `--device` job is run-only, so the build takes a run slot
instead of the worker's compile slot):

```sh
tasks run --offload --who OWNER -- $OFF --device 9BWAY1LSDC -- sh -c \
  './gradlew --no-daemon --console=plain :app:assembleDebug -Pabis=arm64-v8a "-Dorg.gradle.jvmargs=-Xmx3g -Dfile.encoding=UTF-8" -Pandroid.useAndroidX=true -Pandroid.nonTransitiveRClass=true -Pkotlin.code.style=official -PrustRelease=true && "$ANDROID_HOME"/platform-tools/adb -s 9BWAY1LSDC install -r app/build/outputs/apk/debug/app-debug.apk'
```

Logcat comes back as the job's output (it also stays in the worker's bounded
job log):

```sh
# Dump the current buffers and exit (add -t 2000 for the last 2000 lines).
tasks run --offload --who OWNER -- $OFF --device 9BWAY1LSDC -- \
  sh -c '"$ANDROID_HOME"/platform-tools/adb -s 9BWAY1LSDC logcat -d -v time'
# Bounded live capture: 60 s, then exit 0 (timeout's 124 means "time is up").
tasks run --offload --who OWNER -- $OFF --device 9BWAY1LSDC -- \
  sh -c 'timeout 60 "$ANDROID_HOME"/platform-tools/adb -s 9BWAY1LSDC logcat -v time; rc=$?; [ $rc -eq 124 ] && rc=0; exit $rc'
```

The worker's `tasks run` prints the job log when the job ends, so the "live"
capture arrives after its 60 s, not line by line. Don't clear logcat
(`logcat -c`) or wipe/uninstall anything unless the user explicitly asked: a
device slot is not permission to change data on the phone.
