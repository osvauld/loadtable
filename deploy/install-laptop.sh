#!/usr/bin/env bash
# Install a task-manager release on the laptop (kurukkan). Run by the USER, by hand:
#
#   deploy/install-laptop.sh COMMIT            # build, stage, quiesce, switch, restart, check
#   deploy/install-laptop.sh COMMIT --stage-only   # build + stage only; switch nothing
#   deploy/install-laptop.sh --restart-daemon      # stop a daemon not running the linked
#                                                  # release, autostart the linked one (rollback)
#
# What it does, in order (it stops at the first failure and switches nothing before step 5):
#   1. Exports COMMIT (git archive, no .git) to ~/build-cache/p2p_chat/tasks/build/COMMIT and
#      builds it THROUGH THE QUEUE: `tasks run -- cargo build --release --locked --offline -j 2`
#      (the current release admits it like any compile; nothing is replaced while it builds).
#   2. Stages the binary as a NEW immutable release ~/build-cache/p2p_chat/tasks/releases/<sha256>/
#      (the existing scheme; the running release directory is never touched), plus the offload
#      client scripts in ~/.local/share/tasks-offload/<short commit>/, and a release.json.
#   3. Prints the exact rollback command (the current ~/.local/bin/tasks link target and the
#      current tasks-offload wrapper are kept as they are; the wrapper is also saved).
#   4. Quiesces: `tasks drain` when the INSTALLED binary has it; otherwise (the 7df4fe15 release
#      has not) waits until `tasks status` shows "no live jobs" twice, 15 s apart.
#   5. Switches ~/.local/bin/tasks (atomic symlink rename) and the tasks-offload wrapper.
#   6. Stops the old on-demand daemon (SIGTERM to the process whose exe is the OLD release and
#      whose argv is `tasks daemon`); the next client autostarts the NEW one. Workers and job
#      process groups are separate and are not signalled.
#   7. `tasks start-worker` (records "execution enabled" so a reboot resumes the queue), then a
#      post-install check: release identity, `tasks doctor`, daemon autostart after reboot.
#
# Kept as-is: state dir ~/.local/state/tinline-task-manager-native (SQLite ledger, socket, job
# history), the lock files and their inodes in /tmp/claude-1000 (never created, moved or
# deleted here), old releases, Cargo targets and caches. No sudo, no systemd change.
set -eu

die() { echo "install-laptop: $*" >&2; exit 1; }
say() { echo "install-laptop: $*"; }

[ $# -ge 1 ] || die "usage: $0 COMMIT [--stage-only] | $0 --restart-daemon"
BIN_LINK=$HOME/.local/bin/tasks

# Stops every `tasks daemon` of this user whose executable is not the linked release, then
# autostarts the linked one. Workers and job groups are separate processes; not signalled.
restart_daemon() {
    want=$(readlink -f "$BIN_LINK")
    stale=""
    for d in /proc/[0-9]*; do
        [ -O "$d" ] || continue
        args=$(tr '\0' ' ' < "$d/cmdline" 2>/dev/null || true)
        case "$args" in
            *"/tasks daemon ") ;;
            *) continue ;;
        esac
        exe=$(readlink "$d/exe" 2>/dev/null || true)
        [ "$exe" = "$want" ] || stale="$stale ${d#/proc/}"
    done
    for pid in $stale; do
        say "stopping daemon pid $pid ($(readlink "/proc/$pid/exe" 2>/dev/null || echo '?')) with SIGTERM"
        kill -TERM "$pid" 2>/dev/null || true
    done
    for pid in $stale; do
        for _ in $(seq 1 60); do [ -e "/proc/$pid" ] || break; sleep 0.5; done
        [ ! -e "/proc/$pid" ] || die "daemon $pid did not exit"
    done
    "$BIN_LINK" list --active > /dev/null # autostarts the linked release's daemon
    say "daemon now runs $want"
}

