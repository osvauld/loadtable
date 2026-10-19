#!/usr/bin/env python3
"""Idle-cost bench for a `tasks` binary (daemon CPU per request type and per idle minute).

Starts BIN as an isolated daemon (private TASK_MANAGER_STATE_DIR, TASKS_LOCK_DIR, HOME and
no peer config) over a synthetic ledger of N finished jobs, then measures, from
/proc/PID/stat, the daemon CPU spent:

- per request of each type sent straight to the socket (list, job, logs, status, ...),
  with the reply size;
- per minute while idle (no client at all);
- per minute under a modelled `tasks ui` (status every 2 s; the release window also polls
  `list` every 2.5 s; pass --ui-list to include that).

Nothing outside the private directory is touched and the daemon is stopped at the end.
Synthetic identities only. Linux only (uses /proc). Stdlib only.

    python3 -I scripts/idle_bench.py --bin target/debug/tasks --jobs 730 --json out.json
    python3 -I scripts/idle_bench.py ... --budget-idle-cpu-s-per-min 0.6 --budget-status-ms 25

With --budget-* options the script exits 1 when a measurement exceeds its budget.
"""
import argparse
import json
import os
import random
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time

PROTOCOL_VERSION = 2
TICK = os.sysconf("SC_CLK_TCK")


