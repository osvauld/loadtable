# Nix: the `tasks` packages and the compute-worker module

Status: **evaluated only, never applied, never built with Nix.** See "Checks" for what ran.
No `nix build` of the package and no `nixos-rebuild` has been run from this repository.

The laptop is the **controller** (agents, MCP, the window, placement). Workers run the jobs
the allowlisted controller forwards over the iroh peer link (docs/design/peer-link.md),
under their own admission, Cargo cap, memory reserve and device denylist. The controller
cannot raise a worker's limits: they are the worker unit's environment.

| File | What |
| --- | --- |
| `../flake.nix` | The flake: `packages.<system>.{tasks-worker,tasks,default}`, `overlays.default`, `nixosModules.{worker,default}`, `checks.<system>.module-*` |
| `package.nix` | `rustPlatform.buildRustPackage` of this checkout, `cargoLock.lockFile = Cargo.lock` |
| `module.nix` | `services.tasks-worker` |
| `checks.nix` | Evaluation-only checks of example configurations |
| `hosts/lan-worker.nix` | The worker 192.168.1.7 expressed with the module (migration target; not applied) |
| `taskworker-host.nix` | What is applied there today as `/etc/nixos/taskworker.nix`; superseded by the module once migrated (kept: `deploy/worker/enable-test-phone.sh` installs it) |

The old DRAFT `nix/flake.nix` + `nix/worker-module.nix` (`services.taskmanager-worker`,
placeholder service with `PrivateTmp`) are retired; this replaces them.

**Why the flake is at the repository root.** The package is built from the whole Cargo
workspace (`Cargo.toml`, `Cargo.lock`, `src/`, `crates/peer`). A flake in `nix/` used as a
path (`nix flake check ./nix`) only copies `nix/` into the store and cannot see the source;
`?dir=nix` works but makes every reference awkward. At the root, `.#tasks-worker` and
`inputs.task-manager.url = "git+file:///…/task-manager"` just work. The source is filtered
with `lib.fileset` to the Rust inputs only, so docs/feedback/nix edits don't rebuild it.

## Packages

- `tasks-worker` (= `default`): `buildNoDefaultFeatures = true`, only `--bin tasks`:
  daemon, CLI, peer link, status. No eframe/egui and no `tasks mcp`; `tasks ui` and
  `tasks mcp` print `built without ui|mcp (worker build)` and exit 2.
- `tasks`: the full controller build (default features `ui` + `mcp`); the window's
  dlopened GL/xkbcommon/Wayland/X11 libraries are added to its RUNPATH. Optional; the
  laptop keeps its own pinned `~/.local/bin/tasks`.

**Why `doCheck = false`** rather than a `checkFlags` skip list: most of the suite starts real
`tasks daemon`s, takes flock slots, reads `/proc/pressure/*` (admission refuses to run
without PSI), binds loopback iroh endpoints, spawns `git`/`python3`/`sh` and needs a
writable HOME. None of that is meaningful in the build sandbox, and a skip list naming most of
the ~340 tests would silently rot as tests are added. The suite runs where it means
something: `tasks run --who … -- cargo test --offline --locked -j 2`, for both feature sets.

## `services.tasks-worker` options

