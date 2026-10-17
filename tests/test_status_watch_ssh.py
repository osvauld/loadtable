"""Synthetic tests for scripts/status-watch-ssh (ssh fallback of the status stream).

The ssh shim ignores the host and runs the remote command with `sh -c` under a
temporary fake HOME whose `~/.local/bin/tasks status --json` prints a fixture
snapshot. Control files in that HOME make ssh fail (exit 255) or the status
command fail, so offline/status_error markers and recovery are exercised without
contacting a real host.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
WATCH = ROOT / 'scripts/status-watch-ssh'
FIXTURE = ROOT / 'experiments/osvauld-foundation/tests/fixtures/status-snapshot.json'

SSH_SHIM = r'''#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
home = os.environ['FAKE_REMOTE_HOME']
with open(os.path.join(home, 'ssh.log'), 'a') as log:
    log.write(json.dumps(args) + '\n')
with_value = set('bcDEeFIiJLlmOopQRSWw')
i = 0
while i < len(args):
    a = args[i]
    if a == '--':
        i += 1
        break
    if a.startswith('-') and len(a) > 1:
        i += 2 if (a[1] in with_value and len(a) == 2) else 1
        continue
    break
command = ' '.join(args[i + 1:])
polls = os.path.join(home, 'polls')
n = (int(open(polls).read()) if os.path.exists(polls) else 0) + 1
open(polls, 'w').write(str(n))
down = os.path.join(home, 'down-polls')
if os.path.exists(down) and n in json.load(open(down)):
    sys.stderr.write('ssh: connect to host worker.invalid port 22: No route to host\n')
    sys.exit(255)
os.chdir(home)
os.execve('/bin/sh', ['sh', '-c', command], dict(os.environ, HOME=home))
'''

FAKE_TASKS = r'''#!/bin/sh
if [ "$1 $2" != "status --json" ]; then echo "tasks: usage" >&2; exit 2; fi
if [ -e "$HOME/status-broken" ]; then echo "tasks: daemon unavailable" >&2; exit 1; fi
if [ -e "$HOME/status-garbage" ]; then echo "not json"; exit 0; fi
exec cat "$HOME/snapshot.json"
'''


class StatusWatchSshTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='status-watch-ssh-test-')
        self.addCleanup(self.tmp.cleanup)
        base = Path(self.tmp.name)
        self.remote = base / 'remote-home'
        (self.remote / '.local/bin').mkdir(parents=True)
        shim = base / 'fake-ssh'
        shim.write_text(SSH_SHIM)
        shim.chmod(0o755)
        tasks = self.remote / '.local/bin/tasks'
        tasks.write_text(FAKE_TASKS)
        tasks.chmod(0o755)
        (self.remote / 'snapshot.json').write_text(FIXTURE.read_text())
        self.env = dict(os.environ, FAKE_REMOTE_HOME=str(self.remote),
                        TASKS_OFFLOAD_SSH=str(shim), PYTHONDONTWRITEBYTECODE='1',
                        XDG_RUNTIME_DIR=str(base))

    def watch(self, *opts, count=1, check=0):
        argv = [str(WATCH), '--worker', 'tester@worker.invalid', '--interval', '1',
                '--backoff-max', '1', '--count', str(count), *opts]
        r = subprocess.run(argv, env=self.env, capture_output=True, text=True, timeout=60)
        self.assertEqual(r.returncode, check, r.stdout + r.stderr)
        lines = r.stdout.splitlines()
        self.assertEqual(len(lines), count if check == 0 else 0, r.stdout)
        return [json.loads(line) for line in lines]

    def ssh_calls(self):
        return [json.loads(line) for line in (self.remote / 'ssh.log').read_text().splitlines()]

    def test_snapshots_are_compact_lines_marked_online(self):
        lines = self.watch(count=2)
        expected = json.loads(FIXTURE.read_text())
        for seq, line in enumerate(lines, 1):
            self.assertEqual(line['machine']['online'], True)
            self.assertEqual(line['stream'], {**line['stream'], 'transport': 'ssh',
                                              'peer': 'tester@worker.invalid', 'seq': seq})
            del line['stream']
            del line['machine']['online']
            self.assertEqual(line, expected)
        call = self.ssh_calls()[0]
        self.assertIn('BatchMode=yes', call)
        self.assertIn('ControlMaster=auto', call)
        self.assertEqual(call[-1], '"$HOME"/.local/bin/tasks status --json')

    def test_ssh_failure_prints_offline_marker_then_recovers(self):
        (self.remote / 'down-polls').write_text('[1, 2]')
        lines = self.watch(count=3)
        self.assertEqual([line['machine']['online'] for line in lines], [False, False, True])
        first = lines[0]
        self.assertEqual(first['version'], 1)
        self.assertEqual(first['jobs'], [])
        self.assertEqual(first['machine']['id'], 'worker.invalid',
                         'before any snapshot the marker uses the ssh host')
        self.assertEqual(first['stream']['marker'], 'offline')
        self.assertEqual(first['stream']['transport'], 'ssh')
        self.assertIn('No route to host', first['stream']['error'])
        self.assertEqual(first['stream']['retry_in_s'], 1)
        self.assertEqual(lines[2]['machine']['id'], 'synthetic-worker')

    def test_offline_after_a_snapshot_keeps_the_learned_machine_id(self):
        (self.remote / 'down-polls').write_text('[2]')
        lines = self.watch('--machine-id', 'configured-id', count=2)
        self.assertEqual(lines[0]['machine']['id'], 'synthetic-worker')
        self.assertEqual(lines[1]['machine'], {'id': 'synthetic-worker', 'online': False})

    def test_failing_or_invalid_status_command_is_status_error_not_offline(self):
        (self.remote / 'status-broken').touch()
        line = self.watch('--machine-id', 'w1')[0]
        self.assertEqual(line['machine'], {'id': 'w1', 'online': True})
        self.assertEqual(line['stream']['marker'], 'status_error')
        self.assertIn('daemon unavailable', line['stream']['error'])
        (self.remote / 'status-broken').unlink()
        (self.remote / 'status-garbage').touch()
        line = self.watch()[0]
        self.assertEqual(line['stream']['marker'], 'status_error')
        self.assertIn('not JSON', line['stream']['error'])

    def test_size_limit(self):
        line = self.watch('--max-bytes', '100')[0]
        self.assertEqual(line['stream']['marker'], 'status_error')
        self.assertIn('too large', line['stream']['error'])

    def test_options_are_validated_before_contacting_anything(self):
        for opts in (['--interval', '0.5'], ['--max-bytes', str(5 << 20)], ['--count', '0']):
            r = subprocess.run([str(WATCH), '--worker', 'tester@worker.invalid', *opts],
                               env=self.env, capture_output=True, text=True, timeout=60)
            self.assertEqual(r.returncode, 2, opts)
        r = subprocess.run([str(WATCH), '--worker', '-oProxyCommand=x'], env=self.env,
                           capture_output=True, text=True, timeout=60)
        self.assertEqual(r.returncode, 2)
        self.assertFalse((self.remote / 'ssh.log').exists())

    def test_no_multiplex_and_ssh_key(self):
        self.watch('--no-multiplex', '--ssh-key', '/synthetic/key')
        call = self.ssh_calls()[0]
        self.assertNotIn('ControlMaster=auto', call)
        self.assertIn('/synthetic/key', call)
        self.assertIn('IdentitiesOnly=yes', call)


if __name__ == '__main__':
    unittest.main()
