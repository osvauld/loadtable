#!/usr/bin/env bash
# LAN end-to-end check of daemon-to-daemon forwarding (docs/design/peer-link.md), laptop side.
# OPERATOR-RUN: it starts an ISOLATED laptop daemon (own state dir and peer config, the real
# admission lock dir) with the given `tasks` binary, and talks to a peer-capable `tasks daemon`
# that the operator started on the worker (steps below). It never ssh-es anywhere, never
# installs or replaces a binary and never touches the shared laptop daemon.
#
#   scripts/peer-lan-e2e.sh init                # once: laptop key + config, prints endpoint id
#   WORKER_ID=<worker endpoint id> WORKER_ADDR=192.168.1.7:7842 scripts/peer-lan-e2e.sh run
#   E2E_LINK_LOSS=1 ... run                     # also the manual link-loss/re-attach step
#
# Environment: TASKS_BIN (default: this checkout's target/debug/tasks), LAPTOP_IP (default: the
# source address of the route to the worker), E2E_DIR (default ~/build-cache/peer-e2e/laptop),
# E2E_WHO (default peer-e2e).
#
# Worker steps (the operator runs these; they need the worker, not root, except the firewall):
#  1. Build `tasks` ON the worker from this branch, e.g. from the laptop checkout through the
#     existing offload path (NOT a peer forward):
#       tasks run --offload --who peer-e2e -- tasks-offload --worker abe@192.168.1.7 \
#         --ssh-key ~/.ssh/id_ed25519_nixos_worker --who peer-e2e -- \
#         cargo build --offline --locked -j 8 --bin tasks
#     (add --cargo-fetch once if the worker's crate cache lacks iroh 1.3 / iroh-blobs 0.103.1).
#     The summary's remote_dir is the worker checkout; then on the worker:
#       mkdir -p ~/peer-e2e && cp <remote_dir>/target/debug/tasks ~/peer-e2e/tasks
#  2. On the worker: config + key, then allow the laptop as a host:
#       ~/peer-e2e/tasks peer init --bind 192.168.1.7:7842 --config ~/peer-e2e/peer.toml
#       cat >> ~/peer-e2e/peer.toml <<EOF
#       [[peer]]
#       name = "laptop"
#       endpoint_id = "<laptop id printed by: scripts/peer-lan-e2e.sh init>"
#       role = "host"
#       EOF
#     and add `checkouts = "~/peer-e2e/checkouts"` ABOVE the [[peer]] table to keep the test
#     away from ~/offload/checkouts.
#  3. Free UDP 7842 (the only UDP port nix/taskworker-host.nix opens) and start an isolated
#     worker daemon (shared admission lock dir, own state):
#       systemctl --user stop lan-peer.service
#       systemd-run --user --unit tasks-peer-e2e \
#         -E PATH=/run/current-system/sw/bin:$HOME/.nix-profile/bin \
#         -E TASK_MANAGER_STATE_DIR=$HOME/peer-e2e/state \
#         -E TASKS_PEER_CONFIG=$HOME/peer-e2e/peer.toml \
#         -E TASKS_MIN_AVAILABLE_MEMORY_MIB=3072 \
#         ~/peer-e2e/tasks daemon
#       TASK_MANAGER_STATE_DIR=~/peer-e2e/state ~/peer-e2e/tasks peer list   # enabled, endpoint
#  4. Run this script on the laptop. For E2E_LINK_LOSS=1, when it asks:
#       systemctl --user stop tasks-peer-e2e        # the worker's own worker keeps the job
#       (wait for the UNCERTAIN line here), then start it again with the same systemd-run line.
#  5. Afterwards: systemctl --user stop tasks-peer-e2e; systemctl --user start lan-peer.service
#     (worker), and stop nothing on the laptop: this script stops its own daemon.
set -euo pipefail

here=$(cd "$(dirname "$0")/.." && pwd)
TASKS_BIN=${TASKS_BIN:-$here/target/debug/tasks}
E2E_DIR=${E2E_DIR:-$HOME/build-cache/peer-e2e/laptop}
E2E_WHO=${E2E_WHO:-peer-e2e}
export TASK_MANAGER_STATE_DIR=$E2E_DIR/state
export TASKS_PEER_CONFIG=$E2E_DIR/peer.toml

die() { echo "peer-lan-e2e: $*" >&2; exit 2; }
say() { echo "peer-lan-e2e: $*" >&2; }
[ -x "$TASKS_BIN" ] || die "no tasks binary at $TASKS_BIN (set TASKS_BIN)"

