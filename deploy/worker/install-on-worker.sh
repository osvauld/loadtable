#!/usr/bin/env bash
# Runs ON the NixOS worker, from an exported source tree of the release being installed
# (deploy/install-worker.sh on the laptop exports it and calls this). No sudo; user unit only.
#
#   install-on-worker.sh build    COMMIT   # headless release build through the worker's queue
#   install-on-worker.sh stage    COMMIT   # new immutable release dir; keeps the current binary
#   install-on-worker.sh quiesce           # tasks drain if installed binary has it, else wait
#   install-on-worker.sh switch   COMMIT   # ~/.local/bin/tasks -> new release; restart user unit
#   install-on-worker.sh check    COMMIT   # identity, tasks doctor, LIBCLANG_PATH of the daemon
#   install-on-worker.sh rollback          # quiesce, previous release back, restart user unit
#
# Kept: ~/.local/state/tinline-task-manager-native (SQLite, socket, history), /tmp/claude-1000
# lock files and inodes, the unit file (EnvironmentFile=-/etc/taskworker/bindgen.env, JAVA_HOME,
# thresholds) and ~/.local/bin/lan-peer. Nothing is deleted.
set -eu

die() { echo "install-on-worker: $*" >&2; exit 1; }
say() { echo "install-on-worker: $*"; }

BIN=$HOME/.local/bin/tasks
RELEASES=$HOME/.local/share/tasks/releases
PREVIOUS=$RELEASES/previous     # symlink to the release that was installed before the switch
UNIT=tasks-daemon.service
SRC=$(cd "$(dirname "$0")/../.." && pwd)
step=${1:-}
commit=${2:-}

need_commit() { [ -n "$commit" ] || die "$step needs COMMIT"; }

unit_ok() {
    text=$(systemctl --user cat "$UNIT" 2>&1) || die "no user unit $UNIT: $text"
    case "$text" in *"EnvironmentFile=-/etc/taskworker/bindgen.env"*) ;; *) die "$UNIT lacks EnvironmentFile=-/etc/taskworker/bindgen.env" ;; esac
    case "$text" in *"JAVA_HOME="*) ;; *) die "$UNIT lacks JAVA_HOME" ;; esac
    case "$text" in *"ExecStart=%h/.local/bin/tasks daemon"* | *"ExecStart=$HOME/.local/bin/tasks daemon"*) ;;
        *) die "$UNIT does not run ~/.local/bin/tasks daemon" ;; esac
}

quiesce() {
    probe=$("$BIN" --usage-probe 2>&1 || true)
    case "$probe" in
        *"drain [--timeout"*)
            say "installed release has tasks drain"
            "$BIN" drain --timeout 3600 || die "drain did not finish; nothing switched (tasks drain --cancel resumes)"
            ;;
        *)
            say "installed release has no tasks drain: waiting for 'no live jobs' (twice, 15 s apart);"
            say "  this release cannot pause admission, so avoid submitting meanwhile"
            seen=0
            while [ "$seen" -lt 2 ]; do
                status=$("$BIN" status 2>&1 || true)
                case "$status" in
                    *"no live jobs"*) seen=$((seen + 1)) ;;
                    *) seen=0 ;;
                esac
                [ "$seen" -ge 2 ] || sleep 15
            done
            ;;
    esac
}

restart_unit() {
    systemctl --user restart "$UNIT"
    for _ in $(seq 1 40); do
        [ -S "$HOME/.local/state/tinline-task-manager-native/taskd.sock" ] && "$BIN" list --active > /dev/null 2>&1 && return 0
        sleep 0.5
    done
    die "daemon did not answer after restart (systemctl --user status $UNIT)"
}

case "$step" in
build)
    need_commit
    [ -x "$SRC/target/release/tasks" ] && { say "reusing $SRC/target/release/tasks"; exit 0; }
    (cd "$SRC" && "$BIN" run --who tasks-release -- \
        cargo build --release --locked -j 6 --no-default-features --bin tasks) ||
        die "build failed; nothing staged or switched"
    ;;
