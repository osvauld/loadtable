# Task-manager LAN build worker for the NixOS laptop at 192.168.1.7.
# Import from /etc/nixos/configuration.nix, review, then `sudo nixos-rebuild switch`.
# Reversible: remove the import line and rebuild.
{ config, lib, pkgs, ... }:

{
  # SSH: key-only, reachable only on the Wi-Fi LAN interface.
  services.openssh = {
    enable = true;
    openFirewall = false;
    # 2222 is a temporary handover port while a hand-started sshd holds 22; remove later.
    ports = [ 22 2222 ];
    settings = {
      PasswordAuthentication = false;
      KbdInteractiveAuthentication = false;
      PermitRootLogin = "no";
      AllowUsers = [ "abe" ];
    };
  };
  networking.firewall.interfaces.wlo1.allowedTCPPorts = [ 22 2222 ];

  users.users.abe = {
    openssh.authorizedKeys.keys = [
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILaqjNIsZpuVlerLejMNzEvqw+8dK4Ja7dHuOxV3VY4G abe-main"
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGTTGfZ+SPO+CN8gsFqQVeoxms4fZ++prs+l+FSKF35L claude-nixos-worker"
    ];
    # Keep the worker's user-level tasks daemon running without a login session.
    linger = true;
  };

  # A worker must not sleep: ignore lid/idle and disable suspend/hibernate.
  services.logind.settings.Login = {
    HandleLidSwitch = "ignore";
    HandleLidSwitchExternalPower = "ignore";
    HandleLidSwitchDocked = "ignore";
    IdleAction = "ignore";
  };
  systemd.targets.sleep.enable = false;
  systemd.targets.suspend.enable = false;
  systemd.targets.hibernate.enable = false;
  systemd.targets.hybrid-sleep.enable = false;

  # Dedicated compute node: boot headless so the desktop doesn't hold ~2 GB.
  # KDE stays installed; start it on demand with `sudo systemctl start display-manager`.
  systemd.defaultUnit = lib.mkForce "multi-user.target";

  nix.settings.experimental-features = [ "nix-command" "flakes" ];

  # rustup toolchains (rust-toolchain.toml) and prebuilt tools need a dynamic loader.
  programs.nix-ld.enable = true;
  # Shared libraries that offloaded test binaries (rustup-built, nix-ld loader) load at runtime.
  programs.nix-ld.libraries = with pkgs; [ libaom libvpx openssl ];

  environment.systemPackages = with pkgs; [
    git rsync python3 rustup gcc clang cmake ninja gnumake pkg-config
    openssl openssl.dev perl protobuf
    # p2p_chat video crate: pkg-config aom/vpx, nasm for rav1e asm.
    nasm libaom libaom.dev libvpx libvpx.dev
  ];
  # Expose the .pc files above at /run/current-system/sw/lib/pkgconfig; the tasks
  # daemon unit points PKG_CONFIG_PATH there.
  environment.pathsToLink = [ "/lib/pkgconfig" ];
}
