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
in
{
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
      "PATH: user's cargo bin first, then git and toolchains (cargo-ndk), then the system"
      (
        lib.hasPrefix "/var/lib/tasks-worker/.cargo/bin:" dEnv.PATH
        && has "-git-" dEnv.PATH
        && has "cargo-ndk" dEnv.PATH
        && has "/run/current-system/sw/bin" dEnv.PATH
      )
    ]
    [
      "bindgen: LIBCLANG_PATH (libclang lib dir) and BINDGEN_EXTRA_CLANG_ARGS (clang + glibc headers)"
      (
        dEnv.LIBCLANG_PATH == "${pkgs.llvmPackages.libclang.lib}/lib"
        && has "-isystem ${pkgs.llvmPackages.libclang.lib}/lib/clang/" dEnv.BINDGEN_EXTRA_CLANG_ARGS
        && has "/include -isystem ${pkgs.glibc.dev}/include" dEnv.BINDGEN_EXTRA_CLANG_ARGS
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
      "bindgen.enable = false sets neither bindgen variable"
      (
        !(noPeer.systemd.services.tasks-worker.environment ? LIBCLANG_PATH)
        && !(noPeer.systemd.services.tasks-worker.environment ? BINDGEN_EXTRA_CLANG_ARGS)
      )
    ]
  ];
}
