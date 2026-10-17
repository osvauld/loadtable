# Linux worker groundwork (local only)

Requires Python 3.11+ and Linux. No extra Python packages. Run from this checkout:

```sh
scripts/setup-linux-worker.sh --inspect
scripts/setup-linux-worker.sh --plan --cache /absolute/intended/cache
scripts/setup-linux-worker.sh --apply  # always refuses, exit 2
```

All outputs are JSON. Inspect/plan exit 0 only for **Ubuntu 24.04, Debian 12 or
NixOS (any well-formed reported `VERSION_ID`, recorded verbatim), x86_64/aarch64**;
unsupported/unknown hosts return their inspection and exit 2.
This is support for metadata inspection, not a supported worker deployment.
No package manager, tool `--version`, rustup shim, SDK manager, GPU utility or
emulator is executed. Tool versions are unknown, not inferred from PATH.
No downloads, license acceptance, directory creation, sudo, group changes or
services occur. Permission checks do not open `/dev/kvm` or GPU nodes.

### NixOS

On `ID=nixos` the inspection adds a `nixos` object (null elsewhere), from
`etc/NIXOS`, bounded `proc/mounts` and lstat/is_dir metadata only:
`etc_nixos_present`, `root_fs_type`, `nix_store_fs_type` (longest covering
mount), `nix_ro_store_squashfs` (`/nix/.ro-store`), `root_volatile` and
`nix_store_volatile` (tmpfs/ramfs/overlay/squashfs/rootfs), `live_iso`
(marker + volatile root + volatile store; null when mounts are unreadable),
`persistent`, `current_system_present`, `nix_ld_present`
(`run/current-system/sw/share/nix-ld`) and `ld_linux_present`
(`/lib64/ld-linux-x86-64.so.2` or the aarch64 loader). NixOS >= 24.05 ships a
stub loader, so `ld_linux_present` alone is not nix-ld evidence. The NixOS plan
proposes no package-manager command: its `review-declarative-nixos-module`
proposal points at `nix/module.nix` (`services.tasks-worker`) and the root `flake.nix`,
with the module's defaults listed and `draft/validated/applied/approved` explicit
(evaluated with `nix flake check --no-build`, never applied). A live ISO
is a blocker (state is not persistent); unknown mounts are a blocker too.
See [nix/README.md](../nix/README.md).

The plan is deliberately non-executable and blocked: worker versions/checksums,
download budgets, package pins and optional Android destinations need separate
review. It proposes no install command. Cache facts use statvfs at the intended
location or nearest existing ancestor; no recursive usage scan or cache policy
changes. RAM/swap/CPU flags use bounded metadata reads (oversized/missing data
becomes unknown); flags and device nodes are not runtime readiness proof.
`logical_cpus` is the process affinity count (`sched_getaffinity`, else
`os.cpu_count()`; `logical_cpus_source` says which) and is only reported for the
real `/` root; synthetic fixture roots report null unless injected. cgroup v2
limits are read-only: `proc/self/cgroup`'s unified path, then each ancestor's
`cpu.max`/`memory.max` (bounded reads), tightest wins, reported separately as
`cgroup_cpu_limit` (CPUs, float) and `cgroup_memory_max_bytes`. `max`, absent,
malformed or escaping values are null; null means "no limit observed", not
"unlimited". cgroup v1, CPU weight and memory.high are not inspected.

## Explicit local source selection

```sh
python3 -B scripts/worker_source.py --root /absolute/source \
  --file Cargo.toml --file src/lib.rs
# Optional repeated --have-sha256 DIGEST supplies current local possession claims.
```

This command captures bytes in memory, emits a versioned manifest/receipt and
missing SHA-256 list, then exits. It writes no source capsule or blob store and
sends nothing. Do not use a CLI receipt to later reread a mutable workspace for
transfer. For an in-process test, retain the capture itself:

```python
from worker_source import capture, diff

selected = capture(root, ["Cargo.toml", "src/lib.rs"])
receipt = selected.receipt  # local fixture's expected digest, NOT execution approval
missing = selected.missing(currently_possessed_sha256, expected_receipt=receipt)
for digest in missing:
    immutable_bytes = selected.blob(digest, expected_receipt=receipt)
```

