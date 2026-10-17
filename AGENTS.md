# Task-manager repository instructions

This is the authoritative standalone source repository for the shared Rust/egui
workload coordinator. It was extracted from Tinline without deleting historical
copies or changing the running service/state. Read README.md and TRIAL.md fully.

- Verify cwd, branch, HEAD (may be unborn) and git status before work.
- Coordinate exact file ownership; never overwrite another agent's work/feedback.
- Heavy builds/tests use installed `tasks run --who UNIQUE_AGENT -- COMMAND`.
  Inspect `tasks list` and `tasks admission` first. No outer lock around worker
  E2E/self-suite/native_commands scripts: their workers acquire admission.
- Required trial feedback: task report plus your own new feedback file as in
  TRIAL.md. Record blockers before requesting a workaround; don't bypass locks.
- Keep each checkout stable during live compile/run. Honor host cache policy;
  don't override RUSTC_WRAPPER, CARGO_TARGET_DIR, GRADLE_USER_HOME or share targets.
  Missing cache-output links are placed by tasks; don't delete/migrate outputs.
- Build outputs, logs, captures, credentials and state must stay outside Git.
  Preserve required package/runtime assets and deliberate baselines.
- No staging, commits, pushes, merges/resets, phone installs/data wipes, cache
  cleanup or unrelated service restarts without explicit authorization.
- Installed runtime is a pinned independent release, not this Cargo target.
  Rebuilding source is NOT permission to replace it or restart a busy daemon.
  Preserve existing SQLite/socket compatibility, job history, stable flock
  inodes, approval semantics, process-group/descriptor leases and cancellation.
- Tinline adapters/cache namespaces are retained compatibility, not permission
  to modify Tinline, weaken its protocol policy or claim generic hermetic builds.
- Use synthetic identities/fixtures. Report exact checks/results/ignored tests
  and limitations; format owned files only. Models aren't production integration.

Repository is local, initially uncommitted, no remote. Do not manufacture an
initial commit just to give source-capsule tests a Git HEAD; request permission.

## LAN worker offload (NixOS worker 192.168.1.7)

Heavy Cargo/Gradle work can run on the worker's own `tasks` ledger through the
installed `~/.local/bin/tasks-offload` (docs: `scripts/README-offload.md`,
`deploy/worker/README.md`). Run it under a LOCAL run slot, from a checkout no
other agent is editing (the worker copy is keyed by host+path; same-checkout
offloads queue on a client flock):
`tasks run --run --who OWNER -- tasks-offload --worker abe@192.168.1.7
--ssh-key ~/.ssh/id_ed25519_nixos_worker --who OWNER [--fetch REL]... -- CMD`.
- Git-visible files (incl. uncommitted) are synced; `.gitignore`d files such as
  `gradle.properties`, local env files and secrets are not. Pass needed
  non-secret inputs as `env NAME=value`/`-P` flags; never put secrets in argv.
- Worker Cargo may use `-j` up to 8 (the laptop cap of 2 doesn't apply there).
  The worker env is an allowlist (`PASSTHROUGH_ENV` in `src/worker.rs`).
- Worker-built binaries link against `/nix/store`; they do not run on the
  laptop. For p2p_chat desktop use `scripts/offload-portable-desktop.sh`.
  Android APKs are re-signed locally with `~/.android/debug.keystore`; the
  keystore never leaves the laptop. Device installs stay local.
- `--transport iroh` is experimental (content-addressed sync/fetch via
  `lan-peer`); execution still uses SSH.
- Worker root/NixOS changes (`nix/taskworker-host.nix`), licence acceptance,
  `tasks` upgrades on either machine and unit edits are the user's steps: hand
  them a script, don't work around a denied remote write.