if [ "$1" = "--restart-daemon" ]; then
    restart_daemon
    exit 0
fi
COMMIT_ARG=$1
STAGE_ONLY=0
[ "${2:-}" = "--stage-only" ] && STAGE_ONLY=1

REPO=$(cd "$(dirname "$0")/.." && git rev-parse --show-toplevel)
COMMIT=$(git -C "$REPO" rev-parse --verify "$COMMIT_ARG^{commit}") || die "unknown commit $COMMIT_ARG"
SHORT=$(git -C "$REPO" rev-parse --short=7 "$COMMIT")
OFFLOAD_WRAPPER=$HOME/.local/bin/tasks-offload
RELEASES=$HOME/build-cache/p2p_chat/tasks/releases
BUILD=$HOME/build-cache/p2p_chat/tasks/build/$COMMIT
OFFLOAD_DIR=$HOME/.local/share/tasks-offload/$SHORT
STATE=${XDG_STATE_HOME:-$HOME/.local/state}/tinline-task-manager-native
[ -z "${TASK_MANAGER_STATE_DIR:-}" ] || die "unset TASK_MANAGER_STATE_DIR (install for the shared state)"

CURRENT=$(readlink -f "$BIN_LINK") || die "$BIN_LINK is missing"
[ -x "$CURRENT" ] || die "current release $CURRENT is not executable"
say "current release: $CURRENT"
say "installing commit $COMMIT"

# ---- 1. build through the queue (current release admits it) --------------------------------
if [ -x "$BUILD/target/release/tasks" ]; then
    say "reusing build $BUILD/target/release/tasks"
else
    [ ! -e "$BUILD" ] || die "$BUILD exists without a binary; inspect it (nothing is deleted here)"
    mkdir -p "$BUILD"
    git -C "$REPO" archive -o "$BUILD.tar" "$COMMIT"
    tar -x -C "$BUILD" -f "$BUILD.tar"
    rm -f "$BUILD.tar"
    (cd "$BUILD" && "$BIN_LINK" run --who tasks-release -- \
        cargo build --release --locked --offline -j 2 --bin tasks) ||
        die "build failed; nothing was switched (log: tasks logs <job id> above)"
fi
NEW_BIN=$BUILD/target/release/tasks
[ -x "$NEW_BIN" ] || die "no built binary at $NEW_BIN"

# ---- 2. stage a new immutable release --------------------------------------------------------
SHA=$(sha256sum "$NEW_BIN" | cut -d' ' -f1)
DEST=$RELEASES/$SHA
if [ -e "$DEST/tasks" ]; then
    [ "$(sha256sum "$DEST/tasks" | cut -d' ' -f1)" = "$SHA" ] || die "$DEST/tasks exists with other contents"
    say "release $SHA already staged"
else
    mkdir -p "$DEST.tmp-$$"
    install -m 0755 "$NEW_BIN" "$DEST.tmp-$$/tasks"
    printf '{"commit": "%s", "sha256": "%s", "built": "%s", "features": "default"}\n' \
        "$COMMIT" "$SHA" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$DEST.tmp-$$/release.json"
    mv -T "$DEST.tmp-$$" "$DEST"
fi
[ "$DEST/tasks" != "$CURRENT" ] || die "commit $SHORT is already the installed release"
if [ ! -e "$OFFLOAD_DIR" ]; then
    mkdir -p "$OFFLOAD_DIR.tmp-$$"
    for f in tasks-offload tasks_offload.py offload_agent.py worker_source.py \
             status-watch-ssh status_watch_ssh.py offload-portable-desktop.sh README-offload.md; do
        git -C "$REPO" show "$COMMIT:scripts/$f" > "$OFFLOAD_DIR.tmp-$$/$f"
    done
    chmod 0755 "$OFFLOAD_DIR.tmp-$$/tasks-offload" "$OFFLOAD_DIR.tmp-$$/status-watch-ssh" \
        "$OFFLOAD_DIR.tmp-$$/status_watch_ssh.py" "$OFFLOAD_DIR.tmp-$$/offload-portable-desktop.sh"
    mv -T "$OFFLOAD_DIR.tmp-$$" "$OFFLOAD_DIR"
