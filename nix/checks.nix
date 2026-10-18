# Evaluation-only checks for services.tasks-worker. Each example is a NixOS container
# configuration (boot.isContainer: no bootloader/filesystems). The checks assert on the
# evaluated config: service, user, firewall, environment and the rendered peer config.
# A failed expectation throws at evaluation with the list of failures, so
# `nix flake check --no-build` is the whole test. The `tasks` package is a stub here; the
# derivations a full `nix flake check` builds are an empty file per check.
{
  nixpkgs,
  system,
  workerModule,
  hostExample,
  nixosConfigurations,
}:

let
  inherit (nixpkgs) lib;
  pkgs = nixpkgs.legacyPackages.${system};

  stub = pkgs.writeShellScriptBin "tasks" "exit 0";
  laptopId = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

  eval =
    modules:
    (lib.nixosSystem {
      modules = [
        workerModule
        {
          nixpkgs.hostPlatform = system;
          boot.isContainer = true;
          system.stateVersion = "26.05";
          services.tasks-worker.package = stub;
        }
      ]
      ++ modules;
    }).config;

  failedAssertions = cfg: map (a: a.message) (lib.filter (a: !a.assertion) cfg.assertions);

  # name = [ [ description bool ] ... ] -> a trivial derivation, or a throw naming failures.
  expect =
    name: cases:
    let
      failed = map (c: builtins.elemAt c 0) (lib.filter (c: !(builtins.elemAt c 1)) cases);
    in
    if failed == [ ] then
      pkgs.runCommand "tasks-worker-check-${name}" { } "touch $out"
    else
      throw "check ${name} failed:\n  - ${lib.concatStringsSep "\n  - " failed}";

  # hasInfix builds a regex, which may not carry store-path context: compare plain text.
  has = a: b: lib.hasInfix (builtins.unsafeDiscardStringContext a) (builtins.unsafeDiscardStringContext b);

  # 1. Defaults: dedicated system user, all-interface UDP port, sccache, no android.
  dedicated = eval [
    {
      services.tasks-worker = {
        enable = true;
        peer.bind = "10.0.0.2:7843";
        peer.controllers = [
          {
            name = "laptop";
            endpointId = laptopId;
            addr = "10.0.0.1:7843";
          }
        ];
      };
    }
  ];
  dSvc = dedicated.systemd.services.tasks-worker;
  dUnit = dedicated.systemd.units."tasks-worker.service".text;
  dEnv = dSvc.environment;
  dPeer = dedicated.services.tasks-worker.peer.rendered;

  # 2. The real worker (nix/hosts/lan-worker.nix) with a valid controller id pasted in.
  host = eval [
    hostExample
    {
      users.users.abe = {
        isNormalUser = true;
        home = "/home/abe";
      };
      services.tasks-worker.peer.controllers = lib.mkForce [
        {
          name = "laptop";
          endpointId = laptopId;
          approve = "manual";
        }
      ];
    }
  ];
  hSvc = host.systemd.services.tasks-worker;
  hUnit = host.systemd.units."tasks-worker.service".text;
  hEnv = hSvc.environment;
  hPeer = host.services.tasks-worker.peer.rendered;
  fw = host.networking.firewall;

  # 3. The host example as committed (placeholder id) must be refused.
  hostAsCommitted = eval [
    hostExample
    {
      users.users.abe = {
        isNormalUser = true;
        home = "/home/abe";
      };
    }
  ];

  # 4. Misconfigurations the module must refuse.
  refused =
    extra:
    failedAssertions (eval [
      {
        services.tasks-worker = lib.recursiveUpdate {
          enable = true;
          peer.bind = "10.0.0.2:7843";
          peer.controllers = [
            {
              name = "laptop";
              endpointId = laptopId;
            }
          ];
        } extra;
      }
    ]);

  # 5. Peer link off: nothing bound, no port, config path that does not exist. Bindgen off.
  noPeer = eval [
    {
      services.tasks-worker = {
        enable = true;
        peer.enable = false;
        bindgen.enable = false;
      };
    }
  ];

  # sccache off: no wrapper, no server unit, no ordering on it.
  noCache = eval [
    {
      services.tasks-worker = {
        enable = true;
        peer.enable = false;
        sccache.enable = false;
      };
    }
  ];
  dCache = dedicated.systemd.services.tasks-worker-sccache;

  # 6. Fresh-install machines (flake nixosConfigurations; real package, evaluated only).
  tpl = nixosConfigurations.worker-template.config;
  ex = nixosConfigurations.worker-example.config;
  tplFw = tpl.networking.firewall;
  diskOf = cfg: cfg.disko.devices.disk.main;