Each explicitly selected file may be dirty or untracked; no Git/HEAD is needed.
Only individual normalized relative files are accepted: no directory recursion,
globs or symlinks below the root, hardlinks or special files. The root itself
is resolved once with `realpath` (so a root under a symlinked parent, or a
symlinked root, works), must be an existing absolute directory, and is then
walked with no-follow opens. The resolved root appears only as CLI
`diagnostics.resolved_root` / `Capture.resolved_root`, never in the manifest
or receipt, so receipts are independent of the absolute location. The caller
must supply the next allowed set after additions/deletions; stale selections
fail rather than silently omitting missing files. `diff(old, new)` reports added,
removed and changed paths. Renames are removal+addition; content deduplicates
independently of path and executable mode. Deletion affects the new manifest
only, never any old capture or workspace.

Default bounds: 256 files, 4 MiB/file, 16 MiB total, 32 path components, 1024
UTF-8 bytes/path. In-process callers may opt in via `Limits(...)` up to the
built-in `CEILING`: 20000 files, 64 MiB/file, 512 MiB total, 32 components;
anything above is rejected. Capture is entirely in memory (both inventories
transiently), so large captures need the future on-disk blob store; the CLI
always uses the defaults. Two inventories detect ordinary concurrent edits
without retry, not an atomic snapshot or hostile-UID sandbox.

Exclusion rule (checked before reading, even if explicitly selected):

- Any component, case-insensitive: VCS (`.git`, `.hg`, `.jj`, `.svn`),
  credential/config dirs (`.ssh`, `.aws`, `.gnupg`, `.docker`, `.kube`,
  `.config`, `.local`, `.cargo`, `.rustup`, `.android`, `.avd`, `credentials`,
  `secrets`, `captures`), `.cache`, `node_modules`, `__pycache__`, `.gradle`,
  `.cxx`, `.kotlin`, `local.properties`, `gradle.properties`, `.netrc`,
  `.npmrc`, `.pypirc`, `.git-credentials`, names starting `.env`/`id_rsa`/
  `id_ed25519`/`id_ecdsa`; final names with key/keystore/db/sqlite/log/pyc
  suffixes, `credentials.json`/`.toml`, `token`, `tokens.json`.
- Output names `target`, `build`, `dist`, `out`, `outputs`, `sdk`, `ndk`,
  `cache`, `caches` only (a) as the first component under the root, or (b) for
  `target`/`build`, as an intermediate directory whose parent contains a build
  marker: `Cargo.toml` -> `target`; `build.gradle(.kts)`/`settings.gradle(.kts)`
  -> `build`. Markers are checked by no-follow `fstatat` on the already-open
  parent descriptor (any entry type counts; non-ENOENT errors refuse); never
  opened or read. Elsewhere these names are ordinary source, e.g.
  `app/src/main/java/com/x/sdk/Foo.kt` or `src/build/mod.rs`.

This is NOT a secret scanner and not a complete output detector (e.g. a nested
`dist/` from a JS bundler or a Cargo target with a custom `target-dir` is not
recognised): callers must review content and filenames.

The manifest uses domain `task-manager-local-source-manifest-v1` and SHA-256
path/mode/length/content framing adapted from `src/workspace_snapshot.rs`.
Existing Rust receipts/adapters are untouched: their HEAD/layout contracts do
not fit this explicit no-HEAD selection. `iroh_blobs_hash` is explicitly null
at manifest level and on every file entry: iroh-blobs addresses content by
BLAKE3, and a future transport fills the per-file field. It is not part of the
receipt (framing and golden vector unchanged); these SHA-256 values must not be
reinterpreted as native iroh-blobs hashes.

Missing planning is a deterministic local possession model, not authenticated
peer evidence, approval, a wire protocol or remote transfer. Receipt matching
binds retrieval to these immutable bytes only; it does not grant execution.
Per-job authorization, quotas, upstream iroh-blobs, isolated materialization and
transport/restart/cancel semantics remain future work.

## Validation

```sh
tasks list; tasks admission
tasks run --who sol-linux-worker --run -- env PYTHONDONTWRITEBYTECODE=1 \
  python3 -m unittest discover -s tests -p 'test_*.py' -v
```

Shared job 41: 18 passed (14 new contracts, 4 existing wait-budget tests),
0 failed/skipped. Review-fix pass (owner `osv-worker-source`): see the design
document for job IDs and counts. Job 42: real-host CLI inspect/plan/apply checks passed;
EndeavourOS x86_64 was correctly reported unsupported (each CLI exit 2).
Supported distro cases are synthetic fixtures, not real-machine acceptance.
No Cargo build, setup/install, emulator launch or remote transfer was validated.
See [design](../docs/design/linux-worker-local-contracts.md) for the preserved
nested target-placement blocker; do not run further heavy Cargo builds there.
