# DRAFT. No flake.lock is committed: pin nixpkgs deliberately on first lock.
# Never applied by task-manager scripts; `nix flake check` only evaluates.
{
  description = "DRAFT task-manager Linux worker NixOS module (not applied anywhere)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = nixpkgs.lib.genAttrs systems;
      # Evaluation-only example; boot.isContainer avoids bootloader/fs requirements.
      example = system: nixpkgs.lib.nixosSystem {
        inherit system;
        modules = [
          self.nixosModules.worker
          {
            boot.isContainer = true;
            system.stateVersion = "26.05";
            services.taskmanager-worker = {
              enable = true;
              lan = { interface = "lan0"; udpPorts = [ 4433 ]; };
            };
          }
        ];
      };
    in
    {
      nixosModules.worker = import ./worker-module.nix;
      nixosModules.default = self.nixosModules.worker;

      checks = forAll (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          cfg = (example system).config;
          ok = cfg.users.users.taskworker.isSystemUser
            && !(builtins.elem "kvm" cfg.users.users.taskworker.extraGroups)
            && !(cfg.systemd.services ? taskmanager-worker)
            && cfg.networking.firewall.allowedUDPPorts == [ ]
            && cfg.networking.firewall.interfaces.lan0.allowedUDPPorts == [ 4433 ]
            && !cfg.programs.nix-ld.enable;
        in
        {
          module-eval = assert ok; pkgs.runCommand "taskworker-module-eval" { } "touch $out";
        });
    };
}