def seed(state, jobs, estimates_per_job=1.1, log_bytes=32 * 1024):
    """Synthetic ledger shaped like a real one: finished local commands with metrics."""
    os.makedirs(state, mode=0o700, exist_ok=True)
    db = sqlite3.connect(os.path.join(state, "tasks.sqlite"))
    db.executescript(
        """
        CREATE TABLE IF NOT EXISTS jobs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            spec TEXT NOT NULL, phase TEXT NOT NULL, submitted_at INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS runtimes (
            suite TEXT NOT NULL, configuration TEXT NOT NULL, stage TEXT NOT NULL,
            estimate TEXT NOT NULL, PRIMARY KEY(suite, configuration, stage));
        CREATE TABLE IF NOT EXISTS executions (
            job_id INTEGER PRIMARY KEY REFERENCES jobs(id), data TEXT NOT NULL);
        """
    )
    rng = random.Random(7)
    now = int(time.time())
    owners = [f"synthetic-agent-{i}" for i in range(12)]
    configs = []
    for i in range(jobs):
        argv = ["cargo", "test", "--locked", "-j", "2", "-p", f"crate{i % 40}", f"filter_{i}"]
        resource = "compile" if i % 3 else "run"
        configuration = json.dumps({"argv": argv, "resource": resource}, separators=(",", ":"))
        configs.append(configuration)
        spec = {
            "suite": "local.command",
            "owner": owners[i % len(owners)],
            "source": f"/tmp/synthetic-checkouts/repo-{i % 9}",
            "revision": "live-workspace",
            "configuration": configuration,
            "cost": "cpu_simulation",
        }
        phase = rng.choice(["passed"] * 6 + ["failed"] * 3 + ["cancelled"])
        at = now - (jobs - i) * 60
        execution = {
            "approved_at": at,
            "started_at": at,
            "finished_at": at + 50,
            "cancel_requested": False,
            "worker": {"pid": 100000 + i, "start_ticks": 7000000 + i},
            "supervisor": None,
            "snapshot": None,
            "stages": [
                {"stage": "build", "wait_seconds": 0.02, "seconds": 41.5, "exit_code": 0,
                 "signal": None},
                {"stage": "run", "wait_seconds": 0.01, "seconds": 8.25,
                 "exit_code": 0 if phase == "passed" else 101, "signal": None},
            ],
            "android_trial": False,
            "local_command": True,
            "detail": "Local command finished; inspect retained log",
            "metrics": {"warmth": "warm", "warmth_reason": "synthetic", "wall_s": 49.75,
                        "cpu_user_s": 80.5, "cpu_sys_s": 9.25, "peak_rss_bytes": 912345678,
                        "crates_compiled": 12},
        }
        db.execute("INSERT INTO jobs(id, spec, phase, submitted_at) VALUES (?,?,?,?)",
                   (i + 1, json.dumps(spec), phase, at))
        db.execute("INSERT INTO executions(job_id, data) VALUES (?,?)",
                   (i + 1, json.dumps(execution)))
    stages = ["build", "run", "worker.wait_build", "worker.wait_run"]
    for n in range(int(jobs * estimates_per_job)):
        estimate = {"mean": 40.0 + n % 17, "variance": 3.5, "samples": 1 + n % 4}
        db.execute("INSERT OR REPLACE INTO runtimes VALUES (?,?,?,?)",
                   ("local.command", configs[n % jobs], stages[n // jobs % 4],
                    json.dumps(estimate)))
    db.commit()
    db.close()
    # One retained log for the `logs` request.
    log_dir = os.path.join(state, "jobs", str(jobs))
    os.makedirs(log_dir, exist_ok=True)
    with open(os.path.join(log_dir, "job.log"), "w") as f:
        line = "   Compiling synthetic-crate v0.1.0 (/tmp/synthetic-checkouts/repo)\n"
        f.write(line * (log_bytes // len(line)))


def cpu_ticks(pid):
    with open(f"/proc/{pid}/stat") as f:
        fields = f.read().rsplit(")", 1)[1].split()
    return int(fields[11]), int(fields[12])  # utime, stime


def request(sock_path, action, timeout=30):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(timeout)
    s.connect(sock_path)
    s.sendall((json.dumps({"version": PROTOCOL_VERSION, "action": action}) + "\n").encode())
    chunks = []
    while True:
        b = s.recv(1 << 16)
        if not b:
            break
        chunks.append(b)
        if b.endswith(b"\n"):
            break
    s.close()
    return b"".join(chunks)


def measure(pid, fn, count):
    u0, s0 = cpu_ticks(pid)
    t0 = time.monotonic()
    size = 0
    for _ in range(count):
        size = fn()
    wall = time.monotonic() - t0
    u1, s1 = cpu_ticks(pid)
    cpu = (u1 - u0 + s1 - s0) / TICK
    return {"count": count, "cpu_ms_per_request": 1000 * cpu / count,
            "user_ms": 1000 * (u1 - u0) / TICK / count,
            "sys_ms": 1000 * (s1 - s0) / TICK / count,
            "reply_bytes": size, "wall_ms_per_request": 1000 * wall / count}


def per_minute(pid, seconds, load=None):
    u0, s0 = cpu_ticks(pid)
    deadline = time.monotonic() + seconds
    next_status = next_list = time.monotonic()
    requests = 0
    while time.monotonic() < deadline:
        now = time.monotonic()
        if load:
            if now >= next_status:
                load("status")
                requests += 1
                next_status += 2.0
            if "list" in load.kinds and now >= next_list:
                load("list")
                requests += 1
                next_list += 2.5
        time.sleep(0.05)
    u1, s1 = cpu_ticks(pid)
    cpu = (u1 - u0 + s1 - s0) / TICK
    return {"seconds": seconds, "requests": requests, "cpu_s_per_min": cpu * 60 / seconds,
            "core_pct": 100 * cpu / seconds}


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--bin", required=True)
    ap.add_argument("--jobs", type=int, default=730)
    ap.add_argument("--count", type=int, default=40, help="requests per type")
    ap.add_argument("--idle-seconds", type=float, default=60)
    ap.add_argument("--ui-seconds", type=float, default=60)
    ap.add_argument("--ui-list", action="store_true",
                    help="model the release window's extra list poll every 2.5 s")
    ap.add_argument("--json", help="write results here")
    ap.add_argument("--keep", action="store_true", help="keep the private directory")
    ap.add_argument("--budget-idle-cpu-s-per-min", type=float)
    ap.add_argument("--budget-ui-cpu-s-per-min", type=float)
    ap.add_argument("--budget-status-ms", type=float)
    ap.add_argument("--budget-list-active-ms", type=float)
    ap.add_argument("--budget-job-ms", type=float)
    args = ap.parse_args()

    root = tempfile.mkdtemp(prefix="tasks-idle-bench-")
    state, locks, home = (os.path.join(root, d) for d in ("state", "locks", "home"))
    for d in (locks, home):
        os.makedirs(d, mode=0o700)
    seed(state, args.jobs)
    env = {k: v for k, v in os.environ.items()
           if k in ("PATH", "LANG", "LC_ALL", "TZ")}
    env.update(TASK_MANAGER_STATE_DIR=state, TASKS_LOCK_DIR=locks, HOME=home,
               XDG_CONFIG_HOME=os.path.join(home, ".config"),
               XDG_STATE_HOME=os.path.join(home, ".state"),
               TASKS_PEER_CONFIG=os.path.join(home, "no-peer.toml"),
               TASKS_HEALTH_NOTIFY="off")
    log = open(os.path.join(root, "taskd.log"), "w")
    daemon = subprocess.Popen([args.bin, "daemon"], env=env, stdin=subprocess.DEVNULL,
                              stdout=log, stderr=log, start_new_session=True)
    sock = os.path.join(state, "taskd.sock")
    try:
        for _ in range(200):
            if os.path.exists(sock):
                try:
                    request(sock, {"type": "job", "data": 1})
                    break
                except OSError:
                    pass
            time.sleep(0.05)
        else:
            raise SystemExit("daemon did not come up; see " + root)
        time.sleep(3)  # let the sampler/health threads take their first samples
        pid = daemon.pid
        last = args.jobs
        actions = {
            "list": {"type": "list"},
            "list_active": {"type": "list_active"},
            "job": {"type": "job", "data": last},
            "logs": {"type": "logs", "data": last},
            "status": {"type": "status"},
        }
        results = {"bin": args.bin, "jobs": args.jobs, "requests": {}}
        for name, action in actions.items():
            try:
                if b"unknown variant" in request(sock, action):
                    results["requests"][name] = "unsupported"
                    continue
            except OSError as e:
                results["requests"][name] = f"error: {e}"
                continue
            results["requests"][name] = measure(
                pid, lambda a=action: len(request(sock, a)), args.count)
        results["idle"] = per_minute(pid, args.idle_seconds)

        class Load:
            def __init__(self, kinds):
                self.kinds = kinds

            def __call__(self, kind):
                request(sock, {"type": kind})

        results["ui_model"] = per_minute(
            pid, args.ui_seconds, Load({"status", "list"} if args.ui_list else {"status"}))
        results["ui_model"]["includes_list_poll"] = args.ui_list
        with open(f"/proc/{pid}/status") as f:
            results["daemon_rss_kb"] = int(
                next(l for l in f if l.startswith("VmRSS")).split()[1])
    finally:
        os.killpg(daemon.pid, signal.SIGTERM)
        try:
            daemon.wait(10)
        except subprocess.TimeoutExpired:
            os.killpg(daemon.pid, signal.SIGKILL)
        log.close()
    text = json.dumps(results, indent=2)
    print(text)
    if args.json:
        with open(args.json, "w") as f:
            f.write(text + "\n")
    if not args.keep:
        subprocess.run(["rm", "-rf", "--", root], check=False)
    failures = []

    def check(value, budget, what):
        if budget is not None and value is not None and value > budget:
            failures.append(f"{what}: {value:.2f} > budget {budget}")

    r = results["requests"]
    check(results["idle"]["cpu_s_per_min"], args.budget_idle_cpu_s_per_min, "idle cpu s/min")
    check(results["ui_model"]["cpu_s_per_min"], args.budget_ui_cpu_s_per_min, "ui cpu s/min")
    for key, budget in (("status", args.budget_status_ms),
                        ("list_active", args.budget_list_active_ms),
                        ("job", args.budget_job_ms)):
        if isinstance(r.get(key), dict):
            check(r[key]["cpu_ms_per_request"], budget, f"{key} ms/request")
        elif budget is not None:
            failures.append(f"{key}: {r.get(key)}")
    for f in failures:
        print("BUDGET EXCEEDED:", f, file=sys.stderr)
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
