# In-person alternative to nixos-anywhere: a minimal NixOS installer ISO with sshd
# (key-only), the headless `tasks` binary, disko and a copy of this flake at
# /etc/task-manager, so a worker can be installed from the console:
#
#   sudo disko --mode destroy,format,mount --flake /etc/task-manager#<host>   # WIPES the disk
#   sudo nixos-install --flake /etc/task-manager#<host>
#
# `packages.<system>.installer-iso` = config.system.build.isoImage of this module.
{
  lib,
  pkgs,
  modulesPath,
  flakeSource,
  tasksWorker,
  diskoPackage,
  installerKeys,
  ...
}:

{
  imports = [ "${modulesPath}/installer/cd-dvd/installation-cd-minimal.nix" ];

  services.openssh = {
    enable = true;
    settings = {
      PasswordAuthentication = lib.mkForce false;
      KbdInteractiveAuthentication = false;
      PermitRootLogin = lib.mkForce "prohibit-password";
    };
  };
  users.users.root.openssh.authorizedKeys.keys = installerKeys;
  # Workers use ext4 (disko layout in template/worker.nix): no ZFS on the image.
  boot.supportedFilesystems.zfs = lib.mkForce false;
  users.users.nixos.openssh.authorizedKeys.keys = installerKeys;

  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];
  environment.systemPackages = [
    tasksWorker
    pkgs.git
    pkgs.htop
    diskoPackage
  ];
  # The flake (with its flake.lock) for `nixos-install --flake /etc/task-manager#<host>`.
  environment.etc."task-manager".source = flakeSource;
}