stage)
    need_commit
    new=$SRC/target/release/tasks
    [ -x "$new" ] || die "no build at $new (run build first)"
    sha=$(sha256sum "$new" | cut -d' ' -f1)
    mkdir -p "$RELEASES"
    if [ ! -e "$RELEASES/$sha/tasks" ]; then
        mkdir -p "$RELEASES/$sha.tmp-$$"
        install -m 0755 "$new" "$RELEASES/$sha.tmp-$$/tasks"
        printf '{"commit": "%s", "sha256": "%s", "built": "%s", "features": "headless (--no-default-features)"}\n' \
            "$commit" "$sha" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$RELEASES/$sha.tmp-$$/release.json"
        mv -T "$RELEASES/$sha.tmp-$$" "$RELEASES/$sha"
    fi
    # Keep the running binary as a release of its own (today ~/.local/bin/tasks is a plain file).
    cur=$(readlink -f "$BIN")
    cur_sha=$(sha256sum "$cur" | cut -d' ' -f1)
    [ "$cur_sha" != "$sha" ] || die "commit $commit is already installed"
    if [ ! -e "$RELEASES/$cur_sha/tasks" ]; then
        mkdir -p "$RELEASES/$cur_sha"
        install -m 0755 "$cur" "$RELEASES/$cur_sha/tasks"
    fi
    say "staged $RELEASES/$sha/tasks; current kept as $RELEASES/$cur_sha/tasks"
    echo "STAGED $sha $cur_sha"
    ;;
quiesce)
    quiesce
    ;;
switch)
    need_commit
    unit_ok
    sha=$(sha256sum "$SRC/target/release/tasks" | cut -d' ' -f1)
    new=$RELEASES/$sha/tasks
    [ -x "$new" ] || die "not staged: $new"
    cur_sha=$(sha256sum "$(readlink -f "$BIN")" | cut -d' ' -f1)
    [ -x "$RELEASES/$cur_sha/tasks" ] || die "current binary not preserved under $RELEASES"
    ln -sfn "$RELEASES/$cur_sha" "$PREVIOUS.new" && mv -T "$PREVIOUS.new" "$PREVIOUS"
    ln -sfn "$new" "$BIN.new"
    mv -T "$BIN.new" "$BIN"
    say "switched $BIN -> $new (previous: $RELEASES/$cur_sha/tasks)"
    restart_unit
    "$BIN" start-worker
    ;;
check)
    need_commit
    echo "=== post-install check (worker) ==="
    say "binary: $(readlink -f "$BIN")"
    cat "$(dirname "$(readlink -f "$BIN")")/release.json" 2>/dev/null || say "no release.json"
    "$BIN" doctor || say "tasks doctor reported problems (above)"
    pid=$(systemctl --user show -p MainPID --value "$UNIT")
    env=$(tr '\0' '\n' < "/proc/$pid/environ" 2>/dev/null || true)
    found=""
    while IFS= read -r line; do
        case "$line" in LIBCLANG_PATH=*) found=$line ;; esac
    done <<< "$env"
    if [ -n "$found" ]; then
        say "daemon (pid $pid) $found"
    else
        say "WARNING: daemon (pid $pid) has no LIBCLANG_PATH (EnvironmentFile /etc/taskworker/bindgen.env missing?)"
    fi
    ;;
rollback)
    [ -x "$PREVIOUS/tasks" ] || die "no previous release recorded at $PREVIOUS"
    unit_ok
    quiesce
    ln -sfn "$(readlink -f "$PREVIOUS/tasks")" "$BIN.new"
    mv -T "$BIN.new" "$BIN"
    say "rolled back $BIN -> $(readlink -f "$BIN")"
    restart_unit
    "$BIN" start-worker > /dev/null
    ;;
*)
    die "usage: $0 build|stage|switch|check COMMIT | quiesce | rollback"
    ;;
esac
