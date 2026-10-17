# Fresh-install worker template: a whole NixOS machine that is a task-manager worker.
# Used by `nixosConfigurations.worker-template` and by every `nix/hosts/<name>/default.nix`
# through the flake (nixos-anywhere installs it; it WIPES the target disk, see nix/README.md).
#
# Per host only `tasksWorkerHost.*` changes: name, address, controller id, disk, ssh keys.
{
  config,
  lib,
  ...
}:

let
  h = config.tasksWorkerHost;
  inherit (lib) mkOption types;
  placeholderKey = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPLACEHOLDERPLACEHOLDERPLACEHOLDERPLACEHOLDER replace-me";
  placeholderId = lib.concatStrings (lib.replicate 64 "0");
in
{
  options.tasksWorkerHost = {
    hostName = mkOption {
      type = types.str;
      default = "worker-template";
      description = "networking.hostName.";
    };
    address = mkOption {
      type = types.str;
      default = "192.168.1.50";
      description = "This worker's fixed LAN IPv4 (DHCP reservation); the peer link binds it.";
    };
    interface = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "enp3s0";
      description = "Open the peer port only on this interface (null: all interfaces).";
    };
    peerPort = mkOption {
      type = types.port;
      default = 7843;
      description = "UDP port of the peer link.";
    };
    controller = {
      name = mkOption {
        type = types.str;
        default = "laptop";
        description = "Controller peer name.";
      };
      endpointId = mkOption {
        type = types.str;
        default = placeholderId;
        description = "The controller's `tasks peer id`. The template's all-zero placeholder is not a usable key: the peer link reports it and stays down.";
      };
    };
    disk = mkOption {
      type = types.str;
      default = "/dev/nvme0n1";
      description = "Whole disk that disko partitions. Everything on it is DESTROYED by the install.";
    };
    sshAuthorizedKeys = mkOption {
      type = types.listOf types.str;
      default = [ placeholderKey ];
      description = "Public keys for the admin user and root (key-only ssh). The template default is a placeholder no one holds.";
    };
    adminUser = mkOption {
      type = types.str;
      default = "admin";
      description = "Login user for ssh (wheel, key-only); the daemon runs as the dedicated tasks-worker user.";
    };
  };

  config = {
    warnings =
      lib.optional (builtins.elem placeholderKey h.sshAuthorizedKeys)
        "tasksWorkerHost.sshAuthorizedKeys still contains the placeholder key: set your real public key before installing."
      ++ lib.optional (h.controller.endpointId == placeholderId)
        "tasksWorkerHost.controller.endpointId is the all-zero placeholder: the peer link will not admit any controller.";

    system.stateVersion = "26.05";
    networking.hostName = h.hostName;

    # --- Disk (disko): one disk, GPT, 1 GiB EFI system partition + ext4 root. ------------
    # The build cache (~tasks-worker/build-cache, offload checkouts, sccache) lives on root.
    disko.devices.disk.main = {
      type = "disk";
      device = h.disk;
      content = {
        type = "gpt";
        partitions = {
          ESP = {
            size = "1G";
            type = "EF00";
            content = {
              type = "filesystem";
              format = "vfat";
              mountpoint = "/boot";
              mountOptions = [ "umask=0077" ];
            };
          };
          root = {
            size = "100%";
            content = {
              type = "filesystem";
              format = "ext4";
              mountpoint = "/";
            };
          };
        };
      };
    };
    boot.loader.systemd-boot.enable = true;
    boot.loader.efi.canTouchEfiVariables = true;
    # tasks' admission needs PSI; harmless where it is already on.
    boot.kernelParams = [ "psi=1" ];

    # --- Hardware: nixos-facter report written by nixos-anywhere, else generated config. --
    # (Set per host by the flake: hardware.facter.reportPath or hardware-configuration.nix.)

    # --- Access: ssh key-only. The firewall opens TCP 22 (ssh is how the worker is
    # installed and administered) and the peer link's UDP port; nothing else. With
    # `interface` set, both only there.
    services.openssh = {
      enable = true;
      openFirewall = h.interface == null;
      settings = {
        PasswordAuthentication = false;
        KbdInteractiveAuthentication = false;
        PermitRootLogin = "prohibit-password";
      };
    };
    users.users.root.openssh.authorizedKeys.keys = h.sshAuthorizedKeys;
    users.users.${h.adminUser} = {
      isNormalUser = true;
      extraGroups = [ "wheel" ];
      openssh.authorizedKeys.keys = h.sshAuthorizedKeys;
    };
    security.sudo.wheelNeedsPassword = false;
    networking.firewall.enable = true;
    networking.firewall.interfaces = lib.mkIf (h.interface != null) {
      ${h.interface}.allowedTCPPorts = [ 22 ];
    };
    nix.settings.experimental-features = [
      "nix-command"
      "flakes"
    ];
    nix.settings.trusted-users = [ "@wheel" ];

    # --- The worker. ---------------------------------------------------------------------
    services.tasks-worker = {
      enable = true;
      peer.bind = "${h.address}:${toString h.peerPort}";
      peer.controllers = [
        {
          inherit (h.controller) name endpointId;
        }
      ];
      firewall.interfaces = lib.optional (h.interface != null) h.interface;
    };
  };
}