in
{
  template-and-example-host = expect "template" [
    [
      "worker-template: worker enabled as the dedicated user, peer on the template address"
      (
        tpl.services.tasks-worker.enable
        && tpl.services.tasks-worker.user == "tasks-worker"
        && tpl.services.tasks-worker.peer.rendered.bind == "192.168.1.50:7843"
        && (builtins.head tpl.services.tasks-worker.peer.rendered.peer).role == "host"
      )
    ]
    [
      "worker-template: firewall opens only TCP 22 (ssh) and UDP 7843 (peer)"
      (
        tpl.networking.firewall.enable
        && tplFw.allowedTCPPorts == [ 22 ]
        && tplFw.allowedUDPPorts == [ 7843 ]
        && tplFw.allowedTCPPortRanges == [ ]
        && tplFw.allowedUDPPortRanges == [ ]
      )
    ]
    [
      "worker-template: ssh key-only, keys for root and admin"
      (
        tpl.services.openssh.enable
        && !tpl.services.openssh.settings.PasswordAuthentication
        && tpl.services.openssh.settings.PermitRootLogin == "prohibit-password"
        && tpl.users.users.root.openssh.authorizedKeys.keys != [ ]
        && tpl.users.users.admin.openssh.authorizedKeys.keys != [ ]
      )
    ]
    [
      "worker-template: disko single disk, GPT, EFI vfat /boot + ext4 /, systemd-boot"
      (
        (diskOf tpl).device == "/dev/nvme0n1"
        && (diskOf tpl).content.partitions.ESP.content.mountpoint == "/boot"
        && (diskOf tpl).content.partitions.root.content.format == "ext4"
        && tpl.fileSystems."/".fsType == "ext4"
        && tpl.boot.loader.systemd-boot.enable
      )
    ]
    [
      "worker-template: placeholders are flagged as warnings"
      (lib.length tpl.warnings >= 2 && builtins.any (has "placeholder") tpl.warnings)
    ]
    [
      "worker-template: pairing oneshot prints `tasks peer add` after the daemon"
      (
        tpl.systemd.services.tasks-worker-announce.serviceConfig.Type == "oneshot"
        && builtins.elem "tasks-worker.service" tpl.systemd.services.tasks-worker-announce.after
        && has "tasks peer add worker-template " tpl.systemd.services.tasks-worker-announce.script
        && has " 192.168.1.50:7843" tpl.systemd.services.tasks-worker-announce.script
      )
    ]
    [
      "worker-template: sccache server unit and htop come with the module"
      (
        tpl.systemd.services ? tasks-worker-sccache
        && builtins.any (p: (p.pname or "") == "htop") tpl.environment.systemPackages
      )
    ]
    [
      "worker-template: build cache on the root fs (no separate mount), no failed assertions"
      (!(tpl.fileSystems ? "/var/lib/tasks-worker") && failedAssertions tpl == [ ])
    ]
    [
      "worker-example: its own name, address, controller id, disk; no placeholder warnings"
      (
        ex.networking.hostName == "worker-example"
        && ex.services.tasks-worker.peer.rendered.bind == "192.168.1.51:7843"
        && (builtins.head ex.services.tasks-worker.peer.rendered.peer).endpoint_id == laptopId
        && (diskOf ex).device == "/dev/nvme0n1"
        && failedAssertions ex == [ ]
        && !(builtins.any (has "placeholder") ex.warnings)
      )
    ]
  ];

  module-dedicated-user = expect "dedicated-user" [
    [
      "dedicated system user tasks-worker with home /var/lib/tasks-worker"
      (
        dedicated.users.users.tasks-worker.isSystemUser
        && dedicated.users.users.tasks-worker.home == "/var/lib/tasks-worker"
        && dedicated.users.groups ? tasks-worker
      )
    ]
    [
      "system service enabled at boot, runs as tasks-worker"
      (
        builtins.elem "multi-user.target" dSvc.wantedBy
        && dSvc.serviceConfig.User == "tasks-worker"
        && dSvc.serviceConfig.Group == "tasks-worker"
      )
    ]
    [
      "ExecStart is `<package>/bin/tasks daemon`"
      (dSvc.serviceConfig.ExecStart == "${stub}/bin/tasks daemon")
    ]
    [
      "Restart=on-failure, KillMode=process, exit 2 not retried, no restart on switch"
      (
        dSvc.serviceConfig.Restart == "on-failure"
        && dSvc.serviceConfig.KillMode == "process"
        && dSvc.serviceConfig.RestartPreventExitStatus == 2
        && !dSvc.restartIfChanged
      )
    ]
    [
      "no PrivateTmp/ProtectHome/ProtectSystem in the generated unit"
      (!(has "PrivateTmp" dUnit) && !(has "ProtectHome" dUnit) && !(has "ProtectSystem" dUnit))
    ]
    [
      "worker limits in the environment (6 jobs, 1024 MiB)"
      (dEnv.TASKS_MAX_CARGO_JOBS == "6" && dEnv.TASKS_MIN_AVAILABLE_MEMORY_MIB == "1024")
    ]
    [
      "sccache wrapper, 40G, dir under the user's home"
      (
        has "/bin/sccache" dEnv.RUSTC_WRAPPER
        && dEnv.SCCACHE_CACHE_SIZE == "40G"
        && dEnv.SCCACHE_DIR == "/var/lib/tasks-worker/build-cache/sccache"
      )
    ]
    [
      "sccache server: own unit as the service user, same dir/size, never idles out, before the daemon"
      (
        dCache.serviceConfig.User == "tasks-worker"
        && has "/bin/sccache" dCache.serviceConfig.ExecStart
        && dCache.environment.SCCACHE_DIR == dEnv.SCCACHE_DIR
        && dCache.environment.SCCACHE_CACHE_SIZE == dEnv.SCCACHE_CACHE_SIZE
        && dCache.environment.SCCACHE_IDLE_TIMEOUT == "0"
        && dCache.environment.SCCACHE_START_SERVER == "1"
        && dCache.environment.SCCACHE_NO_DAEMON == "1"
        && dCache.serviceConfig.Restart == "always"
        && builtins.elem "multi-user.target" dCache.wantedBy
        && builtins.elem "tasks-worker.service" dCache.before
        && builtins.elem "tasks-worker-sccache.service" dSvc.after
        && builtins.elem "tasks-worker-sccache.service" dSvc.wants
      )
    ]
    [
      "operator tools (htop) in systemPackages, not on the daemon's PATH"
      (
        builtins.any (p: (p.pname or "") == "htop") dedicated.environment.systemPackages
        && !(has "htop" dEnv.PATH)
      )
    ]
    [
      "sccache.enable = false: no wrapper, no server unit, no ordering"
      (
        !(noCache.systemd.services ? tasks-worker-sccache)
        && !(noCache.systemd.services.tasks-worker.environment ? RUSTC_WRAPPER)
        && !(builtins.elem "tasks-worker-sccache.service" noCache.systemd.services.tasks-worker.after)
        && failedAssertions noCache == [ ]
      )
    ]
    [
      "PATH: user's cargo bin first, then git and toolchains (cargo-ndk), then the system"
      (
        lib.hasPrefix "/var/lib/tasks-worker/.cargo/bin:" dEnv.PATH
        && has "-git-" dEnv.PATH
        && has "cargo-ndk" dEnv.PATH
        && has "/run/current-system/sw/bin" dEnv.PATH
      )
    ]
    [
      "declared capabilities: libclang, cargo-ndk, jdk (in toolchains) and operator tools; no android-sdk without android"
      (
        let
          caps = lib.splitString "," dEnv.TASKS_DECLARED_CAPABILITIES;
        in
        builtins.all (c: builtins.elem c caps) [
          "libclang"
          "jdk"
          "cargo-ndk"
          "htop"
        ]
        && !(builtins.elem "android-sdk" caps)
      )
    ]
    [
      "bindgen: LIBCLANG_PATH (libclang lib dir) and BINDGEN_EXTRA_CLANG_ARGS (clang + glibc headers)"
      (
        dEnv.LIBCLANG_PATH == "${pkgs.llvmPackages.libclang.lib}/lib"
        && has "-isystem ${pkgs.llvmPackages.libclang.lib}/lib/clang/" dEnv.BINDGEN_EXTRA_CLANG_ARGS
        && has "/include -idirafter ${pkgs.glibc.dev}/include" dEnv.BINDGEN_EXTRA_CLANG_ARGS
        # -isystem would put glibc before libstdc++, and <cstdlib>'s
        # `#include_next <stdlib.h>` then fails (worker job 263).
        && !(has "-isystem ${pkgs.glibc.dev}/include" dEnv.BINDGEN_EXTRA_CLANG_ARGS)
      )
    ]
    [
      "no TASK_MANAGER_STATE_DIR / TASKS_LOCK_DIR override, no android env by default"
      (
        !(dEnv ? TASK_MANAGER_STATE_DIR)
        && !(dEnv ? TASKS_LOCK_DIR)
        && !(dEnv ? ANDROID_HOME)
        && !(dEnv ? JAVA_HOME)
      )
    ]
    [
      "TASKS_PEER_CONFIG points at /etc/tasks-worker/peer.toml, which is rendered"
      (
        dEnv.TASKS_PEER_CONFIG == "/etc/tasks-worker/peer.toml"
        && dedicated.environment.etc ? "tasks-worker/peer.toml"
      )
    ]
    [
      "peer config: absolute key outside the store, bind, one controller as role host"
      (
        dPeer.secret_key == "/var/lib/tasks-worker/peer.key"
        && dPeer.bind == "10.0.0.2:7843"
        && dPeer.peer == [
          {
            name = "laptop";
            endpoint_id = laptopId;
            addr = "10.0.0.1:7843";
            role = "host";
            approve = "auto";
          }
        ]
      )
    ]
    [
      "UDP 7843 opened (all interfaces), no TCP port"
      (
        dedicated.networking.firewall.allowedUDPPorts == [ 7843 ]
        && dedicated.networking.firewall.allowedTCPPorts == [ ]
      )
    ]
    [
      "state dir 0700 and /tmp/claude-* excluded from age cleaning"
      (
        builtins.elem "d /var/lib/tasks-worker 0700 tasks-worker tasks-worker - -" dedicated.systemd.tmpfiles.rules
        && builtins.elem "x /tmp/claude-*" dedicated.systemd.tmpfiles.rules
      )
    ]
    [
      "endpoint-id helper installed"
      (builtins.any (p: (p.name or "") == "tasks-worker-peer-id") dedicated.environment.systemPackages)
    ]
    [ "no failed assertions" (failedAssertions dedicated == [ ]) ]
  ];

  module-existing-user = expect "existing-user" [
    [ "no failed assertions" (failedAssertions host == [ ]) ]
    [ "no dedicated user created" (!(host.users.users ? tasks-worker)) ]
    [
      "runs as abe:users with HOME-relative paths of abe"
      (
        hSvc.serviceConfig.User == "abe"
        && hSvc.serviceConfig.Group == "users"
        && hSvc.serviceConfig.WorkingDirectory == "/home/abe"
        && lib.hasPrefix "/home/abe/.cargo/bin:/home/abe/.local/bin:" hEnv.PATH
      )
    ]
    [
      "env matches the old user unit (6 jobs, 1024 MiB, sccache 40G, android)"
      (
        hEnv.TASKS_MAX_CARGO_JOBS == "6"
        && hEnv.TASKS_MIN_AVAILABLE_MEMORY_MIB == "1024"
        && hEnv.SCCACHE_CACHE_SIZE == "40G"
        && hEnv.SCCACHE_DIR == "/home/abe/build-cache/sccache"
        && hEnv.ANDROID_HOME == "/home/abe/Android/Sdk"
        && hEnv.JAVA_HOME == "/etc/taskworker/jdk21"
        && host.environment.etc ? "taskworker/jdk21"
        && hEnv.PKG_CONFIG_PATH == "/run/current-system/sw/lib/pkgconfig"
        && hEnv ? LIBCLANG_PATH
        && hEnv ? BINDGEN_EXTRA_CLANG_ARGS
      )
    ]
    [
      "android worker declares libclang, jdk, android-sdk, cargo-ndk and htop"
      (
        let
          caps = lib.splitString "," hEnv.TASKS_DECLARED_CAPABILITIES;
        in
        builtins.all (c: builtins.elem c caps) [
          "libclang"
          "jdk"
          "android-sdk"
          "cargo-ndk"
          "htop"
        ]
      )
    ]
    [
      "peer UDP 7843 only on wlo1 (not global); 7842 kept for lan-peer; ssh 22 on wlo1"
      (
        fw.allowedUDPPorts == [ ]
        && lib.sort lib.lessThan fw.interfaces.wlo1.allowedUDPPorts == [
          7842
          7843
        ]
        && fw.interfaces.wlo1.allowedTCPPorts == [ 22 ]
      )
    ]
    [
      "peer config: controller only, role host, manual approval honoured, no addr"
      (
        hPeer.bind == "192.168.1.7:7843"
        && hPeer.secret_key == "/var/lib/tasks-worker/peer.key"
        && hPeer.peer == [
          {
            name = "laptop";
            endpoint_id = laptopId;
            role = "host";
            approve = "manual";
          }
        ]
      )
    ]
    [
      "adb USB group for abe, nix-ld, no sleep"
      (
        builtins.elem "adbusers" host.users.users.abe.extraGroups
        && has "adbusers" host.services.udev.extraRules
        && host.programs.nix-ld.enable
        && !host.systemd.targets.suspend.enable
      )
    ]
    [
      "no PrivateTmp/ProtectHome in the generated unit; User=abe"
      (!(has "PrivateTmp" hUnit) && !(has "ProtectHome" hUnit) && has "User=abe" hUnit)
    ]
  ];

  module-refusals = expect "refusals" [
    [
      "the committed host example (placeholder endpoint id) is refused"
      (builtins.any (has "endpointId") (failedAssertions hostAsCommitted))
    ]
    [ "bind 0.0.0.0 refused" (refused { peer.bind = "0.0.0.0:7843"; } != [ ]) ]
    [ "bind without a port refused" (refused { peer.bind = "10.0.0.2"; } != [ ]) ]
    [ "no controllers refused" (refused { peer.controllers = [ ]; } != [ ]) ]
    [
      "key in the store refused"
      (refused { peer.secretKeyFile = "${builtins.storeDir}/x-peer.key"; } != [ ])
    ]
    [ "unknown existing user refused" (refused { user = "nosuchuser"; } != [ ]) ]
  ];

  module-peer-disabled = expect "peer-disabled" [
    [ "no failed assertions" (failedAssertions noPeer == [ ]) ]
    [
      "no peer config, no port, TASKS_PEER_CONFIG names a missing file"
      (
        !(noPeer.environment.etc ? "tasks-worker/peer.toml")
        && noPeer.networking.firewall.allowedUDPPorts == [ ]
        && noPeer.systemd.services.tasks-worker.environment.TASKS_PEER_CONFIG
        == "/var/empty/tasks-worker-peer-disabled.toml"
      )
    ]
    [
      "bindgen.enable = false sets neither bindgen variable nor declares libclang"
      (
        !(noPeer.systemd.services.tasks-worker.environment ? LIBCLANG_PATH)
        && !(noPeer.systemd.services.tasks-worker.environment ? BINDGEN_EXTRA_CLANG_ARGS)
        && !(builtins.elem "libclang" (
          lib.splitString "," noPeer.systemd.services.tasks-worker.environment.TASKS_DECLARED_CAPABILITIES
        ))
      )
    ]
  ];

  # Built (not only evaluated) by a full `nix flake check`: parse C and C++ headers with the
  # unwrapped clang (like libclang, no Nix wrapper paths) and exactly the module's
  # BINDGEN_EXTRA_CLANG_ARGS, the way webrtc-audio-processing-sys's bindgen does.
  module-bindgen-cxx-probe =
    let
      clang = pkgs.llvmPackages.clang-unwrapped;
      cxx = "${pkgs.stdenv.cc.cc}/include/c++/${pkgs.stdenv.cc.cc.version}";
    in
    pkgs.runCommand "tasks-worker-check-bindgen-cxx-probe" { } ''
      printf '#include <cstdlib>\n#include <cstdio>\n#include <cstddef>\n' > probe.cc
      printf '#include <stdlib.h>\n#include <stddef.h>\n#include <stdint.h>\n' > probe.c
      ${clang}/bin/clang -x c++ -fsyntax-only -nostdinc ${dEnv.BINDGEN_EXTRA_CLANG_ARGS} \
        -isystem ${cxx} -isystem ${cxx}/${pkgs.stdenv.hostPlatform.config} probe.cc
      ${clang}/bin/clang -x c -fsyntax-only -nostdinc ${dEnv.BINDGEN_EXTRA_CLANG_ARGS} probe.c
      touch $out
    '';
}
