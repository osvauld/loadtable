#!/bin/sh
# Operator script: enable the USB test phone on the NixOS worker.
#
# Run by the USER (not an agent), interactively, on the laptop, from this
# repository's checkout. It needs the worker's sudo password (typed on the
# ssh tty; never stored). Every root step asks first.
#
#   deploy/worker/enable-test-phone.sh            # install module, switch, verify
#   deploy/worker/enable-test-phone.sh --verify   # only the read-only checks
#
# What it does (deploy/worker/README.md, "Test phone on the worker"):
#   1. copies nix/taskworker-host.nix to ~/taskworker.nix.new on the worker and
#      shows the diff against /etc/nixos/taskworker.nix;
#   2. backs up /etc/nixos/taskworker.nix (.pre-test-phone), installs the new
#      module and runs `nixos-rebuild switch` (adds the adbusers group, abe's
#      membership, the udev rules, htop and kitty's terminfo);
#   3. only if the worker's tasks queue is idle and you agree: restarts abe's
#      systemd user manager so the tasks daemon gets the new group (lingering
#      brings it and tasks-daemon.service back; running jobs WOULD be killed,
#      hence the idle check);
#   4. verifies the group, the rules and the daemon's groups.
# Reversible: restore the .pre-test-phone backup and switch again.
set -eu

WORKER=${WORKER:-abe@192.168.1.7}
KEY=${KEY:-$HOME/.ssh/id_ed25519_nixos_worker}
here=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
module="$here/nix/taskworker-host.nix"

remote() { ssh -i "$KEY" -o BatchMode=yes "$WORKER" "$@"; }
remote_tty() { ssh -t -i "$KEY" "$WORKER" "$@"; }
ask() {
    printf '%s [y/N] ' "$1"
    read -r answer
    [ "$answer" = y ] || [ "$answer" = Y ]
}

verify() {
    echo "== checks on $WORKER"
    remote 'set -u
        getent group adbusers || echo "MISSING: adbusers group"
        id abe
        grep -h adbusers /etc/udev/rules.d/99-local.rules || echo "MISSING: adbusers udev rules"
        command -v htop || echo "MISSING: htop"
        infocmp xterm-kitty >/dev/null 2>&1 && echo "kitty terminfo: ok" || echo "MISSING: xterm-kitty terminfo"
        pid=$(systemctl --user show -p MainPID --value tasks-daemon.service)
        gid=$(getent group adbusers | cut -d: -f3)
        if [ "${pid:-0}" != 0 ] && [ -n "$gid" ]; then
            groups=$(sed -n "s/^Groups:[[:space:]]*//p" /proc/$pid/status)
            case " $groups " in
                *" $gid "*) echo "tasks-daemon (pid $pid) has adbusers ($gid)";;
                *) echo "tasks-daemon (pid $pid) LACKS adbusers ($gid): groups $groups; restart the user manager when idle";;
            esac
        else
            echo "tasks-daemon not running or adbusers missing"
        fi
        ls -l /dev/bus/usb/*/* | grep adbusers || echo "no adbusers USB node (expected until a phone is plugged in)"'
}

if [ "${1:-}" = --verify ]; then
    verify
    exit 0
fi

echo "== 1. staging $module on $WORKER"
scp -q -i "$KEY" -o BatchMode=yes "$module" "$WORKER:taskworker.nix.new"
remote 'diff -u /etc/nixos/taskworker.nix ~/taskworker.nix.new || true'
ask "Install it as /etc/nixos/taskworker.nix and run nixos-rebuild switch?" || exit 1

echo "== 2. install + switch (sudo asks for the worker password)"
remote_tty 'set -e
    sudo cp -p /etc/nixos/taskworker.nix /etc/nixos/taskworker.nix.pre-test-phone
    sudo install -m 0644 ~/taskworker.nix.new /etc/nixos/taskworker.nix
    sudo nixos-rebuild switch
    # A phone already plugged in keeps its old node permissions until re-evaluated.
    sudo udevadm trigger --subsystem-match=usb --action=change'

echo "== 3. tasks daemon group refresh"
active=$(remote '~/.local/bin/tasks list --active --json' |
    python3 -c 'import json,sys; d=json.load(sys.stdin); print(len(d.get("jobs", d) if isinstance(d, dict) else d))' 2>/dev/null || echo unknown)
echo "worker active jobs: $active"
if [ "$active" = 0 ] && ask "Restart abe's user manager on the worker now (stops/starts tasks-daemon and other user units)?"; then
    remote_tty 'sudo systemctl restart user@$(id -u).service'
    sleep 3
    remote 'systemctl --user is-active tasks-daemon.service; ~/.local/bin/tasks admission >/dev/null && echo "tasks admission: ok"'
else
    echo "Skipped. When the worker queue is idle: ssh -t $WORKER 'sudo systemctl restart user@\$(id -u).service'"
    echo "(or reboot the worker). Until then device jobs cannot open the phone's USB node."
fi

echo "== 4. verify"
verify
