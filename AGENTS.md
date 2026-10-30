# Repository instructions

LoadTable: one build queue for coding agents across the owner's machines. `loadtable` is the
CLI and MCP server (`crates/tasks-cli`), `loadtabled` the daemon (`crates/tasks-daemon`); the
crates keep their `tasks-*` names from before the rename. The old names `tasks2`/`tasks2d`
are gone from the machines: never run `tasks2`, `tasks2d` or the retired v1 `tasks`.

## Builds and tests go through LoadTable itself

Run every cargo build, test and clippy through the installed `loadtable`
(`~/.local/bin/loadtable`; Claude sessions can use the LoadTable MCP tools, server `loadtable`, tools `mcp__loadtable__*`),
never bare `cargo` in a shell:

```sh
loadtable run --who YOUR_AGENT --anywhere -- cargo fmt --all --check
loadtable run --who YOUR_AGENT --anywhere -- cargo clippy --workspace --all-targets --features tasks-cli/mcp,tasks-cli/ui -j 6 -- -D warnings
loadtable run --who YOUR_AGENT --anywhere -- cargo test -p tasks-daemon --lib FILTER -j 6
loadtable run --who YOUR_AGENT --anywhere -- cargo test --workspace --features tasks-cli/mcp,tasks-cli/ui -j 6   # the full suite, before merging
```

- Name your test target (`-p X --lib FILTER`, `--test NAME`); a bare filter is refused unless
  you pass `--broad-test`. Pass `-j 6` at most. Do not set `RUSTC_WRAPPER` or
  `CARGO_TARGET_DIR`: LoadTable links `target/` to the cache disk and sets the compiler cache.
- `--anywhere` lets the daemon pick the laptop or the nixos peer; `--pin` keeps a job here.
  Jobs on a peer are confined to the build cache, the synced checkout and the toolchains.
- `loadtable list --active`, `loadtable status`, `loadtable logs ID`; `loadtable cancel ID`
  stops your own job only. If `loadtable` itself fails, report the exact error and job id; do
  not work around it with a shell build.

## Changes

- Behaviour is tracked by requirement ids `R-NNN`. A new or changed behaviour gets a row in
  `docs/REGRESSIONS.md` and a test named after it (`fn r826_...`); a change that alters an
  existing behaviour updates its row and test.
- Work on a branch, merge into `dev` when fmt, clippy and the full suite pass; `dev` goes to
  `main` by pull request. Releases follow `docs/RELEASING.md`. The owner pushes.
- User-facing text (CLI messages, refusals, docs) says what to run instead, in plain words.

## Deploying to the owner's machines

A job cannot restart the daemon that runs it, and a restart while jobs run interrupts them:
check `loadtable list --active` first.

- Laptop: `cargo install --locked --force --path crates/tasks-daemon --root ~/.local` and
  `--path crates/tasks-cli --features mcp`, then, when idle,
  `systemctl --user restart loadtabled.service`.
- nixos and the Mac: a peer job may not write `~/.local/bin`. Use the release installer
  (`curl -fsSL https://loadtable.osvauld.com/install.sh | sh`), or build into
  `~/build-cache/loadtable-<version>` with `--peer NAME`; the owner copies the binaries and
  restarts the service (`loadtabled` on both).

## Feedback

After meaningful use, add your own new file under `feedback/` from `feedback/TEMPLATE.md`
(`<agent>-<job-id>-<topic>.md`). The folder is local only (ignored by git, like `deploy/`).
Never edit another agent's report.
