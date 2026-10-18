# The policy file

Status: design (2026-10-18), not implemented beyond `broad_test`, `health::` and `reroute::`.
Part of the [node setup policy](node-setup-policy.md).

## Goal

One declarative HUML format says what each machine provides and how each project's work is
built, tested and deployed. The `tasks` binary reads it and enforces it; the binary itself
knows no particular tool. libclang, the JDK, the Android SDK, cargo and Gradle are entries
in a file, not cases in Rust.

## What is in code today, and where it moves

| Today | Where it is now | Moves to |
|---|---|---|
| What counts as cargo or Gradle, parallelism flags, cache env | Rust (`metrics/command`, `worker.rs`) | `tools::` |
| Capability probes, failure log patterns, which commands need what | Rust (`health/capability.rs`, classifier patterns) | `capabilities::` |
| What a machine provides | Nix module + `TASKS_DECLARED_CAPABILITIES` | machine file `provides::` |
| Machine thresholds (memory/disk gates, cargo jobs) | systemd unit environment | machine file `limits::` |
| Peer identity and allowlist | `~/.config/tasks/peer.toml` (TOML) | machine file `peers::` |
| Offload, fetch, deploy steps | Python (`tasks_offload.py`), shell (`deploy/install-*.sh`) | `recipes::` and `workflows::` |
| Broad-test rule, health and reroute thresholds | `deploy/policy.huml` (already HUML) | unchanged |

Today's behaviour becomes the built-in defaults, so moving an item never changes what
happens until someone edits the file.

## Two layers

- **Machine file**, `~/.config/tasks/policy.huml`. Written by the NixOS module (workers) or
  a one-time bootstrap (other hosts) from the machine's declaration. Holds what this machine
  provides and its limits: slots, memory and disk gates, cache paths, store quotas,
  capabilities provided, peers. It is the ceiling.
- **Project file**, `.tasks/policy.huml` in each repository. Holds what the project's work
  needs: tools used, recipes, workflows, required capabilities, test setup (fixtures,
  devices, denied devices). It may only tighten the machine file, never loosen it (a
  project cannot raise a memory gate's floor away, allow a denied device, or turn
  approval off).

Strict like today's file: unknown keys and bad values are errors, reported loudly at daemon
start, in `tasks admission` and on every submit, and the defaults apply. `tasks admission`
shows each effective value and its source (default, machine file, project file, env).

## Sections

The examples below are illustrative; exact key names and HUML layout are settled in the
implementation.

### `tools::`: what a command is

```huml
tools::
  cargo::
    match:: "cargo"
    jobs_flag: "-j"
    cache_env::
      RUSTC_WRAPPER: "sccache"
    compiler_procs:: "rustc", "cc", "c++"
    broad_test: "cargo-test-filter"
  gradle::
    match:: "gradlew", "gradle"
    jobs_flag: "--max-workers"
```

### `capabilities::`: what a job may need

```huml
capabilities::
  libclang::
    probe::
      file_glob:: "$LIBCLANG_PATH/libclang.so*", "/usr/lib/libclang.so*"
    missing_log:: "Unable to find libclang"
    needed_when::
      tool: "cargo"
      lock_mentions: "bindgen"
    fix: "provide libclang.so and set LIBCLANG_PATH in the machine declaration"
  android-sdk::
    probe::
      env_dir: "ANDROID_HOME"
    needed_when::
      tool: "gradle"
```

A probe is a file, a command, or an environment variable; `tasks doctor` and health run the
same probe on every machine. A failed job whose log matches `missing_log` marks the
capability missing on the machine that ran the job (for an offloaded or forwarded job, the
worker, not the submitter).

### Machine file: `provides::`, `limits::`, `storage::`, `peers::`

```huml
provides:: "libclang", "jdk", "android-sdk", "cargo-ndk", "htop"
limits::
  min_available_memory_mib: 1024
  max_cargo_jobs: 6
storage::
  checkouts_quota_gib: 60
  checkouts_max_age_d: 14
  blob_store_quota_gib: 10
  keep_copies: false
peers::
  laptop::
    endpoint_id: "<its tasks peer id>"
    addr: "192.168.1.10:7842"
    role: "host"
```

A gate above what the machine physically has (memory gate above total RAM) is reported as a
configuration error, not as pressure.

### `recipes::`: a named, checked command

```huml
recipes::
  core-tests::
    run:: "cargo", "test", "-p", "p2pcore", "--lib"
    needs:: "libclang"
    place: "anywhere"
  phone-install::
    run:: "adb", "install", "-r", "app-release.apk"
    place: "device"
    approve: true
```

Agents submit a recipe by name (MCP `submit`), which `tasks` checks at submit: unknown
recipe, missing capability on every eligible machine, or a step that needs approval.

### `workflows::`: steps with dependencies

A workflow is a named set of steps, each a recipe or an action, with `after:` dependencies.
Every step runs as an ordinary job, so admission, placement, capabilities, logs and restart
survival apply to every step.

- `after:` order; independent steps run in parallel on whichever machines have free slots.
- `place:` `anywhere | laptop | worker | device`, plus the recipe's `needs`.
- `out:` / `fetch:` artifacts a step produces and later steps receive (replaces
  `tasks-offload --fetch`).
- `approve: true` for phone installs, data wipes and deploys; device allowlists and denylists
  (the user's personal phone ZD22257P6G is always denied).
- `on_failure:` `stop | continue`, plus `always:` cleanup steps.

```huml
workflows::
  android-release::
    steps::
      native::
        recipe: "native-libs"
        place: "worker"
      apk::
        recipe: "gradle-assemble-release"
        after:: "native"
        fetch:: "native"
      install::
        recipe: "phone-install"
        after:: "apk"
        approve: true
  tasks-deploy::
    steps::
      build-worker::
        recipe: "tasks-headless"
        place: "worker"
      build-laptop::
        recipe: "tasks-gui"
        place: "laptop"
      switch-worker::
        action: "install"
        after:: "build-worker"
        drain: true
        approve: true
      switch-laptop::
        action: "install"
        after:: "build-laptop", "switch-worker"
        drain: true
        approve: true
```

`tasks-deploy` replaces `deploy/install-worker.sh` and `deploy/install-laptop.sh`. Drain,
switch, restart under the unit and a check that the running daemon is the installed binary
are written once in `tasks` and tested once (both scripts had restart races on 2026-10-18).

**Limit:** the file is data, not a programming language. Steps, dependencies, gates and
parameters only: no loops, conditions or embedded code. Anything that needs logic is a script
in the repository that a step runs like any other command. That keeps workflows readable,
checkable at submit and safe to resume after a restart.

## Order of work

1. Schema and loader for the two layers; `tasks admission` shows sources.
2. Move `tools::` and `capabilities::` out of Rust, today's behaviour as defaults; fix the
   probe (`libclang.so`, not `libclang-cpp.so`) and mark the machine that ran a failed job.
3. NixOS module writes the machine file; thresholds and peers move out of the unit
   environment and `peer.toml`.
4. `recipes::`, with MCP `submit` taking a recipe name.
5. `workflows::`, then `tasks-deploy` replaces the install scripts.