laptop_ip() {
    local host=${WORKER_ADDR%:*}
    ip -4 route get "$host" 2>/dev/null | sed -n 's/.* src \([0-9.]*\).*/\1/p' | head -1
}

cmd_init() {
    mkdir -p "$E2E_DIR"
    local ip=${LAPTOP_IP:-}
    if [ -z "$ip" ]; then
        WORKER_ADDR=${WORKER_ADDR:-192.168.1.7:7842}
        ip=$(laptop_ip)
    fi
    [ -n "$ip" ] || die "cannot find the laptop's LAN address; set LAPTOP_IP"
    if [ ! -e "$TASKS_PEER_CONFIG" ]; then
        "$TASKS_BIN" peer init --bind "$ip:0" --config "$TASKS_PEER_CONFIG" >/dev/null
    fi
    say "laptop endpoint id (put it in the worker's [[peer]] laptop entry):"
    "$TASKS_BIN" peer init --config "$TASKS_PEER_CONFIG" 2>/dev/null
}

daemon_pid=
stop_daemon() {
    if [ -n "$daemon_pid" ] && kill -0 "$daemon_pid" 2>/dev/null; then
        kill "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
}

results=()
pass() { results+=("PASS $1"); say "PASS $1"; }
fail() { results+=("FAIL $1: $2"); say "FAIL $1: $2"; }

link_field() { # JOB FIELD
    python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1])).get(sys.argv[2]))' \
        "$TASK_MANAGER_STATE_DIR/peer/links/$1.json" "$2" 2>/dev/null || echo None
}

forward() { # LOG_PREFIX -- tasks run args...
    local prefix=$1; shift
    set +e
    (cd "$E2E_DIR/repo" && "$TASKS_BIN" run --who "$E2E_WHO" --peer worker "$@") \
        >"$E2E_DIR/$prefix.out" 2>"$E2E_DIR/$prefix.err"
    local code=$?
    set -e
    job=$(sed -n 's/^tasks: submitted job \([0-9]*\);.*/\1/p' "$E2E_DIR/$prefix.err" | head -1)
    return $code
}

