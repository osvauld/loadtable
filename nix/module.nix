# services.tasks-worker: a task-manager compute worker on NixOS.
#
# The laptop is the controller (agents, MCP, the window, placement); a worker runs jobs that
# the allowlisted controller forwards over the iroh peer link, under the worker's OWN admission
# (Cargo cap, memory reserve, PSI gates, device denylist). Nothing the controller sends can
# raise these limits: they are this unit's environment.
#
# Imported through the flake (`nixosModules.worker`), which sets `package` to this checkout's
# headless build. Docs: nix/README.md; design: docs/design/peer-link.md.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.tasks-worker;
  inherit (lib)
    concatStringsSep
    literalExpression
    mkDefault
    mkEnableOption
    mkIf
    mkMerge
    mkOption
    optional
    optionalAttrs
    optionals
    types
    ;

  dedicatedUser = cfg.user == "tasks-worker";
  userCfg = config.users.users.${cfg.user} or { };
  home = if dedicatedUser then cfg.stateDir else (userCfg.home or "/home/${cfg.user}");
  group =
    if dedicatedUser then
      "tasks-worker"
    else if (userCfg.group or "") != "" then
      userCfg.group
    else
      "users";

  # "IP:PORT" or "[v6]:PORT" -> PORT (validated by an assertion below).
  bindPort =
    let
      m = builtins.match "(.*):([0-9]+)" cfg.peer.bind;
    in
    if cfg.peer.bind == null || m == null then null else lib.toInt (builtins.elemAt m 1);
  bindHost =
    let
      m = if cfg.peer.bind == null then null else builtins.match "\\[?([^]]*)]?:[0-9]+" cfg.peer.bind;
    in
    if m == null then null else builtins.head m;

  hex64 = s: builtins.match "[0-9a-fA-F]{64}" s != null;
  validName = s: builtins.match "[A-Za-z0-9._-]{1,64}" s != null;

  # The worker's peer config: controllers only, each as a `host` peer (it forwards jobs to
  # this machine; this machine never forwards to it). No worker/both/observer entries.
  peerSettings = {
    secret_key = cfg.peer.secretKeyFile;
    inherit (cfg.peer) bind;
    peer = map (
      c:
      {
        inherit (c) name approve;
        endpoint_id = c.endpointId;
        role = "host";
      }
      // optionalAttrs (c.addr != null) { inherit (c) addr; }
    ) cfg.peer.controllers;
  }
  // optionalAttrs (cfg.peer.checkouts != null) { checkouts = cfg.peer.checkouts; }
  // cfg.peer.settings;
  peerConfigFile = (pkgs.formats.toml { }).generate "tasks-worker-peer.toml" peerSettings;
  peerConfigPath = "/etc/tasks-worker/peer.toml";
  # A path that never exists: the daemon then reports "not configured" and binds nothing,
  # instead of falling back to the user's own ~/.config/tasks/peer.toml.
  peerDisabledPath = "/var/empty/tasks-worker-peer-disabled.toml";

  # The existing worker's unit and tasks' own cache policy (src/cache.rs) use this path.
  sccacheDir = if cfg.sccache.dir != null then cfg.sccache.dir else "${home}/build-cache/sccache";
  androidHome =
    if cfg.android.sdkRoot != null then cfg.android.sdkRoot else "${home}/Android/Sdk";
  javaHome = "/etc/taskworker/jdk21";
  libclang = cfg.bindgen.llvmPackages.libclang;
  sccacheServer = cfg.enable && cfg.sccache.enable && cfg.sccache.server;

  # What this declaration promises, for the daemon's health check (src/health/node.rs probes
  # each name; a declared one that does not resolve raises declared_capability:NODE:NAME).
  # Names: libclang, jdk, android-sdk, cargo-ndk, else a command looked up on the daemon's
  # PATH (which includes /run/current-system/sw/bin, where operatorTools land).
  toolName = p: p.meta.mainProgram or (lib.getName p);
  hasToolchain = name: lib.any (p: lib.getName p == name) cfg.toolchains;
  defaultDeclaredCapabilities = lib.unique (
    optional cfg.bindgen.enable "libclang"
    ++ optional (cfg.android.enable || hasToolchain "openjdk") "jdk"
    # The SDK itself is installed per user with sdkmanager; declaring it makes a missing SDK
    # a named alert instead of a job that fails later.
    ++ optional cfg.android.enable "android-sdk"
    ++ optional (hasToolchain "cargo-ndk") "cargo-ndk"
    ++ map toolName cfg.operatorTools
  );

  # What every live command needs regardless of the toolchain choice: the cache-link step
  # runs `git rev-parse --show-toplevel`.
  basePackages = with pkgs; [
    cfg.package
    git
    coreutils
    findutils
    gnugrep
    gnused
    gawk
    bash
    procps
  ];
  servicePackages =
    basePackages
    ++ cfg.toolchains
    ++ optional cfg.sccache.enable cfg.sccache.package
    ++ optionals cfg.android.enable [ cfg.android.jdk ];

  # Same order as deploy/worker/tasks-daemon.service: the user's rustup/cargo bins first.
  servicePath = concatStringsSep ":" [
    "${home}/.cargo/bin"
    "${home}/.local/bin"
    (lib.makeBinPath servicePackages)
    "/etc/profiles/per-user/${cfg.user}/bin"
    "/run/current-system/sw/bin"
    "/run/wrappers/bin"
  ];

  serviceEnvironment = {
    PATH = servicePath;
    TASKS_MAX_CARGO_JOBS = toString cfg.maxCargoJobs;
    TASKS_MIN_AVAILABLE_MEMORY_MIB = toString cfg.minAvailableMemoryMiB;
    TASKS_PEER_CONFIG = if cfg.peer.enable then peerConfigPath else peerDisabledPath;
    PKG_CONFIG_PATH = "/run/current-system/sw/lib/pkgconfig";
  }
  // optionalAttrs (cfg.declaredCapabilities != [ ]) {
    TASKS_DECLARED_CAPABILITIES = concatStringsSep "," cfg.declaredCapabilities;
  }
  // optionalAttrs cfg.sccache.enable {
    RUSTC_WRAPPER = lib.getExe' cfg.sccache.package "sccache";
    SCCACHE_DIR = sccacheDir;
    SCCACHE_CACHE_SIZE = cfg.sccache.size;
  }
  // optionalAttrs cfg.android.enable {
    ANDROID_HOME = androidHome;
    JAVA_HOME = javaHome;
  }
  // optionalAttrs cfg.bindgen.enable {
    # bindgen dlopens libclang itself; the wrapped `clang` on PATH is not enough, and
    # NixOS has no libclang on any search path (job 456: "Unable to find libclang").
    LIBCLANG_PATH = "${libclang.lib}/lib";
    BINDGEN_EXTRA_CLANG_ARGS = concatStringsSep " " [
      "-isystem ${libclang.lib}/lib/clang/${lib.versions.major libclang.version}/include"
      # -idirafter, not -isystem: libstdc++'s <cstdlib> does `#include_next <stdlib.h>`,
      # which only finds glibc when glibc comes after the C++ headers (worker job 263).
      "-idirafter ${pkgs.glibc.dev}/include"
    ];
  }
  // cfg.extraEnvironment;

  # Runs as the service user before the daemon: create the peer key if missing (0600, never
  # in the store, never overwritten), then wait for the bind address (the daemon binds once;
  # a missing address at boot would leave the peer link disabled until the next restart).
  preStart = pkgs.writeShellScript "tasks-worker-prestart" ''
    set -eu
    PATH=${
      lib.makeBinPath (
        with pkgs;
        [
          coreutils
          iproute2
          gnugrep
        ]
      )
    }
    ${lib.optionalString cfg.peer.enable ''
      key=${lib.escapeShellArg cfg.peer.secretKeyFile}
      if [ ! -e "$key" ] && [ ! -L "$key" ]; then
        umask 077
        mkdir -p "$(dirname "$key")"
        tmp=$(mktemp "$key.new.XXXXXX")
        head -c 32 /dev/urandom | od -An -v -tx1 | tr -d ' \n' > "$tmp"
        echo >> "$tmp"
        if [ "$(tr -d '\n' < "$tmp" | wc -c)" != 64 ]; then
          rm -f "$tmp"; echo "tasks-worker: could not generate $key" >&2; exit 1
        fi
        # -n: never replace a key that appeared meanwhile.
        mv -n "$tmp" "$key"; rm -f "$tmp"
        echo "tasks-worker: generated peer key $key; endpoint id: tasks-worker-peer-id" >&2
      fi
      waited=0
      while ! ip -o addr show | grep -qF " ${toString bindHost}/"; do
        if [ "$waited" -ge ${toString cfg.peer.waitForAddressSec} ]; then
          echo "tasks-worker: ${toString bindHost} is not assigned after ${toString cfg.peer.waitForAddressSec}s; starting anyway (peer link will report the bind error)" >&2
          break
        fi
        sleep 1; waited=$((waited + 1))
      done
    ''}
  '';

  # `tasks peer id` against the module's config: prints this worker's endpoint id for the
  # controller's [[peer]] entry. Run as the service user (the key is 0600):
  #   sudo -u <user> tasks-worker-peer-id
  peerIdScript = pkgs.writeShellScriptBin "tasks-worker-peer-id" ''
    exec env TASKS_PEER_CONFIG=${peerConfigPath} ${lib.getExe cfg.package} peer id "$@"
  '';

  controllerType = types.submodule {
    options = {
      name = mkOption {
        type = types.str;
        example = "laptop";
        description = "Peer name ([A-Za-z0-9._-]{1,64}); jobs show as OWNER@sender-host.";
      };
      endpointId = mkOption {
        type = types.str;
        example = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        description = "The controller's `tasks peer id` (64 hex). The TLS-proven id is the only admission gate.";
      };
      addr = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "192.168.1.25:7842";
        description = "Where the controller listens. Not needed by a worker (it never dials a host peer); kept for reference.";
      };
      approve = mkOption {
        type = types.enum [
          "auto"
          "manual"
        ];
        default = "auto";
        description = ''
          Inbound jobs from this controller: `auto` approves and starts them (the equivalent of
          the ssh `tasks run` of tasks-offload); `manual` leaves them queued for
          `tasks approve-command` on the worker. Admission still applies either way.
        '';
      };
    };
  };