fi
say "staged $DEST/tasks and $OFFLOAD_DIR"

# ---- 3. rollback -------------------------------------------------------------------------------
SAVED_WRAPPER=$RELEASES/tasks-offload.wrapper.before-$SHORT
[ -e "$SAVED_WRAPPER" ] || cp -p "$OFFLOAD_WRAPPER" "$SAVED_WRAPPER"
SELF=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")
ROLLBACK="'$BIN_LINK' drain --timeout 3600 && ln -sfn '$CURRENT' '$BIN_LINK.rollback' && mv -T '$BIN_LINK.rollback' '$BIN_LINK' && cp -p '$SAVED_WRAPPER' '$OFFLOAD_WRAPPER' && '$SELF' --restart-daemon"
say "ROLLBACK (one command, after this install): $ROLLBACK"
if [ "$STAGE_ONLY" = 1 ]; then
    say "--stage-only: nothing switched"
    exit 0
fi

# ---- 4. quiesce --------------------------------------------------------------------------------
probe=$("$BIN_LINK" --usage-probe 2>&1 || true)
case "$probe" in
    *"drain [--timeout"*)
        say "installed release has tasks drain: pausing claims until the daemon restarts"
        "$BIN_LINK" drain --timeout 3600 || die "drain did not finish; nothing switched (tasks drain --cancel resumes)"
        ;;
    *)
        say "installed release has no tasks drain: waiting for 'no live jobs' (twice, 15 s apart)"
        say "  (no new jobs should be started meanwhile: this release cannot pause admission)"
        seen=0
        while [ "$seen" -lt 2 ]; do
            status=$("$BIN_LINK" status 2>&1 || true)
            case "$status" in
                *"no live jobs"*) seen=$((seen + 1)) ;;
                *) seen=0 ;;
            esac
            [ "$seen" -ge 2 ] || sleep 15
        done
        ;;
esac

# ---- 5. switch ---------------------------------------------------------------------------------
ln -sfn "$DEST/tasks" "$BIN_LINK.new"
mv -T "$BIN_LINK.new" "$BIN_LINK"
printf '#!/bin/sh\nexec %s/tasks-offload "$@"\n' "$OFFLOAD_DIR" > "$OFFLOAD_WRAPPER.new"
chmod 0755 "$OFFLOAD_WRAPPER.new"
mv -T "$OFFLOAD_WRAPPER.new" "$OFFLOAD_WRAPPER"
say "switched $BIN_LINK -> $DEST/tasks"

# ---- 6. restart the on-demand daemon -----------------------------------------------------------
restart_daemon || die "new binary is installed but the old daemon still runs; rollback: $ROLLBACK"

# ---- 7. resume + post-install check ------------------------------------------------------------
"$BIN_LINK" start-worker
echo "=== post-install check ==="
say "release: $(readlink -f "$BIN_LINK") (commit $COMMIT, sha256 $SHA)"
cat "$DEST/release.json"
"$BIN_LINK" doctor || say "tasks doctor reported problems (above)"
if systemctl --user is-enabled tasks-daemon.service > /dev/null 2>&1; then
    say "daemon after reboot: systemd user unit tasks-daemon.service is enabled"
else
    say "daemon after reboot: NOT started automatically. There is no systemd user unit on this host;"
    say "  the daemon starts on the first tasks command after a reboot, and only then resumes the queue."
    say "  Proposed fix (not applied): install a user unit like deploy/worker/tasks-daemon.service with"
    say "  ExecStart=$BIN_LINK daemon and this host's thresholds, then"
    say "  'systemctl --user daemon-reload && systemctl --user enable tasks-daemon' (start it only when idle)."
fi
say "rollback, if needed: $ROLLBACK"
