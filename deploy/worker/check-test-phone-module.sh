#!/bin/sh
# Evaluates nix/taskworker-host.nix in the worker's real configuration WITHOUT
# installing or switching anything: copies /etc/nixos/*.nix into a temporary
# directory, swaps in this checkout's module, instantiates the system and
# prints what the test-phone change adds. No root, no nixos-rebuild.
# Run from the repository root on the worker (e.g. through a worker tasks job).
set -eu
module=${1:-nix/taskworker-host.nix}
tmp=$(mktemp -d "${TMPDIR:-/tmp}/taskworker-eval.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
cp /etc/nixos/configuration.nix /etc/nixos/hardware-configuration.nix "$tmp/"
cp "$module" "$tmp/taskworker.nix"
export NIX_PATH="nixpkgs=/nix/var/nix/profiles/per-user/root/channels/nixos:nixos-config=$tmp/configuration.nix"
eval_attr() {
  nix-instantiate --eval --strict --json '<nixpkgs/nixos>' -A "$1"
}
echo "abe extraGroups: $(eval_attr config.users.users.abe.extraGroups)"
echo "adbusers group:  $(eval_attr config.users.groups.adbusers.name)"
rules=$(nix-instantiate --eval --strict --json '<nixpkgs/nixos>' -A config.services.udev.extraRules |
  python3 -c 'import json,sys; print(json.load(sys.stdin))')
printf '%s\n' "$rules" | grep adbusers
# Syntax check. Before the switch the adbusers group does not exist yet, which
# udevadm reports as an error, so check a copy that names an existing group.
if getent group adbusers >/dev/null; then group=adbusers; else group=users; fi
printf '%s\n' "$rules" | sed "s/GROUP=\"adbusers\"/GROUP=\"$group\"/" > "$tmp/99-local.rules"
udevadm verify --no-style "$tmp/99-local.rules"
nix-instantiate --eval --strict --json -E \
  'map (p: p.name or "?") (import <nixpkgs/nixos> { }).config.environment.systemPackages' |
  python3 -c 'import json,sys; n=json.load(sys.stdin); print("htop:", any(x.startswith("htop") for x in n), "| kitty terminfo:", [x for x in n if "kitty" in x])'
drv=$(nix-instantiate '<nixpkgs/nixos>' -A system)
echo "system derivation: $drv"
