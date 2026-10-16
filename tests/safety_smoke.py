#!/usr/bin/env python3
"""Bounded real gate/cancel smoke. No stress generation or outer admission.
If the host meets the REAL policy, execute only a CPU/address-space-limited shell.
Otherwise verify waiting/cancellation; never override pressure to force admission.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

binary = Path(sys.argv[1]).resolve()
root = Path(tempfile.mkdtemp(prefix='safety-smoke-', dir=Path.home() / 'build-cache/tm-fixtures'))
env = dict(os.environ, TASK_MANAGER_STATE_DIR=str(root / 'state'))
log = (root / 'daemon.log').open('w')
daemon = subprocess.Popen([str(binary), 'daemon'], env=env, stdout=log, stderr=log)
job_id = None

def cli(*args):
    p = subprocess.run([str(binary), *map(str, args)], env=env, cwd=root,
                       capture_output=True, text=True, timeout=5)
    assert p.returncode == 0, (p.stdout, p.stderr)
    r = json.loads(p.stdout)
    assert not r['error'], r
    return r

def job():
    return next(j for j in cli('list')['jobs'] if j['id'] == job_id)

try:
    deadline = time.monotonic() + 5
    while not (root / 'state/taskd.sock').exists():
        assert daemon.poll() is None and time.monotonic() < deadline
        time.sleep(.05)
    command = 'ulimit -t 2 && ulimit -v 131072 && printf "%s" "$CARGO_BUILD_JOBS" > executed; printf bounded-smoke'
    job_id = cli('enqueue', '--who', 'synthetic-safety-smoke', '--run', '--', '/bin/sh', '-c', command)['submitted_id']
    cli('approve-command', job_id)
    cli('start-worker')
    deadline = time.monotonic() + 8
    current = job()
    while current['phase'] not in ('passed', 'failed', 'cancelled', 'interrupted') and time.monotonic() < deadline:
        time.sleep(.1)
        current = job()
    if current['phase'] == 'passed':
        assert (root / 'executed').read_text() == '2'
        outcome = 'bounded real command passed with CARGO_BUILD_JOBS=2'
    else:
        assert current['phase'] == 'waiting_run', current
        assert not (root / 'executed').exists()
        cli('cancel', job_id)
        deadline = time.monotonic() + 8
        while job()['phase'] != 'cancelled':
            assert time.monotonic() < deadline, job()
            time.sleep(.1)
        assert not (root / 'executed').exists()
        outcome = 'real admission waited and cancelled without executing workload'
    record = cli('logs', job_id, '--json')
    plain = subprocess.run([str(binary), 'logs', str(job_id)], env=env, capture_output=True, text=True, timeout=5)
    assert plain.returncode == 0 and plain.stdout == record['log']['text']
    summary = {'status': 'passed', 'outcome': outcome, 'fixture': str(root), 'job': job(),
               'stress_generated': False, 'full_execution_proven': outcome.startswith('bounded real command')}
    (root / 'summary.json').write_text(json.dumps(summary, indent=2))
    print(json.dumps(summary, indent=2), flush=True)
finally:
    if job_id is not None:
        try:
            if job()['phase'] not in ('passed', 'failed', 'cancelled', 'interrupted'):
                cli('cancel', job_id)
        except Exception:
            pass
    # Wait for the owned worker/cleanup lease, then stop only this owned daemon.
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        try:
            if not cli('list')['execution_enabled']:
                break
        except Exception:
            break
        time.sleep(.1)
    daemon.terminate()
    daemon.wait(timeout=5)
    log.close()
