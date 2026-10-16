# DRAFT NixOS worker module

Status: **draft, unvalidated, never applied.** Nix was not installed on the
authoring host, so `nix flake check` has not run. Nothing in this repository
runs `nixos-rebuild`, `nix-env` or any other mutating command.

`worker-module.nix` defines `services.taskmanager-worker`:

| Option | Default | Effect when enabled |
| --- | --- | --- |
| `enable` | false | System user/group `taskworker`, state dir `/var/lib/taskworker` (0750, tmpfiles), toolchain (rustc, cargo, cmake, ninja, pkg-config, clang from nixpkgs) as the user's packages |
| `kvm.enable` | false | Adds `taskworker` to `kvm` |
| `service.enable` / `service.execStart` | false / null | Placeholder hardened systemd unit for a future lan-peer/worker binary; no binary is packaged; execStart is required (assertion) |
| `lan.interface` / `lan.udpPorts` | null / [] | `networking.firewall.interfaces.<if>.allowedUDPPorts` only; ports without an interface fail an assertion; no global firewall port |
| `nixLd.enable` | false | `programs.nix-ld.enable = true` |
| `androidEmulator.enable` / `.package` | false / null | adds `android-tools` and an optional caller-supplied emulator composition; requires `kvm.enable`; Android SDK licences are never accepted here |

`flake.nix` exports `nixosModules.worker` (and `default`) and an
evaluation-only check against a container example. No `flake.lock` is
included; the nixpkgs branch `nixos-26.05` is an assumption matching the
probable target ISO and must be pinned deliberately.

A live ISO is not a durable worker (tmpfs root, overlay/squashfs store): the
inspector reports `nixos.live_iso` and the plan blocks on it. Installing NixOS,
importing this module and choosing the interface/ports are separate,
explicitly approved operator steps.

Validate only where Nix exists, through the shared coordinator:

```sh
tasks run --who AGENT --run -- nix flake check ./nix
```