| Option | Default | Effect |
| --- | --- | --- |
| `enable` | false | System service `tasks-worker.service` running `${package}/bin/tasks daemon` |
| `package` | the flake's `tasks-worker` | The headless build |
| `user` | `"tasks-worker"` | Dedicated system user (home = `stateDir`). Any other name must exist in `users.users`, e.g. `"abe"` as in today's deployment |
| `stateDir` | `/var/lib/tasks-worker` | Module state, 0700 `user` (tmpfiles): the peer key by default. The ledger stays at the user's `~/.local/state/tinline-task-manager-native` (`TASK_MANAGER_STATE_DIR` is never set) |
| `maxCargoJobs` | 6 | `TASKS_MAX_CARGO_JOBS` (1–64) |
| `minAvailableMemoryMiB` | 1024 | `TASKS_MIN_AVAILABLE_MEMORY_MIB` (1024–1048576) |
| `sccache.enable` | true | `RUSTC_WRAPPER=<sccache>/bin/sccache` |
| `sccache.size` | `"40G"` | `SCCACHE_CACHE_SIZE` |
| `sccache.dir` | null = `<home>/build-cache/sccache` | `SCCACHE_DIR` (the existing worker's value; also tasks' cache-policy path) |
| `sccache.package` | `pkgs.sccache` | |
| `peer.enable` | true | Render the peer config. Off: `TASKS_PEER_CONFIG` names a missing file, nothing binds |
| `peer.bind` | null (required when enabled) | `"IP:PORT"` / `"[v6]:PORT"`; 0.0.0.0 / :: refused |
| `peer.controllers` | `[ ]` (≥1 required) | `[{ name, endpointId, addr ? null, approve ? "auto" }]`, rendered as `role = "host"`: the allowlist is the controllers, nothing else |
| `peer.secretKeyFile` | `${stateDir}/peer.key` | Created by `ExecStartPre` (as `user`, umask 077, never overwritten) if missing; never in the store (assertion) |
| `peer.checkouts` | null = `~/offload/checkouts` | Where forwarded checkouts land (shared with tasks-offload's warm targets) |
| `peer.settings` | `{ }` | Extra peer.toml keys (`reattach_timeout_s`, `status_interval_s`, `store`, …) |
| `peer.waitForAddressSec` | 60 | Pre-start wait for the bind IP (the daemon binds once; Wi-Fi at boot) |
| `firewall.openPeerPort` | true | Open the peer UDP port and nothing else |
| `firewall.interfaces` | `[ ]` = all | Open it only on these interfaces |
| `toolchains` | taskworker-host.nix's list | rsync python3 rustup gcc clang cmake ninja gnumake pkg-config openssl(.dev) perl protobuf nasm libaom/libvpx/libvmaf(.dev) alsa-lib(.dev) meson patchelf jdk21 cargo-ndk unzip; on the daemon PATH and in systemPackages |
| `android.enable` | false | `ANDROID_HOME` (`android.sdkRoot`, null = `<home>/Android/Sdk`) and `JAVA_HOME=/etc/taskworker/jdk21` (a link to `android.jdk`, default jdk21). The SDK is installed per user with sdkmanager; **the module never accepts Android SDK licences** |
| `android.adbUsb` | false | Group `adbusers` for `user` + udev rules (headless: no seat, no uaccess) |
| `bindgen.enable` / `bindgen.llvmPackages` | true / `pkgs.llvmPackages` | `LIBCLANG_PATH=<libclang.lib>/lib`, `BINDGEN_EXTRA_CLANG_ARGS="-isystem <libclang.lib>/lib/clang/<major>/include -isystem <glibc.dev>/include"` (bindgen loads libclang itself; worker job 456 failed with "Unable to find libclang"). Forwarded to live commands by tasks (this branch) |
| `nixLd.enable` / `nixLd.libraries` | true / libaom libvpx libvmaf openssl | `programs.nix-ld` for rustup toolchains, the SDK's adb and offloaded test binaries |
| `keepAwake` | true | logind ignores lid/idle; sleep/suspend/hibernate targets off |
| `excludeLockDirFromTmpCleaning` | true | tmpfiles `x /tmp/claude-*` (stable flock inodes) |
| `extraEnvironment` | `{ }` | More daemon environment (e.g. `TASKS_RUN_CPU_SOME_MAX_PCT`); wins over the module's values |
| `restartOnSwitch` | false | `restartIfChanged`: a rebuild does not restart a (possibly busy) daemon |

The unit: `Type=simple`, `User=`/`Group=` from `user`, `WorkingDirectory=<home>`,
`UMask=0077`, `KillMode=process` (stopping is not cancellation; jobs keep their process
groups and leases), `Restart=on-failure`, `RestartSec=30`, `RestartPreventExitStatus=2`
("another daemon owns this state directory" and other configuration errors are not retried in
a loop), wants/after `network-online.target`. **No `PrivateTmp`, `ProtectHome`,
`ProtectSystem`** or any other mount namespace: the admission lock directory
`/tmp/claude-$UID`, `taskd.sock` and the checkouts must be the same files SSH clients,
`tasks admission` and legacy helpers see (deploy/worker/README.md "/tmp cleaning and
PrivateTmp"). Environment: `PATH` (`~/.cargo/bin`, `~/.local/bin`, package + git + coreutils
+ toolchains, the per-user and system profiles, `/run/wrappers/bin`), `TASKS_MAX_CARGO_JOBS`,
`TASKS_MIN_AVAILABLE_MEMORY_MIB`, `TASKS_PEER_CONFIG=/etc/tasks-worker/peer.toml`,
`PKG_CONFIG_PATH=/run/current-system/sw/lib/pkgconfig`, the sccache, bindgen and Android
variables, then `extraEnvironment`.

What reaches a job: tasks clears the job environment and keeps only an allowlist from the
daemon (`PASSTHROUGH_ENV` in `src/worker.rs`: HOME, PATH, PKG_CONFIG_PATH, NIX_LD*,
ANDROID_HOME, JAVA_HOME, …) plus, for live commands, `RUSTC_WRAPPER`, `SCCACHE_DIR`,
`SCCACHE_CACHE_SIZE`, `GRADLE_USER_HOME`, `LIBCLANG_PATH` and `BINDGEN_EXTRA_CLANG_ARGS`
(the last two added on this branch). `TASKS_*` configure tasks itself. Anything else in
`extraEnvironment` stays in the daemon and does **not** reach jobs unless added there. A system service gets the user's supplementary groups (adbusers)
from `/etc/group` at every start, so no user-manager restart is needed any more.

Rendered `/etc/tasks-worker/peer.toml` (world-readable: endpoint ids are public and the key is
not in it):

```toml
bind = "192.168.1.7:7843"
secret_key = "/var/lib/tasks-worker/peer.key"

[[peer]]
approve = "auto"
endpoint_id = "<the laptop's tasks peer id>"
name = "laptop"
role = "host"
```

`role = "host"`: the controller forwards jobs here (status, jobs, source sync, artifact
fetch); this worker never forwards to it. Unknown endpoints and role mismatches are refused
before a challenge. `approve = "auto"` approves and starts inbound jobs (what the ssh
`tasks run` of tasks-offload does today); `"manual"` leaves them for `tasks approve-command`
on the worker. Admission applies either way.

The worker's endpoint id, for the controller's config: `sudo -u <user> tasks-worker-peer-id`
(installed by the module; `tasks peer id` against the module's config).

## Add a worker in 6 steps

Prerequisites: NixOS 26.05 with flakes, PSI (`/proc/pressure/cpu` exists), a fixed LAN
address (DHCP reservation), and a peer-capable `tasks` on the controller.

1. **Controller, once:** `tasks peer init --bind <laptop LAN IP>:7843`; note its
   `tasks peer id`.
2. **Worker flake:** in `/etc/nixos/flake.nix` add
   `inputs.task-manager.url = "git+file:///path/to/task-manager";` (a clone of this
   repository) and `task-manager.nixosModules.worker` to the system's `modules`.
3. **Worker config:**
   ```nix
   services.tasks-worker = {
     enable = true;
     # user = "abe";                     # optional: an existing account instead of tasks-worker
     peer.bind = "192.168.1.8:7843";     # this worker's LAN address
     peer.controllers = [ { name = "laptop"; endpointId = "<id from step 1>"; } ];
     firewall.interfaces = [ "wlo1" ];   # optional: only on the LAN interface
     # android.enable = true;            # if jobs build APKs (SDK per user, via sdkmanager)
   };
   ```
4. **Build, then switch (user step):** `sudo nixos-rebuild build` (compiles `tasks`; see
   "Full build" for the cost), then `sudo nixos-rebuild switch`. With the dedicated user,
   install its Rust toolchain once (`sudo -iu tasks-worker rustup default stable`, online)
   or set `toolchains` to nixpkgs' `rustc`/`cargo`.
5. **Endpoint id:** `sudo -u tasks-worker tasks-worker-peer-id` (the key was generated at the
   first start). On the controller, add to `~/.config/tasks/peer.toml`
   ```toml
   [[peer]]
   name = "worker2"
   endpoint_id = "<id from step 5>"
   addr = "192.168.1.8:7843"
   role = "worker"
   ```
   and restart the controller's daemon when it is idle (`tasks list --active` empty).
6. **Check:** `tasks peer list` on the controller shows the worker online;
   `tasks place -- cargo build --offline --locked` names it; `tasks run --peer worker2 -- true`.

## Migrating the existing worker (192.168.1.7): not performed

Today: the user unit `~/.config/systemd/user/tasks-daemon.service` (user abe with linger;
`TASKS_MAX_CARGO_JOBS=6`, `RUSTC_WRAPPER` sccache, `SCCACHE_DIR`, `SCCACHE_CACHE_SIZE=40G`,
`TASKS_MIN_AVAILABLE_MEMORY_MIB=1024`, `ANDROID_HOME`, `JAVA_HOME`) plus
`/etc/nixos/taskworker.nix` (= `nix/taskworker-host.nix`). Target: `nix/hosts/lan-worker.nix`,
`user = "abe"`, which keeps every path the old unit used:

| Preserved | How |
| --- | --- |
| Job history (`~/.local/state/tinline-task-manager-native`: SQLite ledger, logs, links) | Same user, HOME and XDG defaults; `TASK_MANAGER_STATE_DIR` stays unset; this branch changes no schema |
| Stable flock inodes (`/tmp/claude-1000/*.lock`) | Same UID and default lock dir; no `PrivateTmp`; `x /tmp/claude-*` keeps them out of age cleaning. Never delete or move lock files |
| sccache (`SCCACHE_DIR=%h/build-cache/sccache`, 40G) | `sccache.dir = null` resolves to the same `/home/abe/build-cache/sccache`; `RUSTC_WRAPPER` moves from `/run/current-system/sw/bin/sccache` to the store path of the same package |
| Checkouts and warm targets (`~/offload/checkouts`, `~/build-cache`) | Same user; `peer.checkouts = null` |
| Android SDK, debug keystore, adb key, test-phone USB access | Same HOME; `android.enable`, `android.adbUsb` |

Interim fix already on the worker (user-applied, outside this repo): the old user unit reads
`/etc/taskworker/bindgen.env` through `EnvironmentFile=` for `LIBCLANG_PATH` and
`BINDGEN_EXTRA_CLANG_ARGS`. With the module, `bindgen.enable` (default true) sets both in
`tasks-worker.service`, so drop that `EnvironmentFile=` line and the `/etc/taskworker`
file with the old unit. Note: the worker's installed `tasks` forwards these two variables to
jobs only once it is a release containing this branch's `src/worker.rs` change (earlier
releases clear them; check with a `--run` job that echoes `$LIBCLANG_PATH`). Until then a job
can pass them itself: `env LIBCLANG_PATH=… BINDGEN_EXTRA_CLANG_ARGS=… cargo …`.

Steps (user, in an idle window: `tasks list --active` empty on the worker):

1. Paste the laptop's `tasks peer id` into `nix/hosts/lan-worker.nix` (the committed
   placeholder is refused by an assertion) and compare it with
   `systemctl --user cat tasks-daemon`. The worker's `/etc/nixos` is not a flake today
   (configuration.nix, no `<nixpkgs>` on NIX_PATH). Either convert it to a flake (input
   `task-manager`, module `task-manager.nixosModules.worker`), or, staying non-flake, import
   the files directly from a clone of this repo and build the package with the host's pkgs:
   ```nix
   imports = [ /home/abe/task-manager/nix/module.nix ./lan-worker.nix ];
   services.tasks-worker.package = pkgs.callPackage /home/abe/task-manager/nix/package.nix {
     src = /home/abe/task-manager;   # copies the checkout; prefer a clean clone
   };
   ```
   Drop the `./taskworker.nix` import (keep the file for rollback). The new host file drops
   the temporary ssh port 2222 and turns on the test-phone USB rules
   (`android.adbUsb`), which the applied taskworker.nix does not have yet.
