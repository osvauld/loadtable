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
  # iroh QUIC for the lan-peer source/artifact transfer (LAN interface only;
  # admission is the account attestation + allowlist, not the port).
  networking.firewall.interfaces.wlo1.allowedUDPPorts = [ 7842 ];

  users.users.abe = {
    openssh.authorizedKeys.keys = [
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILaqjNIsZpuVlerLejMNzEvqw+8dK4Ja7dHuOxV3VY4G abe-main"
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGTTGfZ+SPO+CN8gsFqQVeoxms4fZ++prs+l+FSKF35L claude-nixos-worker"
    ];
    # Keep the worker's user-level tasks daemon running without a login session.
    linger = true;
    # USB access to the test phone (udev rule below). Takes effect for the tasks
    # daemon only after its user manager restarts (deploy/worker/README.md).
    extraGroups = [ "adbusers" ];
  };

  # Test phone on the worker (deploy/worker/README.md "Test phone on the worker").
  # systemd >= 258 tags adb/fastboot interfaces ID_DEBUG_APPLIANCE=android and grants
  # access only through "uaccess", i.e. to the user of an ACTIVE SEAT session. This
  # worker is headless and the tasks daemon is a lingering user service with no seat,
  # so uaccess never applies to it. nixpkgs 26.05 removed programs.adb and
  # android-udev-rules for that reason; give the node to a group instead.
  # USB device nodes only (DEVTYPE usb_device); 99-local.rules runs after systemd's
  # 70-uaccess.rules, so ID_DEBUG_APPLIANCE is already set.
  users.groups.adbusers = { };
  services.udev.extraRules = ''
    # Any adb/fastboot device (systemd's own classification).
    SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", ENV{ID_DEBUG_APPLIANCE}=="android", GROUP="adbusers", MODE="0660"
    # Google/Pixel (18d1) and Motorola (22b8), also before USB debugging is enabled.
    SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", ATTR{idVendor}=="18d1", GROUP="adbusers", MODE="0660"
    SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", ATTR{idVendor}=="22b8", GROUP="adbusers", MODE="0660"
  '';

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
  programs.nix-ld.libraries = with pkgs; [ libaom libvpx libvmaf openssl fontconfig.lib freetype ];

  environment.systemPackages = with pkgs; [
    git rsync python3 rustup gcc clang cmake ninja gnumake pkg-config
    openssl openssl.dev perl protobuf
    # p2p_chat video crate: pkg-config aom/vpx, nasm for rav1e asm.
    nasm libaom libaom.dev libvpx libvpx.dev libvmaf libvmaf.dev
    # p2p_chat desktop crate: alsa (cpal), meson for webrtc-audio-processing's bundled build,
    # patchelf to make worker-built binaries run on other distros (deploy/worker/README.md).
    alsa-lib alsa-lib.dev meson patchelf
    # osvauld2 (parley): yeslogic-fontconfig-sys links fontconfig; fontconfig.pc needs freetype2.pc.
    fontconfig fontconfig.dev freetype freetype.dev
    # p2p_chat Android APK: Gradle 9.8.1 (wrapper) and AGP 9.4.1 need a JDK; the main host
    # uses OpenJDK 21. cargo-ndk 4.1.2 matches the main host. The Android SDK itself is
    # installed per user with Google's sdkmanager into ~/Android/Sdk (deploy/worker/README.md);
    # unzip unpacks its cmdline-tools archive.
    jdk21 cargo-ndk unzip
    # Operator convenience: htop, and kitty's terminfo so ncurses tools work over ssh
    # from kitty (TERM=xterm-kitty).
    htop kitty.terminfo
    # No android-tools: device jobs use the SDK's platform-tools adb
    # (~/Android/Sdk/platform-tools, run through nix-ld), the same adb release as the
    # main host, so one adb server protocol/version is in use (README).
  ];
  # Stable JAVA_HOME for the tasks daemon unit (Environment=JAVA_HOME=/etc/taskworker/jdk21).
  environment.etc."taskworker/jdk21".source = pkgs.jdk21.home;
  # Expose the .pc files above at /run/current-system/sw/lib/pkgconfig; the tasks
  # daemon unit points PKG_CONFIG_PATH there.
  environment.pathsToLink = [ "/lib/pkgconfig" ];
}
