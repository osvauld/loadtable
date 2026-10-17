# The `tasks` binary built from this checkout with nixpkgs' rustPlatform.
#
#   ui = false  -> `tasks-worker`: `--no-default-features` (daemon, CLI, peer link, status;
#                  no egui window, no `tasks mcp`). What a compute worker runs.
#   ui = true   -> `tasks`: the full controller build (window + MCP). The window dlopens
#                  GL/X11/Wayland libraries at run time, so they are added to its RUNPATH.
#
# Every dependency comes from Cargo.lock (no git dependencies), fetched as fixed-output
# derivations; the build itself is offline and `--locked`.
{
  lib,
  rustPlatform,
  src,
  # Read at evaluation time, so passed as plain (unfiltered) paths: a lib.fileset source is
  # not readable during `nix flake check --no-build`.
  cargoToml,
  cargoLockFile,
  ui ? false,
  # Only for ui = true: run-time dlopen targets of eframe/winit/glow (GL, xkbcommon,
  # Wayland, X11), added to the binary's RUNPATH. The flake passes them.
  guiLibs ? [ ],
}:

let
  manifest = lib.importTOML cargoToml;
in
rustPlatform.buildRustPackage {
  pname = if ui then "tasks" else "tasks-worker";
  inherit (manifest.package) version;
  inherit src;

  cargoLock.lockFile = cargoLockFile;

  # Only the `tasks` binary of the root package; crates/peer is a library it links.
  cargoBuildFlags = [
    "-p"
    "tinline-task-manager"
    "--bin"
    "tasks"
  ];
  buildNoDefaultFeatures = !ui;

  # No tests in the Nix sandbox (see nix/README.md "Why doCheck = false"): most of the
  # suite starts real daemons, takes flock slots under /tmp/claude-$UID, reads
  # /proc/pressure (PSI gates), binds loopback iroh endpoints, runs git/python3 and
  # expects a writable HOME. A checkFlags skip list would have to name well over a hundred
  # tests and silently rot as tests are added. The suite runs through `tasks run` instead.
  doCheck = false;

  postFixup = lib.optionalString (ui && guiLibs != [ ]) ''
    patchelf --add-rpath ${lib.makeLibraryPath guiLibs} $out/bin/tasks
  '';

  meta = {
    description =
      if ui then
        "Shared workload coordinator (controller build: CLI, daemon, peer link, egui window, MCP)"
      else
        "Shared workload coordinator, headless worker build (daemon, CLI, peer link, status)";
    license = lib.licenses.gpl3Plus;
    mainProgram = "tasks";
    platforms = lib.platforms.linux;
  };
}