2. `sudo nixos-rebuild build` (compiles `tasks`, switches nothing).
3. Stop and disable the old user unit (stops only the daemon; running jobs continue):
   `systemctl --user disable --now tasks-daemon.service`. Only one daemon can own the state
   directory; the new one exits 2 (not retried) while the old one runs.
4. `sudo nixos-rebuild switch`. It starts `tasks-worker.service`; the first start creates
   `/var/lib/tasks-worker/peer.key`. Check `systemctl status tasks-worker`, `tasks admission`
   (`thresholds.source: worker-record`, `max_cargo_jobs: 6`, `lock_dir /tmp/claude-1000`)
   and `tasks list --limit 3` (the old jobs are still there).
5. Point ssh clients at the same binary: keep `~/.local/bin/tasks` as
   `~/.local/bin/tasks.pre-module`, then `ln -sfn /run/current-system/sw/bin/tasks
   ~/.local/bin/tasks` (tasks-offload calls `~/.local/bin/tasks`, and a client autostarts
   the daemon from its own path).
6. `sudo -u abe tasks-worker-peer-id` → the laptop's `[[peer]] name = "worker"`,
   `addr = "192.168.1.7:7843"`, `role = "worker"`; restart the laptop daemon when idle.

Rollback: re-import `./taskworker.nix`, `nixos-rebuild switch`,
`systemctl --user enable --now tasks-daemon.service`. Ledger, locks and caches never moved,
so history survives both directions.