cmd_run() {
    [ -n "${WORKER_ID:-}" ] || die "WORKER_ID is required (the worker's tasks peer id)"
    [ -n "${WORKER_ADDR:-}" ] || die "WORKER_ADDR is required (e.g. 192.168.1.7:7842)"
    [ -e "$TASKS_PEER_CONFIG" ] || die "run '$0 init' first"
    if ! grep -q "^name = \"worker\"" "$TASKS_PEER_CONFIG"; then
        cat >>"$TASKS_PEER_CONFIG" <<EOF

[[peer]]
name = "worker"
endpoint_id = "$WORKER_ID"
addr = "$WORKER_ADDR"
role = "worker"
EOF
    fi
    mkdir -p "$TASK_MANAGER_STATE_DIR"
    if [ -S "$TASK_MANAGER_STATE_DIR/taskd.sock" ] && "$TASKS_BIN" peer list >/dev/null 2>&1; then
        die "an E2E laptop daemon is already running for $TASK_MANAGER_STATE_DIR; stop it first"
    fi
    "$TASKS_BIN" daemon 2>>"$E2E_DIR/daemon.log" &
    daemon_pid=$!
    trap stop_daemon EXIT
    for _ in $(seq 100); do
        "$TASKS_BIN" peer list >/dev/null 2>&1 && break
        sleep 0.1
    done
    "$TASKS_BIN" peer list || die "laptop E2E daemon did not start (see $E2E_DIR/daemon.log)"

    # Scratch checkout (synthetic content only).
    if [ ! -d "$E2E_DIR/repo/.git" ]; then
        mkdir -p "$E2E_DIR/repo/src"
        git -C "$E2E_DIR/repo" init -q
        printf 'alpha\n' >"$E2E_DIR/repo/src/a.txt"
        printf 'bravo\n' >"$E2E_DIR/repo/src/b.txt"
        printf 'SYNTHETIC=1\n' >"$E2E_DIR/repo/.env.local"
    fi

    # 1. Status stream from the worker.
    if line=$(timeout 30 "$TASKS_BIN" peer status-watch worker | head -1) &&
        python3 -I -c 'import json,sys; d=json.loads(sys.argv[1]); assert d["version"]==1 and d["machine"]["online"] is True and d["stream"]["transport"]=="iroh"' "$line"; then
        pass "status stream"
    else
        fail "status stream" "${line:-no line}"
    fi

    # 2. Passing job: logs here, exit 0.
    if forward pass --run -- sh -c 'echo hello-from-worker; uname -n; cat src/a.txt' &&
        grep -q '^hello-from-worker$' "$E2E_DIR/pass.out"; then
        pass "forward exit 0 + log (local job $job, remote $(link_field "$job" remote_job_id))"
    else
        fail "forward exit 0" "see $E2E_DIR/pass.err"
    fi
    # 3. Failing job: the worker's exit code.
    set +e; forward fail --run -- sh -c 'echo failing; exit 3'; code=$?; set -e
    [ "$code" = 3 ] && pass "forward exit 3" || fail "forward exit 3" "got $code"
    # 4./5. Content-addressed sync: unchanged -> 0 blobs; one change -> 1 blob.
    forward again --run -- true || true
    [ "$(link_field "$job" blobs_sent)" = 0 ] && pass "resync sends 0 blobs" ||
        fail "resync" "blobs_sent=$(link_field "$job" blobs_sent)"
    printf 'bravo %s\n' "$(date +%s)" >"$E2E_DIR/repo/src/b.txt"
    forward changed --run -- true || true
    [ "$(link_field "$job" blobs_sent)" = 1 ] && pass "one change sends 1 blob" ||
        fail "changed" "blobs_sent=$(link_field "$job" blobs_sent)"
    # 6. Cancel here -> the worker job is cancelled.
    (cd "$E2E_DIR/repo" && "$TASKS_BIN" run --who "$E2E_WHO" --peer worker --run -- sh -c 'echo waiting; exec sleep 300') \
        >"$E2E_DIR/cancel.out" 2>"$E2E_DIR/cancel.err" &
    client=$!
    for _ in $(seq 600); do
        job=$(sed -n 's/^tasks: submitted job \([0-9]*\);.*/\1/p' "$E2E_DIR/cancel.err" | head -1)
        [ -n "$job" ] && [ "$(link_field "$job" remote_job_id)" != None ] && break
        sleep 0.2
    done
    sleep 2
    "$TASKS_BIN" cancel "$job" >/dev/null || true
    wait "$client" || true
    for _ in $(seq 100); do
        [ "$(link_field "$job" remote_phase)" = cancelled ] && break
        sleep 0.2
    done
    [ "$(link_field "$job" remote_phase)" = cancelled ] &&
        pass "cancel here cancels worker job $(link_field "$job" remote_job_id)" ||
        fail "cancel" "remote_phase=$(link_field "$job" remote_phase)"
    # 7. The personal phone is refused before anything is forwarded.
    if forward deny --device ZD22257P6G -- true; then
        fail "denylist" "ZD22257P6G was accepted"
    else
        grep -q ZD22257P6G "$E2E_DIR/deny.err" && pass "denylisted device refused" ||
            fail "denylist" "see $E2E_DIR/deny.err"
    fi
    # 8. Optional: link loss -> UNCERTAIN -> re-attach.
    if [ "${E2E_LINK_LOSS:-}" = 1 ]; then
        (cd "$E2E_DIR/repo" && "$TASKS_BIN" run --who "$E2E_WHO" --peer worker --run -- sh -c 'echo begin; sleep 90; echo end') \
            >"$E2E_DIR/loss.out" 2>"$E2E_DIR/loss.err" &
        client=$!
        say "NOW on the worker: systemctl --user stop tasks-peer-e2e   (waiting for UNCERTAIN...)"
        for _ in $(seq 900); do
            job=$(sed -n 's/^tasks: submitted job \([0-9]*\);.*/\1/p' "$E2E_DIR/loss.err" | head -1)
            [ -n "$job" ] && [ "$(link_field "$job" remote_state)" = uncertain ] && break
            sleep 0.2
        done
        say "UNCERTAIN seen; NOW start the worker daemon again (same systemd-run line)"
        if wait "$client" && [ "$(grep -c '^end$' "$E2E_DIR/loss.out")" = 1 ] &&
            [ "$(grep -c '^begin$' "$E2E_DIR/loss.out")" = 1 ]; then
            pass "link loss -> UNCERTAIN -> re-attached, log complete once"
        else
            fail "link loss" "see $E2E_DIR/loss.err"
        fi
    fi
    printf '%s\n' "${results[@]}"
    printf '%s\n' "${results[@]}" | grep -q '^FAIL' && exit 1
    exit 0
}

case "${1:-}" in
init) cmd_init ;;
run) cmd_run ;;
*) die "usage: $0 init | run   (see the header)" ;;
esac
