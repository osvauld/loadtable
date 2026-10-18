# task-manager: the `tasks` packages and the NixOS compute-worker module.
#
# At the repository root (not in nix/) because the package is built from the whole Cargo
# workspace (Cargo.toml, Cargo.lock, src/, crates/peer): a flake in nix/ referenced as a path
# would only see nix/. The Nix files themselves live in nix/ (package.nix, module.nix,
# checks.nix, template/, hosts/, installer.nix). Guide: nix/README.md.
{
  description = "task-manager (tasks): workload coordinator packages and NixOS worker module";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  # Declarative partitioning for fresh installs (nixos-anywhere / the installer ISO).
  inputs.disko = {
    url = "github:nix-community/disko";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      self,
      nixpkgs,
      disko,
    }:
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

      # A whole fresh-install worker machine: the module, the template (disko layout, ssh
      # key-only, firewall) and one host's values + hardware report. x86_64 unless the
      # facter report says otherwise.
      mkWorker =
        {
          hostModule ? { },
          hostDir ? null,
        }:
        let
          has = file: hostDir != null && builtins.pathExists (hostDir + "/${file}");
        in
        lib.nixosSystem {
          modules = [
            { nixpkgs.hostPlatform = lib.mkDefault "x86_64-linux"; }
            self.nixosModules.worker
            disko.nixosModules.disko
            ./nix/template/worker.nix
            hostModule
            (
              if has "facter.json" then
                { hardware.facter.reportPath = hostDir + "/facter.json"; }
              else if has "hardware-configuration.nix" then
                hostDir + "/hardware-configuration.nix"
              else
                {
                  # No report yet (it is written by nixos-anywhere): generic modules.
                  boot.initrd.availableKernelModules = [
                    "nvme"
                    "xhci_pci"
                    "ahci"
                    "usb_storage"
                    "sd_mod"
                  ];
                }
            )
          ];
        };

      # nix/hosts/<name>/default.nix -> nixosConfigurations.<name>
      hostNames = lib.attrNames (
        lib.filterAttrs (
          name: type: type == "directory" && builtins.pathExists (./nix/hosts + "/${name}/default.nix")
        ) (builtins.readDir ./nix/hosts)
      );
    in
    {
      packages = forAll (
        system:
        packagesFor nixpkgs.legacyPackages.${system}
        // {
          # In-person install medium (nix/installer.nix). Never built by the checks.
          installer-iso =
            (lib.nixosSystem {
              specialArgs = {
                flakeSource = self;
                tasksWorker = self.packages.${system}.tasks-worker;
                diskoPackage = disko.packages.${system}.disko;
                # Replace with your public key(s) before building the ISO.
                installerKeys = [ ];
              };
              modules = [
                { nixpkgs.hostPlatform = system; }
                ./nix/installer.nix
              ];
            }).config.system.build.isoImage;
        }
      );

      nixosConfigurations = {
        worker-template = mkWorker { };
      }
      // lib.genAttrs hostNames (
        name:
        mkWorker {
          hostDir = ./nix/hosts + "/${name}";
          hostModule = ./nix/hosts + "/${name}/default.nix";
        }
      );

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
          inherit (self) nixosConfigurations;
        }
      );
    };
}