## Checks

```sh
nix flake check --no-build .     # evaluation only
```

`checks.<system>.module-*` evaluate NixOS container configurations (`boot.isContainer`) with
a stub `tasks` and throw the list of failed expectations:

- `module-dedicated-user`: defaults — system user, service, ExecStart, restart policy, no
  PrivateTmp/ProtectHome in the generated unit text, env, PATH, peer config, UDP port on all
  interfaces only, tmpfiles, the endpoint-id helper, no failed assertions;
- `module-existing-user`: `hosts/lan-worker.nix` with a valid id — user abe, env equal to the
  old unit's, UDP 7843 only on wlo1, manual approval rendered, adb group, nix-ld, no sleep;
- `module-refusals`: placeholder id, 0.0.0.0, missing port, no controllers, key in the
  store, undeclared user;
- `module-peer-disabled`: nothing rendered or opened.

They never build `tasks`, so even a full `nix flake check` builds only empty marker files.

## Full build (not run; needs the user's go-ahead)

```sh
nix build .#tasks-worker -L --max-jobs 1 --cores 4
```

It builds inside nix-daemon, **outside `tasks` admission, cancellation and the job ledger**.
Cost: see the estimate in the feedback file of this change. `nixos-rebuild` of a config that
enables the module runs the same build.

## Limitations

- Evaluated only: the package was never built by Nix and the module never switched.
- The pre-start wait covers the boot race, but if the bind address is missing when the
  daemon starts, the peer link stays disabled until the next (idle) daemon restart.
- `tasks-worker-peer-id` must run as the service user (the key is 0600).
- The dedicated user needs its own rustup toolchain and Android SDK (online, per user); the
  module installs neither and never accepts SDK licences.
- Controllers are rendered as `role = "host"` only; observer peers are not offered.
- Nothing here retires `tasks-offload`/`lan-peer`; their ssh and UDP 7842 stay in the host
  file until docs/design/peer-link.md's remaining items are done.
