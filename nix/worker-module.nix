# DRAFT task-manager Linux worker module. Not applied anywhere; unvalidated
# until `nix flake check` runs on a host with Nix. Everything optional is off.
{ config, lib, pkgs, ... }:

let
  cfg = config.services.taskmanager-worker;
  inherit (lib) mkEnableOption mkIf mkMerge mkOption optional optionals types;
  stateDir = "/var/lib/taskworker";
  toolchain = with pkgs; [ rustc cargo cmake ninja pkg-config clang ]
    ++ optionals cfg.androidEmulator.enable (
      [ android-tools ] ++ optional (cfg.androidEmulator.package != null) cfg.androidEmulator.package);
in
{
  options.services.taskmanager-worker = {
    enable = mkEnableOption "the DRAFT task-manager worker user, toolchain and state directory";

    kvm.enable = mkEnableOption "adding taskworker to the kvm group (emulator acceleration)";

    service = {
      enable = mkEnableOption "the placeholder worker systemd service (no binary is packaged yet)";
      execStart = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "/nix/store/...-lan-peer/bin/lan-peer --state /var/lib/taskworker";
        description = "Command for the future lan-peer/worker binary. Required when the service is enabled.";
      };
    };

    lan = {
      interface = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "enp3s0";
        description = "Only interface on which the worker UDP ports are opened. No global port is opened.";
      };
      udpPorts = mkOption {
        type = types.listOf types.port;
        default = [ ];
        description = "UDP ports opened on lan.interface only (e.g. a future iroh endpoint).";
      };
    };

    nixLd.enable = mkEnableOption "programs.nix-ld for prebuilt dynamically linked tools (e.g. Android SDK)";

    androidEmulator = {
      enable = mkEnableOption "Android emulator tooling for taskworker (unvalidated; requires kvm.enable)";
      package = mkOption {
        type = types.nullOr types.package;
        default = null;
        description = ''
          Caller-supplied emulator/SDK composition (e.g. from pkgs.androidenv). This module never
          accepts Android SDK licences; that is a separate explicit decision by the operator.
        '';
      };
    };
  };

  config = mkIf cfg.enable (mkMerge [
    {
      assertions = [
        {
          assertion = !cfg.service.enable || cfg.service.execStart != null;
          message = "services.taskmanager-worker.service.execStart must be set when the service is enabled.";
        }
        {
          assertion = cfg.lan.udpPorts == [ ] || cfg.lan.interface != null;
          message = "services.taskmanager-worker.lan.interface is required to open udpPorts (no global ports).";
        }
        {
          assertion = !cfg.androidEmulator.enable || cfg.kvm.enable;
          message = "services.taskmanager-worker.androidEmulator.enable requires kvm.enable.";
        }
      ];

      users.groups.taskworker = { };
      users.users.taskworker = {
        isSystemUser = true;
        group = "taskworker";
        home = stateDir;
        createHome = false;
        extraGroups = optional cfg.kvm.enable "kvm";
        packages = toolchain;
      };

      systemd.tmpfiles.rules = [ "d ${stateDir} 0750 taskworker taskworker - -" ];
    }

    (mkIf cfg.service.enable {
      systemd.services.taskmanager-worker = {
        description = "DRAFT task-manager LAN worker (placeholder)";
        wantedBy = [ "multi-user.target" ];
        wants = [ "network-online.target" ];
        after = [ "network-online.target" ];
        path = toolchain;
        serviceConfig = {
          User = "taskworker";
          Group = "taskworker";
          ExecStart = cfg.service.execStart;
          StateDirectory = "taskworker";
          StateDirectoryMode = "0750";
          WorkingDirectory = stateDir;
          Restart = "on-failure";
          NoNewPrivileges = true;
          ProtectSystem = "strict";
          ProtectHome = true;
          PrivateTmp = true;
        };
      };
    })

    (mkIf (cfg.lan.interface != null && cfg.lan.udpPorts != [ ]) {
      networking.firewall.interfaces.${cfg.lan.interface}.allowedUDPPorts = cfg.lan.udpPorts;
    })

    (mkIf cfg.nixLd.enable {
      programs.nix-ld.enable = true;
    })
  ]);
}
