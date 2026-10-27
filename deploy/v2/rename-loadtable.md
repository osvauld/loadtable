# Moving a `tasks2` machine to LoadTable (R-804)

`tasks2`/`tasks2d` are now `loadtable`/`loadtabled`. The first `loadtabled` start moves the
old state (ledger, identity, pairings), config and warm build outputs to their new names and
leaves links at the old paths, so nothing has to be copied or paired again. Old and new
machines keep talking to each other while they are upgraded one by one.

Do it when the machine has no running jobs: stopping the old service interrupts them.

**Before the swap, on a machine with a phone that must never be used:** the serial is no longer
built in (R-820). Add it to the machine policy (`~/.config/tasks2/machine-policy.toml` before the
move, `~/.config/loadtable/` after), or every job may use that phone:

```toml
[machine]
denied_devices = ["<the phone's serial>"]
```

## Laptop and the NixOS worker (systemd)

```sh
# build and install (CLI with ui,mcp on the laptop; mcp only on the worker)
cargo install --locked --force --path crates/tasks-daemon --root ~/.local
cargo install --locked --force --path crates/tasks-cli --features ui,mcp --root ~/.local

# the old names keep working for scripts and agent docs for a while
ln -sfn loadtable  ~/.local/bin/tasks2
ln -sfn loadtabled ~/.local/bin/tasks2d

# swap the service: stop the old one first, or the move refuses to run under it
systemctl --user disable --now tasks2d.service
loadtabled install --machine "$(hostname)"   # writes ~/.config/systemd/user/loadtabled.service
systemctl --user daemon-reload
systemctl --user enable --now loadtabled.service
rm ~/.config/systemd/user/tasks2d.service && systemctl --user daemon-reload

loadtable status | head -5                   # the daemon log says "rename: moved ..."
```

If the worker's unit is declared in the NixOS configuration instead, rename
`systemd.user.services.tasks2d` to `systemd.user.services.loadtabled` there and point
`ExecStart` at `loadtabled serve`; keep `Delegate`/`DelegateSubgroup` (see `worker-unit.md`).

## The Mac (launchd)

```sh
cargo install --locked --force --path crates/tasks-daemon --root ~/.local
cargo install --locked --force --path crates/tasks-cli --features ui,mcp --root ~/.local
ln -sfn loadtable  ~/.local/bin/tasks2
ln -sfn loadtabled ~/.local/bin/tasks2d

launchctl bootout gui/$(id -u)/tasks2d
loadtabled install --machine mac             # writes ~/Library/LaunchAgents/loadtabled.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/loadtabled.plist
rm ~/Library/LaunchAgents/tasks2d.plist
```

From then on the restart is `launchctl kickstart -k gui/$(id -u)/loadtabled`.

## Agents

- The MCP server now calls itself `loadtable`. Rename the server entry in each client's
  config (for Claude Code, the `tasks2` key in `~/.claude.json`) to `loadtable` and point its
  `command` at `~/.local/bin/loadtable`; its tools then show as `mcp__loadtable__*`.
- `TASKS_WHO` and every other `TASKS_*` setting still work; the new names are `LOADTABLE_*`.
- Repos may keep `.tasks/run-policy.toml`; `.loadtable/run-policy.toml` wins where both exist.
- The CLI's first line is now `loadtable: submitted job N; ...` (was `tasks: ...`); scripts that
  parse it need the new prefix.
