# Trial feedback: portable p2p_chat desktop build offloaded to the NixOS worker

- Agent/session: worker-desktop (branch feature/worker-desktop)
- Job ID(s): host 230 / worker 39 (preflight, failed as intended: missing meson, patchelf, alsa);
  host 231 / worker 40 (build failed: libaom in lib64/); host 233 / worker 42 (passed)
- Outcome: friction (no tool defect found); one p2p_chat script portability bug
- Redacted command, cwd type, resource class: `tasks run --run --who worker-desktop --
  scripts/offload-portable-desktop.sh --who worker-desktop [--preflight] --jobs 6`, which runs
  `tasks-offload ... --fetch target/portable/p2p-desktop -- sh -c RECIPE`. cwd: own
  `git clone --local` of p2p_chat. Host run slot; remote compile slot (preflight: remote run slot).
- What was exercised: real offload sync, remote build, failure path, fetch, local checks on
  the fetched binary.
- Expected: a fetched binary that loads on the Arch host without /nix/store paths.
- Observed:
  - Worker 42: sync 1.3 s (1261 files, 23.6 MB), run 391.9 s (cargo release build 5m39s at
    -j 6, plus libaom), fetch 27.9 s for 104 MB, total 421 s. `ldd` all `/usr/lib`, GLIBC ≤ 2.39,
    GLIBCXX ≤ 3.4.30, `LD_BIND_NOW=1` smoke (unknown option → usage, exit 1) passed.
  - Worker 40 failed after 355 s: `scripts/build_libaom.py` installs into `lib64/` on NixOS
    (CMake GNUInstallDirs), and `crates/video/build.rs` wants `lib/libaom.a`. The recipe now
    uses a symlink shim prefix. A p2p_chat fix would be `-DCMAKE_INSTALL_LIBDIR=lib` in
    build_libaom.py. Not changed here, because p2p_chat isn't mine.
  - The preflight (worker 39) took 9 s and named the exact missing pieces, so a build would
    not have failed one library at a time (compare lead-vp8enc 227/38: alsa after 114 s).
  - As already reported by lead-vp8enc, the host prints "native admitted command failed: exit
    status: 2" right after "local command exited: exit status: 1/101". The two exit codes
    for one failure are confusing.
  - The installed host `tasks` rejects `tasks list --active` (prints usage), although README
    documents it. The installed release predates the source.
  - The `sh -c` recipe is echoed in full as the argv in the worker job log (about 2 KB). It's
    readable, but noisy.
  - The auto-mode permission classifier denied the brief's `scp` of the module to
    `~/taskworker-nixos/taskworker.nix` ("Remote Shell Writes"). Instead, the module was
    evaluated by piping an expression to `nix-instantiate -` on the worker. That expression
    imports `~/taskworker-nixos/configuration.nix` plus the new module and uses
    `disabledModules` to drop the old copy. No remote file was written.
- Impact: one wasted 6-minute remote build (lib64). Otherwise the offload was much faster
  than a laptop build, and admission was smooth.
- Suggested improvement: (1) a `tasks-offload --preflight`-style hook, or a documented
  convention, for per-project dependency probes (this script does it for p2p-desktop only);
  (2) make the host's final exit status match the remote one.
- Acceptance test for suggestion: on a worker without alsa, the probe names `alsa` in under
  10 s with no compile. A failed remote exit 101 shows exit 101 on the host's final line.
- Workaround requested/authorized: none. The nix-build fallback is user-level and needs no
  root. The module change still needs the user's root rebuild.
