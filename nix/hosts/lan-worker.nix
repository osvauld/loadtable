# The LAN worker 192.168.1.7 expressed with services.tasks-worker: the module-based
# replacement for nix/taskworker-host.nix (/etc/nixos/taskworker.nix there today).
# NOT applied. Migration steps: nix/README.md "Migrating the existing worker".
#
# Use from the worker's /etc/nixos/flake.nix (or configuration.nix via flake-compat):
#   imports = [ task-manager.nixosModules.worker ./lan-worker.nix ];
# It keeps everything host-specific that the module does not own: SSH, keys, headless boot.
{ lib, pkgs, ... }:

let
  # The laptop's `tasks peer id` (run there once `tasks peer init --bind 192.168.1.25:7843`
  # exists). Left invalid on purpose: the module's assertion refuses this placeholder, so the
  # file cannot be switched to until a real id is pasted here.
  laptopEndpointId = "REPLACE-with-the-laptop-tasks-peer-id";
in
{
  services.tasks-worker = {
    enable = true;
    # The existing account: keeps ~/.local/state/tinline-task-manager-native (job history),
    # /tmp/claude-1000 (lock inodes), ~/offload/checkouts, ~/.rustup, ~/build-cache/sccache and
    # ~/Android/Sdk. Its old user unit must be disabled first (README).
    user = "abe";
    maxCargoJobs = 6;
    minAvailableMemoryMiB = 1024;
    sccache = {
      enable = true;
      size = "40G";
      # dir = null -> /home/abe/build-cache/sccache, the old unit's SCCACHE_DIR (%h/build-cache/sccache).
    };
    android = {
      enable = true; # ANDROID_HOME=/home/abe/Android/Sdk, JAVA_HOME=/etc/taskworker/jdk21
      # The Pixel 3a test phone (group adbusers + udev). Not on the worker yet: its applied
      # taskworker.nix predates deploy/worker/enable-test-phone.sh; this turns it on.
      adbUsb = true;
    };
    peer = {
      # 7842 stays with `lan-peer serve` (tasks-offload --transport iroh) during the transition.
      bind = "192.168.1.7:7843";
      controllers = [
        {
          name = "laptop";
          endpointId = laptopEndpointId;
          approve = "auto";
        }
      ];
    };
    firewall.interfaces = [ "wlo1" ];
  };

  # --- Host-specific, unchanged from nix/taskworker-host.nix -------------------------------
  services.openssh = {
    enable = true;
    openFirewall = false;
    # The applied file also has 2222 (a temporary handover port); dropped here.
    ports = [ 22 ];
    settings = {
      PasswordAuthentication = false;
      KbdInteractiveAuthentication = false;
      PermitRootLogin = "no";
      AllowUsers = [ "abe" ];
    };
  };
  networking.firewall.interfaces.wlo1.allowedTCPPorts = [ 22 ];
  # lan-peer serve (tasks-offload --transport iroh); drop once tasks-offload is retired.
  networking.firewall.interfaces.wlo1.allowedUDPPorts = [ 7842 ];

  users.users.abe = {
    openssh.authorizedKeys.keys = [
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILaqjNIsZpuVlerLejMNzEvqw+8dK4Ja7dHuOxV3VY4G abe-main"
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGTTGfZ+SPO+CN8gsFqQVeoxms4fZ++prs+l+FSKF35L claude-nixos-worker"
    ];
    # Not needed by the system service any more; kept for other user units (lan-peer).
    linger = true;
  };

  systemd.defaultUnit = lib.mkForce "multi-user.target";
  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];
  environment.systemPackages = with pkgs; [
    htop
    kitty.terminfo
  ];
}
