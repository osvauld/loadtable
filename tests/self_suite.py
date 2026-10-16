#!/usr/bin/env python3
"""Submit the real checkout self-suite through its worker. Run directly, not
inside buildlock: the worker takes compile/run admission itself. State/logs are
retained outside Git. No existing daemon/state or other processes are stopped.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
from wait_budget import WaitBudget


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('binary', type=Path)
    ap.add_argument('source', type=Path)
    ap.add_argument('--target-dir', type=Path)
    ap.add_argument('--timeout', type=float, default=300, help='observed execution seconds, excluding admission')
    ap.add_argument('--queue-timeout', type=float, default=0, help='separate admission limit; 0 waits indefinitely')
    args = ap.parse_args()
    WaitBudget(args.timeout, args.queue_timeout)  # Validate before creating fixtures.
    binary = str(args.binary.resolve())
    source = args.source.resolve()
    revision = subprocess.check_output(['git', '-C', str(source), 'rev-parse', 'HEAD'], text=True).strip()
    state = Path(tempfile.mkdtemp(prefix='task-worker-self-suite-'))
    env = dict(os.environ, TASK_MANAGER_STATE_DIR=str(state))
    if args.target_dir:
        env['TASK_MANAGER_TARGET_DIR'] = str(args.target_dir.resolve())
    log = (state / 'fixture.log').open('w')
    daemon = subprocess.Popen([binary, 'daemon'], env=env, stdout=log, stderr=log)
    worker = None

    def cli(*arguments):
        argv = [binary, *arguments]
        if arguments and arguments[0] == 'logs':
            argv.append('--json')
        result = subprocess.run(argv, env=env, capture_output=True, text=True, timeout=10)
        assert result.returncode == 0, result.stderr + result.stdout
        return json.loads(result.stdout)

    try:
        deadline = time.monotonic() + 10
        while not (state / 'taskd.sock').exists():
            assert daemon.poll() is None, 'fixture daemon exited'
            assert time.monotonic() < deadline, 'fixture daemon startup timeout'
            time.sleep(.05)
        spec = dict(suite='task-manager.tests', owner='self-suite-validation', source=str(source),
                    revision=revision, configuration='debug/offline', cost='quick')
        spec_path = state / 'spec.json'
        spec_path.write_text(json.dumps(spec))
        job_id = cli('submit', str(spec_path))['jobs'][-1]['id']
        cli('approve', str(job_id))
        worker = subprocess.Popen([binary, 'worker'], env=env, stdout=log, stderr=log)
        print('Real self-suite job', job_id, 'state', state, 'worker', worker.pid, flush=True)
        budget = WaitBudget(args.timeout, args.queue_timeout)
        last = None
        while True:
            job = next(j for j in cli('list')['jobs'] if j['id'] == job_id)
            expired = budget.update(job['phase'])
            if expired:
                raise AssertionError(f'{expired}; no duplicate launched. Inspect {state}')
            if last != job['phase']:
                print('Phase:', job['phase'], flush=True)
                last = job['phase']
            if job['phase'] in ('passed', 'failed', 'interrupted', 'cancelled'):
                break
            time.sleep(.2)
        print(json.dumps(job['execution'], indent=2), flush=True)
        print(cli('logs', str(job_id))['log']['text'][-12000:], flush=True)
        assert job['phase'] == 'passed', job
        assert worker.wait(timeout=10) == 0
        print('SELF-SUITE PASSED; retained source snapshot/results/logs:', state, flush=True)
    finally:
        if worker and worker.poll() is None:
            worker.terminate()
            try:
                worker.wait(timeout=15)
            except subprocess.TimeoutExpired:
                worker.kill()
                worker.wait(timeout=5)
        daemon.terminate()
        daemon.wait(timeout=5)
        log.close()


if __name__ == '__main__':
    main()
