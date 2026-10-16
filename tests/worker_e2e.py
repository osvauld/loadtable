#!/usr/bin/env python3
"""Real admission integration, opt-in: run directly, NEVER inside buildlock.
Each worker stage acquires the real queue itself. Fixtures use no git commits,
no phone/network, and a tiny no-dependency Rust workspace. Artifacts/logs are
retained in a unique temp directory. Only processes belonging to this fixture
are stopped. A slow admission wait is not restarted as a duplicate job.
"""
import argparse
import fcntl
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time
from wait_budget import WaitBudget


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('binary', type=Path)
    ap.add_argument('--timeout', type=float, default=300, help='observed execution seconds, excluding admission')
    ap.add_argument('--queue-timeout', type=float, default=0, help='separate admission limit; 0 waits indefinitely')
    args = ap.parse_args()
    WaitBudget(args.timeout, args.queue_timeout)  # Validate before creating fixtures.
    binary = args.binary.resolve()
    # Build artifacts need the worker's 20 GiB reserve. /tmp is a smaller tmpfs
    # on this machine, so put only this fixture's owned data on the cache disk.
    fixtures = Path.home() / 'build-cache/tm-fixtures'
    fixtures.mkdir(mode=0o700, parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix='task-worker-e2e-', dir=fixtures))
    state = root / 'state'
    state.mkdir(mode=0o700)
    env = dict(os.environ, TASK_MANAGER_STATE_DIR=str(state), TASK_MANAGER_TARGET_DIR=str(root / 'target'))
    owned = []
    owned_notes = {}
    revision = 'a' * 40
    source = root / 'source'
    source.mkdir()
    subprocess.run(['git', 'init', '-q', str(source)], check=True)
    # Synthetic object ID via refs, matching snapshot API tests: no commits made.
    (source / '.git/HEAD').write_text(revision + '\n')
    workspace = source / 'tools/task-manager'
    (workspace / 'src').mkdir(parents=True)
    (workspace / 'tests').mkdir()
    (workspace / 'Cargo.toml').write_text('[package]\nname="worker-fixture"\nversion="0.1.0"\nedition="2024"\n[workspace]\n')
    (workspace / 'Cargo.lock').write_text('version = 4\n[[package]]\nname = "worker-fixture"\nversion = "0.1.0"\n')
    (workspace / 'tests/smoke.rs').write_text('#[test] fn integration_passes() { assert_eq!(2+2,4); }\n')

    def daemon():
        log = (state / 'fixture-daemon.log').open('ab')
        process = subprocess.Popen([str(binary), 'daemon'], env=env, stdout=log, stderr=log)
        log.close()
        owned.append(process)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            try:
                with socket.socket(socket.AF_UNIX) as client:
                    client.connect(str(state / 'taskd.sock'))
                return process
            except OSError:
                assert process.poll() is None, 'daemon exited'
                time.sleep(.05)
        raise AssertionError('daemon startup timeout')

    def action(kind, data=None):
        request = {'version': 2, 'action': {'type': kind}}
        if data is not None:
            request['action']['data'] = data
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(8)
            client.connect(str(state / 'taskd.sock'))
            client.sendall(json.dumps(request).encode() + b'\n')
            file = client.makefile('rb')
            response = json.loads(file.readline())
        assert response['error'] is None, response
        return response

    def submit(approve=True):
        spec = dict(suite='task-manager.tests', owner='fixture-agent', source=str(source),
                    revision=revision, configuration='debug/offline', cost='quick')
        job = action('submit', spec)['jobs'][-1]
        if approve:
            action('approve', job['id'])
        return job['id']

    def job(id):
        return next(j for j in action('list')['jobs'] if j['id'] == id)

    def target_plan():
        response = subprocess.run([str(binary), 'cache-plan'], env=env, capture_output=True, text=True, timeout=15)
        assert response.returncode == 0, response.stderr
        plan = json.loads(response.stdout)
        assert plan['dry_run'] and not plan['deletion_enabled']
        return next(t for t in plan['targets'] if t['path'] == str(root / 'target'))

    def worker(environment=None):
        log = (state / 'fixture-worker.log').open('ab')
        process = subprocess.Popen([str(binary), 'worker'], env=environment or env, stdout=log, stderr=log)
        log.close()
        owned.append(process)
        return process

    def wait(id, predicate, timeout=None):
        budget = WaitBudget(timeout if timeout is not None else args.timeout, args.queue_timeout)
        last = None
        previous = None
        while True:
            last = job(id)
            expired = budget.update(last['phase'])
            if last['phase'] != previous:
                print(f'job {id}: {last["phase"]} (queued {budget.queue:.1f}s, executing {budget.execution:.1f}s)', flush=True)
                previous = last['phase']
            if expired:
                raise AssertionError(f'{expired}; retained at {root}; do not launch duplicate: {last}')
            if predicate(last):
                return last
            if last['phase'] in ('failed', 'interrupted', 'cancelled'):
                raise AssertionError(last)
            time.sleep(.1)

    def remember_admission(current):
        supervisor = current['execution']['supervisor']
        suffix = f"__command {current['id']} run {supervisor['pid']} {supervisor['start_ticks']}"
        for note in Path('/tmp/claude-1000').glob('p2p-run-*.holder'):
            try:
                holder = json.loads(note.read_text())
                if suffix in holder.get('cmd', ''):
                    owned_notes[note] = holder['pid']
            except (OSError, ValueError):
                pass

    def joined(process):
        code = process.wait(timeout=10)
        assert code == 0, (code, (state / 'fixture-worker.log').read_text())

    lease = None
    try:
        server = daemon()
        (workspace / 'src/lib.rs').write_text('#[test] fn passes() { assert!(true); }\n')
        backlog = submit(approve=False)
        first = submit()
        w = worker()
        result = wait(first, lambda j: j['phase'] == 'passed')
        joined(w)
        assert job(backlog)['phase'] == 'queued', 'proposal backlog executed implicitly'
        assert result['execution']['snapshot']['digest']
        assert [s['stage'] for s in result['execution']['stages']] == ['build', 'run']
        assert all(s['exit_code'] == 0 for s in result['execution']['stages'])
        assert action('logs', first)['log']['text']
        assert any(e['stage'] == 'worker.run' for e in action('list')['estimates'])
        cache_policy = json.loads((state / f'jobs/{first}/compile-cache.json').read_text())
        assert cache_policy['max_size'] == '30G'
        assert Path(cache_policy['directory']).is_absolute()
        assert (root / 'target/.tasks-target.lock').exists()
        assert (root / 'target/.tasks-target.owner.json').exists()
        assert target_plan()['status'] == 'retain_recent'
        print('PASS: immutable source, explicit approval, build/run, logs/result/history, inert backlog', flush=True)
        server.kill()
        server.wait(timeout=5)
        server = daemon()
        assert job(first)['phase'] == 'passed'
        print('PASS: completed result survives daemon restart', flush=True)

        # Another cooperating state/worker owns this target: no build may begin.
        lease = (root / 'target/.tasks-target.lock').open('a+')
        fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        target_wait = submit()
        w = worker()
        wait(target_wait, lambda j: j['phase'] == 'preparing' and 'exclusive target lease' in j['execution']['detail'])
        assert not (state / f'jobs/{target_wait}/build-admitted.json').exists()
        action('cancel', target_wait)
        wait(target_wait, lambda j: j['phase'] == 'cancelled')
        joined(w)
        fcntl.flock(lease, fcntl.LOCK_UN)
        lease.close()
        lease = None
        print('PASS: target contention waits without executing; wait is cancellable', flush=True)

        # Exercise real statvfs admission, not only a synthetic threshold model.
        # This opt-in Linux fixture requires a small /dev/shm tmpfs.
        small = os.statvfs('/dev/shm')
        assert small.f_bavail * small.f_frsize < 20 * 1024**3, 'disk-gate fixture requires /dev/shm below 20 GiB free'
        low_target = Path(tempfile.mkdtemp(prefix='tm-disk-gate-', dir='/dev/shm'))
        low_disk = submit()
        w = worker(dict(env, TASK_MANAGER_TARGET_DIR=str(low_target)))
        wait(low_disk, lambda j: j['phase'] == 'preparing' and 'disk reserve' in j['execution']['detail'])
        assert not (state / f'jobs/{low_disk}/build-admitted.json').exists()
        assert not (low_target / '.tasks-target.lock').exists(), 'do not hold target while waiting for disk'
        action('cancel', low_disk)
        wait(low_disk, lambda j: j['phase'] == 'cancelled')
        joined(w)
        low_target.rmdir()  # Empty directory owned by this fixture only.
        print('PASS: real low-space target blocks before capture/build; wait is cancellable', flush=True)

        # Refuse unmeasurable target usage rather than assuming it fits a budget.
        # FIFO is metadata-only: never open it or allocate a multi-GiB fixture.
        budget_fixture = root / 'target/budget-unverifiable-fifo'
        os.mkfifo(budget_fixture, 0o600)
        refused = submit()
        w = worker()
        wait(refused, lambda j: j['phase'] == 'failed')
        assert w.wait(timeout=10) != 0, 'unverifiable target budget must stop this worker'
        assert 'target budget unverifiable' in action('logs', refused)['log']['text']
        assert not (state / f'jobs/{refused}/build-admitted.json').exists()
        assert target_plan()['status'] == 'protected_review'
        budget_fixture.unlink()  # Only our fixture, after its worker has exited.
        assert job(first)['phase'] == 'passed'
        print('PASS: unverifiable target budget refuses build without deleting prior results/artifacts', flush=True)

        (workspace / 'src/lib.rs').write_text('#[test] fn fails() { panic!("intentional fixture failure"); }\n')
        failed = submit()
        w = worker()
        wait(failed, lambda j: j['phase'] == 'failed')
        joined(w)
        assert job(failed)['execution']['stages'][-1]['exit_code'] != 0
        print('PASS: failing test result and exit status retained', flush=True)

        # Hold the real compile admission lock (no heavy work), ensuring waiting is observable.
        lock_path = Path('/tmp/claude-1000/p2p-build.lock')
        lease = lock_path.open('a+')
        deadline = time.monotonic() + args.queue_timeout if args.queue_timeout else None
        while True:
            try:
                fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                assert deadline is None or time.monotonic() < deadline, 'compile admission timeout; no duplicate started'
                time.sleep(.2)
        (workspace / 'src/lib.rs').write_text('#[test] fn passes() { assert!(true); }\n')
        waiting = submit()
        w = worker()
        wait(waiting, lambda j: j['phase'] == 'waiting_build')
        time.sleep(.3)
        assert not (state / f'jobs/{waiting}/build-admitted.json').exists()
        action('cancel', waiting)
        wait(waiting, lambda j: j['phase'] == 'cancelled')
        joined(w)
        assert job(backlog)['phase'] == 'queued'
        fcntl.flock(lease, fcntl.LOCK_UN)
        lease.close()
        lease = None
        print('PASS: shared lock blocks admission; waiting cancellation does not execute; backlog untouched', flush=True)

        (workspace / 'src/lib.rs').write_text('#[test] fn slow() { std::thread::sleep(std::time::Duration::from_secs(60)); }\n')
        cancelled = submit()
        w = worker()
        wait(cancelled, lambda j: j['phase'] == 'running')
        active_plan = target_plan()
        assert active_plan['status'] == 'protected_active' and active_plan['inventory'] is None
        action('cancel', cancelled)
        wait(cancelled, lambda j: j['phase'] == 'cancelled')
        joined(w)
        assert job(first)['phase'] == 'passed'
        print('PASS: running cancellation stops only fixture stage processes', flush=True)

        crashed = submit()
        w = worker()
        current = wait(crashed, lambda j: j['phase'] == 'running')
        remember_admission(current)
        w.kill()
        w.wait(timeout=5)
        wait(crashed, lambda j: j['phase'] == 'interrupted')
        recovery = worker()
        joined(recovery)
        assert job(crashed)['phase'] == 'interrupted'
        assert target_plan()['status'] == 'protected_review', 'worker death must not appear as clean idle retention'
        with (root / 'target/.tasks-target.lock').open('a+') as target_check:
            fcntl.flock(target_check, fcntl.LOCK_EX | fcntl.LOCK_NB)
        assert job(backlog)['phase'] == 'queued'
        print('PASS: killed worker cleanup, durable interruption, no automatic replay', flush=True)

        supervisor_crash = submit()
        w = worker()
        current = wait(supervisor_crash, lambda j: j['phase'] == 'running')
        remember_admission(current)
        supervisor = current['execution']['supervisor']['pid']
        os.kill(supervisor, signal.SIGKILL)
        wait(supervisor_crash, lambda j: j['phase'] == 'interrupted')
        assert w.wait(timeout=10) != 0, 'supervisor loss must stop worker, not overlap next job'
        deadline = time.monotonic() + 10
        lease_check = (state / 'worker.lock').open('a+')
        while True:
            try:
                fcntl.flock(lease_check, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                assert time.monotonic() < deadline, 'stage lease not released after supervisor loss'
                time.sleep(.1)
        lease_check.close()
        with (root / 'target/.tasks-target.lock').open('a+') as target_check:
            fcntl.flock(target_check, fcntl.LOCK_EX | fcntl.LOCK_NB)
        assert job(first)['phase'] == 'passed'
        print('PASS: killed stage supervisor terminates admitted command and releases worker lease', flush=True)

        # Tamper only our snapshot while admission is held; worker must refuse it.
        lease = lock_path.open('a+')
        deadline = time.monotonic() + args.queue_timeout if args.queue_timeout else None
        while True:
            try:
                fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                assert deadline is None or time.monotonic() < deadline, 'compile admission timeout; no duplicate started'
                time.sleep(.2)
        tampered = submit()
        w = worker()
        wait(tampered, lambda j: j['phase'] == 'waiting_build')
        captured = state / f'jobs/{tampered}/snapshot/src/lib.rs'
        captured.chmod(0o600)
        captured.write_text('#[test] fn injected() {{}}')
        fcntl.flock(lease, fcntl.LOCK_UN)
        lease.close()
        lease = None
        wait(tampered, lambda j: j['phase'] == 'failed')
        joined(w)
        assert 'digest mismatch' in action('logs', tampered)['log']['text']
        print('PASS: altered captured source rejected before build', flush=True)
        print('ALL WORKER E2E CHECKS PASSED; retained fixtures/logs:', root, flush=True)
    finally:
        if lease is not None:
            lease.close()
        for process in reversed(owned):
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
        # SIGKILL skips buildlock's finally block, leaving stale status metadata.
        # Remove ONLY notes recorded for our exact supervisor identity, after
        # obtaining the real slot and confirming that exact holder PID is gone.
        for note, pid in owned_notes.items():
            with note.with_suffix('.lock').open('a+') as slot:
                try:
                    fcntl.flock(slot, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    holder = json.loads(note.read_text())
                    if holder.get('pid') != pid:
                        continue
                    try:
                        os.kill(pid, 0)
                    except ProcessLookupError:
                        note.unlink()
                except (BlockingIOError, FileNotFoundError, ValueError):
                    pass
        # Stage supervisors notice worker death and clean up their own group.


if __name__ == '__main__':
    main()
