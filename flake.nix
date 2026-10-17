# task-manager: the `tasks` packages and the NixOS compute-worker module.
#
# At the repository root (not in nix/) because the package is built from the whole Cargo
# workspace (Cargo.toml, Cargo.lock, src/, crates/peer): a flake in nix/ referenced as a path
# would only see nix/. The Nix files themselves live in nix/ (package.nix, module.nix,
# checks.nix, hosts/). Guide: nix/README.md.
{
  description = "task-manager (tasks): workload coordinator packages and NixOS worker module";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAll = lib.genAttrs systems;

      # Only what the Rust build reads: docs/feedback/nix edits don't rebuild the package.
      src = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.unions [
          ./Cargo.toml
          ./Cargo.lock
          ./src
          ./crates/peer/Cargo.toml
          ./crates/peer/src
        ];
      };

      cargoToml = ./Cargo.toml;
      cargoLockFile = ./Cargo.lock;

      packagesFor = pkgs: rec {
        tasks-worker = pkgs.callPackage ./nix/package.nix { inherit src cargoToml cargoLockFile; };
        tasks = pkgs.callPackage ./nix/package.nix {
          inherit src cargoToml cargoLockFile;
          ui = true;
          guiLibs = [
            pkgs.libGL
            pkgs.libxkbcommon
            pkgs.wayland
            # Top-level names in newer nixpkgs, xorg.* before.
            (pkgs.libx11 or pkgs.xorg.libX11)
            (pkgs.libxcursor or pkgs.xorg.libXcursor)
            (pkgs.libxi or pkgs.xorg.libXi)
            (pkgs.libxrandr or pkgs.xorg.libXrandr)
          ];
        };
        default = tasks-worker;
      };
    in
    {
      packages = forAll (system: packagesFor nixpkgs.legacyPackages.${system});

      overlays.default = final: _prev: {
        inherit (packagesFor final) tasks-worker tasks;
      };

      # services.tasks-worker, with `package` defaulting to this checkout's headless build.
      nixosModules.worker =
        { lib, pkgs, ... }:
        {
          imports = [ ./nix/module.nix ];
          services.tasks-worker.package =
            lib.mkDefault
              self.packages.${pkgs.stdenv.hostPlatform.system}.tasks-worker;
        };
      nixosModules.default = self.nixosModules.worker;

      # Evaluation-only checks (see nix/checks.nix). They never build `tasks`: the examples
      # use a stub package, so even a full `nix flake check` compiles no Rust.
      checks = forAll (
        system:
        import ./nix/checks.nix {
          inherit nixpkgs system;
          workerModule = ./nix/module.nix;
          hostExample = ./nix/hosts/lan-worker.nix;
        }
      );
    };
}
