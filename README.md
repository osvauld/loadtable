# LoadTable

One build queue for all your coding agents. Their builds and tests run on whichever of your own
machines can take them, and everything that ran is on record: what, where, for whom and how long.

LoadTable is early software (alpha) for a handful of personal machines you trust. Website and
documentation: <https://loadtable.osvauld.com>.

## What it does

- **One queue per machine, shared by every agent.** Agents submit jobs (`loadtable run`, or the MCP
  tools) instead of running `cargo`/`gradle` in a shell, so ten agents don't start ten builds at
  once. Admission watches memory, CPU and I/O pressure and keeps each kind of job to its slots.
- **Your other machines take work.** Paired machines (over [iroh](https://iroh.computer), direct or
  relayed) run jobs the local machine is too busy or too small for. The checkout is synced, the
  output comes back, and placement learns from how long each job took where.
- **A record of everything.** Every job, refusal and peer action is in a local ledger and a signed
  audit log, with exact per-job resource figures where cgroups are available.
- **Rules that agents can't ignore.** Broad `cargo test` runs, multi-command shell scripts and
  denied devices are refused at submit with a message that says what to run instead.
- **Jobs from other machines are confined.** A job a peer sends runs under Landlock (Linux) or
  Seatbelt (macOS): it reaches the build cache and toolchains, not the rest of your home folder.

## Install

```sh
curl -fsSL https://loadtable.osvauld.com/install.sh | sh   # Linux x86-64/arm64, macOS
loadtabled install                                          # the daemon as a user service
loadtable status
```

Or from source, with a recent stable Rust:

```sh
cargo install --locked --path crates/tasks-daemon --root ~/.local
cargo install --locked --path crates/tasks-cli --features mcp --root ~/.local   # add ui for the window
```

Then [pair your machines](https://loadtable.osvauld.com/docs/pairing) and
[point your agents at it](https://loadtable.osvauld.com/docs/agents).

## Layout

| Crate | What |
|---|---|
| `tasks-cli` | `loadtable`: the CLI and the MCP server (`loadtable mcp`) |
| `tasks-daemon` | `loadtabled`: queue, admission, placement, peers, service install |
| `tasks-core`, `tasks-policy`, `tasks-admission`, `tasks-placement`, `tasks-timing` | the model and the rules |
| `tasks-ledger` | the job ledger and the audit log (SQLite) |
| `tasks-peer`, `tasks-identity` | pairing, trust and the peer protocol |
| `tasks-runner`, `tasks-platform` | running jobs; Linux and macOS specifics |
| `tasks-ui`, `tasks-mcp` | the desktop window; the MCP tools |

The crates keep their `tasks-*` names from before the project was named.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --features tasks-cli/mcp -- -D warnings
cargo test --workspace --features tasks-cli/mcp
```

`docs/REGRESSIONS.md` lists every behaviour with the tests that hold it; a change that alters one
updates its row. Releases: `docs/RELEASING.md`.

## License

MIT, see [LICENSE](LICENSE).