in
{
  options.services.tasks-worker = {
    enable = mkEnableOption "the task-manager compute worker (`tasks daemon` as a system service)";

    package = mkOption {
      type = types.package;
      description = "The headless `tasks` build. The flake's module sets this checkout's `tasks-worker`.";
    };

    user = mkOption {
      type = types.str;
      default = "tasks-worker";
      example = "abe";
      description = ''
        Account the daemon and every job run as. The default creates a dedicated system user
        `tasks-worker` (home = stateDir). An existing account (e.g. "abe" on the current worker)
        keeps its ledger at ~/.local/state/tinline-task-manager-native, its lock directory
        /tmp/claude-$UID, its checkouts, rustup and sccache; that account must already exist.
      '';
    };

    stateDir = mkOption {
      type = types.path;
      default = "/var/lib/tasks-worker";
      description = ''
        Module-owned state (0700, owned by `user`): the peer key by default. For the dedicated
        user it is also HOME, so the ledger lands in stateDir/.local/state/... The ledger is
        never moved by this option (TASK_MANAGER_STATE_DIR stays unset, as SSH clients expect).
      '';
    };

    maxCargoJobs = mkOption {
      type = types.ints.between 1 64;
      default = 6;
      description = "TASKS_MAX_CARGO_JOBS: the largest Cargo -j a job may request here (placement uses it as this machine's cap).";
    };

    minAvailableMemoryMiB = mkOption {
      type = types.ints.between 1024 1048576;
      default = 1024;
      description = "TASKS_MIN_AVAILABLE_MEMORY_MIB: admission's MemAvailable reserve.";
    };

    sccache = {
      enable = mkOption {
        type = types.bool;
        default = true;
        description = "Set RUSTC_WRAPPER to sccache for every job.";
      };
      package = mkOption {
        type = types.package;
        default = pkgs.sccache;
        defaultText = literalExpression "pkgs.sccache";
        description = "sccache package.";
      };
      size = mkOption {
        type = types.str;
        default = "40G";
        description = "SCCACHE_CACHE_SIZE.";
      };
      dir = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "SCCACHE_DIR; null = <user home>/build-cache/sccache (the existing worker's value and tasks' cache-policy path).";
      };
      server = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Run the sccache server as its own service, `tasks-worker-sccache.service` (as `user`,
          same SCCACHE_DIR/SCCACHE_CACHE_SIZE, SCCACHE_IDLE_TIMEOUT=0, restarted if it exits),
          ordered before the daemon. Without it the first job's rustc spawns the server inside
          that job, and the server inherits the job's slot-lock lease (`p2p-build.lock`),
          holding the compile slot after the job ends (worker jobs 154/155). It is a system
          unit running as `user` rather than a systemd --user unit, so it also works for the
          dedicated system user, which has no user manager.
        '';
      };
    };

    peer = {
      enable = mkOption {
        type = types.bool;
        default = true;
        description = "Run the iroh peer link (needs bind and at least one controller). Off: the daemon binds nothing.";
      };
      bind = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "192.168.1.7:7842";
        description = "Explicit LAN address IP:PORT ([v6]:PORT) to bind; 0.0.0.0/:: are refused by tasks.";
      };
      controllers = mkOption {
        type = types.listOf controllerType;
        default = [ ];
        description = "The allowlist: controllers that may forward jobs here (role host). Nobody else is admitted.";
      };
      secretKeyFile = mkOption {
        type = types.str;
        default = "${cfg.stateDir}/peer.key";
        defaultText = literalExpression ''"''${stateDir}/peer.key"'';
        description = ''
          The worker's iroh secret key (64 hex, 0600, owned by `user`). Created on first start
          if missing; never in the Nix store, never overwritten. Its public half is the endpoint
          id (`tasks-worker-peer-id`).
        '';
      };
      checkouts = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Where forwarded checkouts land; null = tasks' default ~/offload/checkouts (shared with tasks-offload's warm targets).";
      };
      settings = mkOption {
        type = types.attrsOf (
          types.oneOf [
            types.int
            types.float
            types.str
          ]
        );
        default = { };
        example = {
          reattach_timeout_s = 3600;
          status_interval_s = 2;
        };
        description = "Extra top-level peer.toml keys (store, reattach_timeout_s, connect_timeout_s, status_interval_s, heartbeat_s, link_stall_s).";
      };
      rendered = mkOption {
        type = types.attrs;
        readOnly = true;
        internal = true;
        default = peerSettings;
        description = "The peer.toml content (as an attrset) written to /etc/tasks-worker/peer.toml; for checks.";
      };
      announce = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Pairing: after the daemon starts, a oneshot `tasks-worker-announce.service` prints this
          worker's endpoint id and the controller command (`tasks peer add NAME ID ADDR`) to
          the journal and the console, and writes it to stateDir/endpoint-id.
        '';
      };
      waitForAddressSec = mkOption {
        type = types.ints.unsigned;
        default = 60;
        description = "Before starting, wait up to this long for the bind IP to be assigned (Wi-Fi at boot).";
      };
    };

    firewall = {
      openPeerPort = mkOption {
        type = types.bool;
        default = true;
        description = "Open the peer link's UDP port (nothing else).";
      };
      interfaces = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "wlo1" ];
        description = "Open it only on these interfaces; empty = on all interfaces.";
      };
    };

    toolchains = mkOption {
      type = types.listOf types.package;
      default = with pkgs; [
        rsync
        python3
        rustup
        gcc
        clang
        cmake
        ninja
        gnumake
        pkg-config
        openssl
        openssl.dev
        perl
        protobuf
        # p2p_chat video: pkg-config aom/vpx/vmaf, nasm for rav1e/libaom asm.
        nasm
        libaom
        libaom.dev
        libvpx
        libvpx.dev
        libvmaf
        libvmaf.dev
        # p2p_chat desktop: alsa (cpal), meson (webrtc-audio-processing), patchelf.
        alsa-lib
        alsa-lib.dev
        meson
        patchelf
        # p2p_chat Android: JDK 21 for Gradle/AGP, cargo-ndk; unzip for sdkmanager archives.
        jdk21
        cargo-ndk
        unzip
      ];
      defaultText = literalExpression "the package list of nix/taskworker-host.nix (rustup, clang, cmake, jdk21, cargo-ndk, codec libs, ...)";
      description = ''
        Build tools on the daemon's PATH (and in systemPackages, so SSH sessions see the same).
        Defaults to what nix/taskworker-host.nix provides on the current worker. rustup reads
        rust-toolchain.toml; toolchains are installed per user (online, once).
      '';
    };

    android = {
      enable = mkEnableOption "ANDROID_HOME/JAVA_HOME for jobs (the SDK itself is installed per user with sdkmanager; licences are never accepted here)";
      sdkRoot = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "ANDROID_HOME; null = <user home>/Android/Sdk.";
      };
      jdk = mkOption {
        type = types.package;
        default = pkgs.jdk21;
        defaultText = literalExpression "pkgs.jdk21";
        description = "JDK linked at /etc/taskworker/jdk21 (JAVA_HOME, stable across rebuilds).";
      };
      adbUsb = mkEnableOption "USB access to adb/fastboot devices for `user` (group adbusers + udev rules; headless, no seat/uaccess)";
    };

    bindgen = {
      enable = mkOption {
        type = types.bool;
        default = true;
        description = ''
          LIBCLANG_PATH and BINDGEN_EXTRA_CLANG_ARGS (clang's resource headers + glibc headers)
          for jobs whose build scripts run bindgen (e.g. p2p_chat desktop's
          webrtc-audio-processing). tasks forwards both from the daemon to live commands.
        '';
      };
      llvmPackages = mkOption {
        type = types.attrs;
        default = pkgs.llvmPackages;
        defaultText = literalExpression "pkgs.llvmPackages";
        description = "LLVM package set whose libclang bindgen loads.";
      };
    };

    operatorTools = mkOption {
      type = types.listOf types.package;
      default = with pkgs; [
        htop
        iotop-c
        lsof
        tmux
      ];
      defaultText = literalExpression "with pkgs; [ htop iotop-c lsof tmux ]";
      description = "Tools for the operator's SSH sessions (systemPackages only, not the daemon's PATH): no `nix-shell -p htop` on a worker.";
    };

    declaredCapabilities = mkOption {
      type = types.listOf (types.strMatching "[A-Za-z0-9._+-]+");
      default = defaultDeclaredCapabilities;
      defaultText = literalExpression ''
        libclang (bindgen.enable), jdk (android.enable or a JDK in toolchains), android-sdk
        (android.enable), cargo-ndk (in toolchains) and the main program of each operatorTools
        package (htop, iotop-c, lsof, tmux)
      '';
      description = ''
        TASKS_DECLARED_CAPABILITIES for the daemon: what this node promises. The health monitor
        and `tasks doctor` probe each one and raise `declared_capability:NODE:NAME` (with a fix
        line) when it does not resolve, and placement/reroute skip the node for commands that
        need it (docs/design/node-setup-policy.md).
      '';
    };

    nixLd = {
      enable = mkOption {
        type = types.bool;
        default = true;
        description = "programs.nix-ld for rustup toolchains, the Android SDK's adb and prebuilt tools.";
      };
      libraries = mkOption {
        type = types.listOf types.package;
        default = with pkgs; [
          libaom
          libvpx
          libvmaf
          openssl
        ];
        defaultText = literalExpression "[ libaom libvpx libvmaf openssl ]";
        description = "Extra programs.nix-ld.libraries for offloaded test binaries.";
      };
    };

    keepAwake = mkOption {
      type = types.bool;
      default = true;
      description = "Ignore lid/idle and disable sleep/suspend/hibernate targets (a worker must not sleep).";
    };

    excludeLockDirFromTmpCleaning = mkOption {
      type = types.bool;
      default = true;
      description = ''
        tmpfiles `x /tmp/claude-*`: never age-clean the admission lock directory, so held flock
        inodes stay stable (deploy/worker/README.md "/tmp cleaning").
      '';
    };

    extraEnvironment = mkOption {
      type = types.attrsOf types.str;
      default = { };
      example = {
        TASKS_RUN_CPU_SOME_MAX_PCT = "80";
        TASKS_MIN_CACHE_FREE_GIB = "20";
      };
      description = "Extra daemon environment; wins over the module's values. Jobs only see the variables tasks forwards (src/worker.rs PASSTHROUGH_ENV and the live-command list); others configure the daemon/worker only.";
    };

    restartOnSwitch = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Restart the daemon when a rebuild changes the unit. Off by default: a restart during
        `nixos-rebuild switch` should be a deliberate, idle-time step (`tasks list --active`).
        Running jobs survive a daemon restart anyway (KillMode=process).
      '';
    };
  };

  config = mkIf cfg.enable (mkMerge [
    {
      assertions = [
        {
          assertion = !cfg.peer.enable || bindPort != null;
          message = "services.tasks-worker.peer.bind must be \"IP:PORT\" (or set peer.enable = false).";
        }
        {
          assertion = !cfg.peer.enable || (bindHost != "0.0.0.0" && bindHost != "::");
          message = "services.tasks-worker.peer.bind must name the LAN interface address, not 0.0.0.0/::.";
        }
        {
          assertion = !cfg.peer.enable || cfg.peer.controllers != [ ];
          message = "services.tasks-worker.peer.controllers is empty: no controller could reach this worker.";
        }
        {
          assertion = builtins.all (c: validName c.name && hex64 c.endpointId) cfg.peer.controllers;
          message = "services.tasks-worker.peer.controllers: name must match [A-Za-z0-9._-]{1,64} and endpointId must be the 64-hex `tasks peer id`.";
        }
        {
          assertion =
            lib.length (lib.unique (map (c: c.name) cfg.peer.controllers))
            == lib.length cfg.peer.controllers;
          message = "services.tasks-worker.peer.controllers: duplicate name.";
        }
        {
          assertion = !lib.hasPrefix builtins.storeDir cfg.peer.secretKeyFile;
          message = "services.tasks-worker.peer.secretKeyFile must not be in the Nix store.";
        }
        {
          assertion = dedicatedUser || (config.users.users ? ${cfg.user});
          message = "services.tasks-worker.user = \"${cfg.user}\" must be declared in users.users.";
        }
      ];

      users.groups = optionalAttrs dedicatedUser { tasks-worker = { }; };
      users.users = optionalAttrs dedicatedUser {
        tasks-worker = {
          isSystemUser = true;
          group = "tasks-worker";
          home = cfg.stateDir;
          createHome = true;
          # A login shell, so the operator can `sudo -iu tasks-worker` (rustup, sdkmanager).
          shell = pkgs.bashInteractive;
        };
      };

      systemd.tmpfiles.rules = [
        "d ${cfg.stateDir} 0700 ${cfg.user} ${group} - -"
      ]
      ++ optional cfg.excludeLockDirFromTmpCleaning "x /tmp/claude-*";

      environment.etc."tasks-worker/peer.toml" = mkIf cfg.peer.enable {
        source = peerConfigFile;
      };
      environment.systemPackages = [
        cfg.package
        peerIdScript
        pkgs.git
      ]
      ++ cfg.toolchains
      ++ cfg.operatorTools
      ++ optional cfg.sccache.enable cfg.sccache.package;
      # The .pc files of toolchains at /run/current-system/sw/lib/pkgconfig (PKG_CONFIG_PATH).
      environment.pathsToLink = [ "/lib/pkgconfig" ];

      systemd.services.tasks-worker = {
        description = "task-manager worker daemon (tasks daemon, native admission, iroh peer link)";
        wantedBy = [ "multi-user.target" ];
        wants = [ "network-online.target" ] ++ optional sccacheServer "tasks-worker-sccache.service";
        after = [ "network-online.target" ] ++ optional sccacheServer "tasks-worker-sccache.service";
        restartIfChanged = cfg.restartOnSwitch;
        environment = lib.mapAttrs (_: lib.mkForce) serviceEnvironment;
        serviceConfig = {
          Type = "simple";
          User = cfg.user;
          Group = group;
          ExecStartPre = "${preStart}";
          ExecStart = "${lib.getExe cfg.package} daemon";
          WorkingDirectory = home;
          UMask = "0077";
          # Stopping stops only the daemon; the worker and admitted jobs keep their own
          # process groups and leases. Stopping is NOT cancellation.
          KillMode = "process";
          Restart = "on-failure";
          RestartSec = 30;
          # Exit 2 is a configuration error such as "another daemon owns this state
          # directory" (e.g. an autostarted or old user-unit daemon): don't loop on it.
          RestartPreventExitStatus = 2;
          # Deliberately NO PrivateTmp, ProtectHome, ProtectSystem or other mount namespace:
          # the admission lock directory /tmp/claude-$UID, the state socket and the live
          # checkouts must be the same files that SSH clients and legacy helpers see, or
          # admission silently stops excluding them (deploy/worker/README.md).
        };
      };
    }

    (mkIf sccacheServer {
      # The compiler-cache server, started outside every job (see sccache.server).
      systemd.services.tasks-worker-sccache = {
        description = "sccache server for task-manager jobs (outside any job's slot lease)";
        wantedBy = [ "multi-user.target" ];
        before = [ "tasks-worker.service" ];
        environment = {
          SCCACHE_DIR = sccacheDir;
          SCCACHE_CACHE_SIZE = cfg.sccache.size;
          SCCACHE_IDLE_TIMEOUT = "0";
          # Foreground server (what `sccache --start-server` runs, without daemonizing), so
          # systemd tracks and restarts it.
          SCCACHE_START_SERVER = "1";
          SCCACHE_NO_DAEMON = "1";
        };
        serviceConfig = {
          Type = "simple";
          User = cfg.user;
          Group = group;
          WorkingDirectory = home;
          UMask = "0077";
          ExecStart = lib.getExe' cfg.sccache.package "sccache";
          # A restarted server serves the same on-disk cache; between exit and restart a
          # job's rustc would spawn one inside the job again, so keep the gap short.
          Restart = "always";
          RestartSec = 2;
          # Same reason as the daemon: no private /tmp or home.
        };
      };
    })

    # Two mkIfs, not an `if` choosing attribute names: the module system must see the
    # attribute structure without evaluating config (that `if` is infinite recursion).
    (mkIf (cfg.peer.enable && cfg.peer.announce) {
      systemd.services.tasks-worker-announce = {
        description = "Print the task-manager worker's endpoint id for pairing";
        wantedBy = [ "multi-user.target" ];
        after = [ "tasks-worker.service" ];
        wants = [ "tasks-worker.service" ];
        serviceConfig = {
          Type = "oneshot";
          User = cfg.user;
          Group = group;
          UMask = "0022";
          StandardOutput = "journal+console";
        };
        script = ''
          key=${lib.escapeShellArg cfg.peer.secretKeyFile}
          # The daemon's ExecStartPre creates the key; give a slow first boot a minute.
          for _ in $(seq 60); do [ -e "$key" ] && break; sleep 1; done
          id=$(${lib.getExe peerIdScript})
          echo "$id" > ${lib.escapeShellArg "${cfg.stateDir}/endpoint-id"}
          echo "tasks-worker: endpoint id $id"
          echo "tasks-worker: on the controller run: tasks peer add ${config.networking.hostName} $id ${toString cfg.peer.bind}"
        '';
      };
    })

    (mkIf (cfg.peer.enable && cfg.firewall.openPeerPort && bindPort != null) {
      networking.firewall.allowedUDPPorts = mkIf (cfg.firewall.interfaces == [ ]) [ bindPort ];
      networking.firewall.interfaces = mkIf (cfg.firewall.interfaces != [ ]) (
        lib.genAttrs cfg.firewall.interfaces (_: {
          allowedUDPPorts = [ bindPort ];
        })
      );
    })

    (mkIf cfg.android.enable {
      environment.etc."taskworker/jdk21".source = cfg.android.jdk.home;
    })

    (mkIf cfg.android.adbUsb {
      # systemd >= 258 grants adb USB nodes only via uaccess (an active seat); a headless
      # worker's daemon has none, and nixpkgs 26.05 dropped programs.adb. Use a group.
      users.groups.adbusers = { };
      users.users = lib.genAttrs [ cfg.user ] (_: {
        extraGroups = [ "adbusers" ];
      });
      services.udev.extraRules = ''
        SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", ENV{ID_DEBUG_APPLIANCE}=="android", GROUP="adbusers", MODE="0660"
        SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", ATTR{idVendor}=="18d1", GROUP="adbusers", MODE="0660"
        SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", ATTR{idVendor}=="22b8", GROUP="adbusers", MODE="0660"
      '';
    })

    (mkIf cfg.nixLd.enable {
      programs.nix-ld.enable = true;
      programs.nix-ld.libraries = cfg.nixLd.libraries;
    })

    (mkIf cfg.keepAwake {
      services.logind.settings.Login = {
        HandleLidSwitch = mkDefault "ignore";
        HandleLidSwitchExternalPower = mkDefault "ignore";
        HandleLidSwitchDocked = mkDefault "ignore";
        IdleAction = mkDefault "ignore";
      };
      systemd.targets.sleep.enable = false;
      systemd.targets.suspend.enable = false;
      systemd.targets.hibernate.enable = false;
      systemd.targets.hybrid-sleep.enable = false;
    })
  ]);
}
