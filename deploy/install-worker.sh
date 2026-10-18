#!/usr/bin/env bash
# Install a task-manager release on the NixOS worker, run by the USER on the laptop:
#
#   deploy/install-worker.sh COMMIT              # export, build, stage, quiesce, switch, check
#   deploy/install-worker.sh COMMIT --stage-only # export, build, stage; switch nothing
#   deploy/install-worker.sh --rollback          # previous release back (quiesces first)
#
# Source goes to the worker as `git archive COMMIT` in ~/build-cache/tasks-release/COMMIT/ (a
# new directory each commit); the build is headless (`--no-default-features`, -j 6) and runs
# through the worker's own queue. The worker steps are deploy/worker/install-on-worker.sh from
# that export: new immutable release under ~/.local/share/tasks/releases/<sha256>/, the
# current binary kept beside it, ~/.local/bin/tasks switched by an atomic symlink rename,
# then `systemctl --user restart tasks-daemon` (user unit, no sudo; the unit is checked for
# EnvironmentFile=-/etc/taskworker/bindgen.env and JAVA_HOME and is not edited).
set -eu

WORKER=${TASKS_WORKER:-abe@192.168.1.7}
KEY=${TASKS_WORKER_KEY:-$HOME/.ssh/id_ed25519_nixos_worker}
SSH=(ssh -i "$KEY" -o BatchMode=yes -o IdentitiesOnly=yes -o ConnectTimeout=15 "$WORKER")

die() { echo "install-worker: $*" >&2; exit 1; }
say() { echo "install-worker: $*"; }

[ $# -ge 1 ] || die "usage: $0 COMMIT [--stage-only] | $0 --rollback"
if [ "$1" = "--rollback" ]; then
    script=$("${SSH[@]}" 'ls -d ~/build-cache/tasks-release/*/deploy/worker/install-on-worker.sh 2>/dev/null | tail -1') ||
        die "cannot reach $WORKER"
    [ -n "$script" ] || die "no exported release with install-on-worker.sh on the worker"
    exec "${SSH[@]}" "bash '$script' rollback"
fi

REPO=$(cd "$(dirname "$0")/.." && git rev-parse --show-toplevel)
COMMIT=$(git -C "$REPO" rev-parse --verify "$1^{commit}") || die "unknown commit $1"
STAGE_ONLY=0
[ "${2:-}" = "--stage-only" ] && STAGE_ONLY=1
REMOTE_DIR="build-cache/tasks-release/$COMMIT"
REMOTE="bash ~/$REMOTE_DIR/deploy/worker/install-on-worker.sh"

tarball=$(mktemp "${TMPDIR:-/tmp}/tasks-release-XXXXXX.tar")
trap 'rm -f "$tarball"' EXIT
git -C "$REPO" archive -o "$tarball" "$COMMIT"
"${SSH[@]}" "mkdir -p ~/$REMOTE_DIR && tar -x -C ~/$REMOTE_DIR" < "$tarball"
say "exported $COMMIT to $WORKER:~/$REMOTE_DIR"

"${SSH[@]}" "$REMOTE build $COMMIT"
staged=$("${SSH[@]}" "$REMOTE stage $COMMIT")
echo "$staged"
line=$(printf '%s\n' "$staged" | tail -1)
case "$line" in
    "STAGED "*) ;;
    *) die "stage did not report its result" ;;
esac
set -- $line
NEW_SHA=$2
OLD_SHA=$3
say "ROLLBACK (one command): $0 --rollback"
say "  (= on the worker: quiesce, ~/.local/bin/tasks -> ~/.local/share/tasks/releases/$OLD_SHA/tasks, systemctl --user restart tasks-daemon)"
if [ "$STAGE_ONLY" = 1 ]; then
    say "--stage-only: nothing switched (new release $NEW_SHA)"
    exit 0
fi
"${SSH[@]}" "$REMOTE quiesce"
"${SSH[@]}" "$REMOTE switch $COMMIT"
"${SSH[@]}" "$REMOTE check $COMMIT"
say "installed $COMMIT on $WORKER (sha256 $NEW_SHA); rollback: $0 --rollback"
