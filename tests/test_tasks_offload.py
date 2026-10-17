"""Synthetic SSH offload v0 tests: fake worker HOME, ssh shim, real local rsync.

Nothing here contacts a real host. The ssh shim ignores the host argument and
runs the remote command with `sh -c` under a temporary fake HOME; the fake
`~/.local/bin/tasks` mimics the installed client's `run`/`cancel` output.
"""
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest

SCRIPTS = Path(__file__).resolve().parents[1] / 'scripts'
CLIENT = SCRIPTS / 'tasks_offload.py'

SSH_SHIM = r'''#!/usr/bin/env python3
import json, os, subprocess, sys
args = sys.argv[1:]
with open(os.environ['FAKE_SSH_LOG'], 'a') as log:
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
home = os.environ['FAKE_REMOTE_HOME']
os.chdir(home)
env = dict(os.environ, HOME=home)
if os.path.exists(os.path.join(home, 'drop-ssh')) and ' run --who ' in command:
    child = subprocess.Popen(['sh', '-c', command], env=env, stderr=subprocess.PIPE)
    for line in child.stderr:
        sys.stderr.buffer.write(line)
        sys.stderr.flush()
        if b'submitted job' in line:
            break
    child.kill()
    child.wait()
    sys.stderr.write('Connection to fake closed by remote host.\n')
    sys.exit(255)
os.execve('/bin/sh', ['sh', '-c', command], env)
'''

FAKE_TASKS = r'''#!/usr/bin/env python3
import json, os, subprocess, sys, time
state = os.path.join(os.environ['HOME'], 'fake-tasks')
os.makedirs(state, exist_ok=True)
args = sys.argv[1:]
if args[:1] == ['cancel'] and len(args) == 2:
    open(os.path.join(state, 'cancel-' + args[1]), 'w').close()
    print(json.dumps({'version': 2, 'error': None}))
    sys.exit(0)
if args[:1] != ['run'] or '--' not in args:
    sys.stderr.write('tasks: enqueue requires -- before argv\n')
    sys.exit(2)
split = args.index('--')
opts, argv = args[1:split], args[split + 1:]
who = opts[opts.index('--who') + 1] if '--who' in opts else 'user'
counter = os.path.join(state, 'counter')
job = (int(open(counter).read()) if os.path.exists(counter) else 40) + 1
open(counter, 'w').write(str(job))
with open(os.path.join(state, 'job-%d.json' % job), 'w') as f:
    json.dump({'who': who, 'opts': opts, 'argv': argv, 'cwd': os.getcwd()}, f)
sys.stderr.write('tasks: submitted job %d; Ctrl-C detaches, tasks cancel %d cancels\n' % (job, job))
sys.stderr.write('tasks: job %d Running\n' % job)
sys.stderr.flush()
child = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
open(os.path.join(state, 'started-%d' % job), 'w').close()
phase = None
while child.poll() is None:
    if os.path.exists(os.path.join(state, 'cancel-%d' % job)):
        child.kill()
        phase = 'cancelled'
        break
    time.sleep(0.05)
out = child.communicate()[0]
code = child.returncode
if phase == 'cancelled':
    code = 130
elif code == 0:
    phase = 'passed'
else:
    phase = 'failed'
sys.stderr.write('tasks: job %d %s\n' % (job, phase.capitalize()))
sys.stdout.buffer.write(out)
if out and not out.endswith(b'\n'):
    sys.stdout.write('\n')
print('job %d %s (exit %d) owner=%s' % (job, phase, code, who))
sys.stdout.flush()
open(os.path.join(state, 'finished-%d' % job), 'w').close()
sys.exit(code)
'''


class OffloadTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='tasks-offload-test-')
        self.addCleanup(self.tmp.cleanup)
        base = Path(self.tmp.name)
        self.local_home = base / 'local-home'
        self.remote_home = base / 'remote-home'
        self.bin = base / 'bin'
        self.repo = base / 'repo'
        for d in (self.local_home, self.remote_home / '.local/bin', self.bin, self.repo):
            d.mkdir(parents=True)
        self.ssh_log = base / 'ssh.log'
        shim = self.bin / 'fake-ssh'
        shim.write_text(SSH_SHIM)
        shim.chmod(0o755)
        tasks = self.remote_home / '.local/bin/tasks'
        tasks.write_text(FAKE_TASKS)
        tasks.chmod(0o755)
        self.env = dict(os.environ, HOME=str(self.local_home),
                        FAKE_REMOTE_HOME=str(self.remote_home), FAKE_SSH_LOG=str(self.ssh_log),
                        TASKS_OFFLOAD_SSH=str(shim), GIT_CONFIG_NOSYSTEM='1',
                        GIT_CONFIG_GLOBAL=os.devnull, PYTHONDONTWRITEBYTECODE='1')
        self.git('init', '-q')
        self.write('Cargo.toml', '[package]\nname = "fixture"\n')
        self.write('src/lib.rs', 'pub fn a() {}\n')
        self.git('add', 'Cargo.toml', 'src/lib.rs')

    # helpers -----------------------------------------------------------------
    def git(self, *args):
        return subprocess.run(['git', *args], cwd=self.repo, env=self.env, check=True,
                              capture_output=True, text=True).stdout

    def write(self, rel, text, mode=None):
        p = self.repo / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)
        if mode is not None:
            p.chmod(mode)
        return p

    def offload(self, *cmd, opts=(), cwd=None, check_exit=None):
        argv = [sys.executable, '-B', str(CLIENT), '--worker', 'tester@worker.invalid',
                '--who', 'offload-test', *opts, '--', *cmd]
        result = subprocess.run(argv, cwd=cwd or self.repo, env=self.env,
                                capture_output=True, text=True, timeout=120)
        if check_exit is not None:
            self.assertEqual(result.returncode, check_exit, result.stdout + result.stderr)
        return result

    @staticmethod
    def summary(result):
        line = result.stdout.strip().splitlines()[-1]
        return json.loads(line)

    def remote_dir(self):
        dirs = [p for p in (self.remote_home / 'offload/checkouts').iterdir() if p.is_dir()]
        self.assertEqual(len(dirs), 1)
        return dirs[0]

    def remote_files(self):
        root = self.remote_dir()
        return sorted(str(p.relative_to(root)) for p in root.rglob('*')
                      if p.is_file() or p.is_symlink())

    def job_record(self, job):
        return json.loads((self.remote_home / f'fake-tasks/job-{job}.json').read_text())

    # tests -------------------------------------------------------------------
    def test_unborn_head_tracked_dirty_untracked_synced_and_ignored_skipped(self):
        self.assertNotEqual(subprocess.run(['git', 'rev-parse', '--verify', 'HEAD'], cwd=self.repo,
                                           env=self.env, capture_output=True).returncode, 0)
        self.write('src/lib.rs', 'pub fn dirty() {}\n')            # tracked, dirty
        self.write('notes/untracked.txt', 'new\n')                 # untracked
        self.write('.gitignore', 'ignored.txt\n')
        self.write('ignored.txt', 'nope\n')
        r = self.offload('true', check_exit=0)
        root = self.remote_dir()
        self.assertEqual((root / 'src/lib.rs').read_text(), 'pub fn dirty() {}\n')
        self.assertEqual((root / 'notes/untracked.txt').read_text(), 'new\n')
        self.assertFalse((root / 'ignored.txt').exists())
        s = self.summary(r)
        self.assertEqual(s['exit'], 0)
        self.assertEqual(s['remote_job_id'], 41)
        self.assertEqual(s['synced_files'], 4)
        self.assertEqual(s['worker'], 'tester@worker.invalid')
        self.assertTrue(s['remote_dir'].startswith('~/offload/checkouts/'))
        rec = self.job_record(41)
        self.assertTrue(rec['who'].startswith('offload-test@'))
        self.assertEqual(rec['argv'], ['true'])
        self.assertNotIn('--run', rec['opts'])  # compile resource unless --remote-run

    def test_excluded_credential_and_output_paths_not_sent(self):
        excluded = ['.env', '.env.local', 'secrets/api.txt', 'deploy/id_ed25519', 'server.pem',
                    'debug.log', 'target/debug/app', 'crate/target/x.rlib', '.cargo/credentials.toml',
                    'build/out.bin', 'node_modules/m/index.js', 'app.sqlite']
        self.write('crate/Cargo.toml', '[package]\nname="c"\n')
        for rel in excluded:
            self.write(rel, 'SECRET-OR-OUTPUT\n')
        r = self.offload('true', check_exit=0)
        files = self.remote_files()
        for rel in excluded:
            self.assertNotIn(rel, files)
        self.assertIn('crate/Cargo.toml', files)
        self.assertGreaterEqual(self.summary(r)['excluded_files'], len(excluded))
        for p in self.remote_dir().rglob('*'):
            if p.is_file():
                self.assertNotIn('SECRET-OR-OUTPUT', p.read_text())

    def test_prune_deleted_file_preserves_target_and_rename(self):
        self.write('a.txt', 'A\n')
        self.write('b.txt', 'B\n')
        self.offload('sh', '-c', 'mkdir -p target/debug crate/target && echo built > target/debug/app'
                     ' && echo x > crate/target/y', check_exit=0)
        self.write('crate/Cargo.toml', '[package]\nname="c"\n')
        root = self.remote_dir()
        (root / 'stale-remote-only.txt').write_text('old\n')
        (self.repo / 'b.txt').unlink()
        (self.repo / 'a.txt').rename(self.repo / 'renamed.txt')
        r = self.offload('true', check_exit=0)
        files = self.remote_files()
        self.assertNotIn('b.txt', files)
        self.assertNotIn('a.txt', files)
        self.assertNotIn('stale-remote-only.txt', files)
        self.assertEqual((root / 'renamed.txt').read_text(), 'A\n')
        self.assertEqual((root / 'target/debug/app').read_text(), 'built\n')
        self.assertEqual((root / 'crate/target/y').read_text(), 'x\n')
        self.assertIn('b.txt', self.summary(r)['pruned'])

    def test_exec_bit_change_propagates(self):
        self.write('run.sh', '#!/bin/sh\necho hi\n', mode=0o644)
        self.offload('true', check_exit=0)
        self.assertFalse(os.access(self.remote_dir() / 'run.sh', os.X_OK))
        (self.repo / 'run.sh').chmod(0o755)
        r = self.offload('./run.sh', check_exit=0)
        self.assertTrue(os.access(self.remote_dir() / 'run.sh', os.X_OK))
        self.assertIn('hi', r.stdout)
        (self.repo / 'run.sh').chmod(0o644)
        self.offload('true', check_exit=0)
        self.assertFalse(os.access(self.remote_dir() / 'run.sh', os.X_OK))

    def test_in_tree_symlink_kept_and_escaping_symlink_rejected(self):
        os.symlink('src/lib.rs', self.repo / 'link.rs')
        self.offload('true', check_exit=0)
        self.assertEqual(os.readlink(self.remote_dir() / 'link.rs'), 'src/lib.rs')
        os.symlink('../../etc/passwd', self.repo / 'src/escape')
        r = self.offload('true', check_exit=2)
        self.assertIn('escapes', r.stderr)

    def test_subdir_cwd_is_mapped(self):
        self.write('sub/dir/file.txt', 'x\n')
        r = self.offload('pwd', cwd=self.repo / 'sub/dir', check_exit=0)
        rec = self.job_record(self.summary(r)['remote_job_id'])
        self.assertEqual(rec['cwd'], str(self.remote_dir() / 'sub/dir'))
        self.assertEqual(self.summary(r)['remote_cwd'].split('/')[-2:], ['sub', 'dir'])

    def test_exit_code_propagates(self):
        r = self.offload('sh', '-c', 'echo failing; exit 7', check_exit=7)
        self.assertIn('failing', r.stdout)
        self.assertEqual(self.summary(r)['exit'], 7)
        self.assertEqual(self.summary(r)['remote_phase'], 'failed')

    def test_quoting_of_args_with_spaces_and_quotes(self):
        args = ['a b', "it's", '"dq"', '$HOME', '*', '; rm -rf /', '`id`', '']
        r = self.offload('printf', '%s|', *args, check_exit=0)
        rec = self.job_record(self.summary(r)['remote_job_id'])
        self.assertEqual(rec['argv'], ['printf', '%s|', *args])
        self.assertIn('a b|it\'s|"dq"|$HOME|*|; rm -rf /|`id`||', r.stdout)

    def test_remote_run_flag_and_owner_identity(self):
        r = self.offload('true', opts=('--remote-run',), check_exit=0)
        rec = self.job_record(self.summary(r)['remote_job_id'])
        self.assertIn('--run', rec['opts'])
        self.assertRegex(rec['who'], r'^offload-test@[^@]+$')

    def test_fetch_into_default_destination(self):
        r = self.offload('sh', '-c', 'mkdir -p target/release && echo artifact > target/release/foo',
                         opts=('--fetch', 'target/release/foo'), check_exit=0)
        s = self.summary(r)
        dest = self.local_home / 'build-cache/offload/offload-test' / str(s['remote_job_id'])
        self.assertEqual((dest / 'target/release/foo').read_text(), 'artifact\n')
        self.assertEqual(s['fetched'], ['target/release/foo'])
        self.assertEqual(s['fetch_dest'], str(dest))
        self.assertFalse((self.repo / 'target').exists())

    def test_fetch_rejects_escaping_and_checkout_destination(self):
        self.offload('true', opts=('--fetch', '../x'), check_exit=2)
        self.offload('true', opts=('--fetch', 'a', '--fetch-dest', str(self.repo / 'out')),
                     check_exit=2)
        self.assertFalse(self.ssh_log.exists())

    def test_dry_run_touches_nothing_remote(self):
        self.write('a.txt', 'A\n')
        r = self.offload('cargo', 'test', opts=('--dry-run', '--cargo-fetch'), check_exit=0)
        self.assertFalse(self.ssh_log.exists())
        self.assertFalse((self.remote_home / 'offload').exists())
        self.assertFalse((self.remote_home / '.local/lib').exists())
        s = self.summary(r)
        self.assertTrue(s['dry_run'])
        self.assertIsNone(s['remote_job_id'])
        self.assertEqual(s['synced_files'], 3)

    def test_agent_installed_once(self):
        self.offload('true', check_exit=0)
        agent = self.remote_home / '.local/lib/tasks-offload/agent.py'
        self.assertTrue(agent.is_file())
        installs = self.ssh_log.read_text().count('install-agent')
        self.offload('true', check_exit=0)
        self.assertEqual(self.ssh_log.read_text().count('install-agent'), installs)
        agent.write_text('# tampered\n')
        self.offload('true', check_exit=0)
        self.assertNotEqual(agent.read_text(), '# tampered\n')

    def test_disconnect_reports_uncertain_and_never_resubmits(self):
        (self.remote_home / 'drop-ssh').write_text('')
        r = self.offload('sh', '-c', 'sleep 0.2')
        self.assertNotEqual(r.returncode, 0)
        self.assertIn('UNCERTAIN', r.stderr)
        self.assertIn('tasks logs 41', r.stderr)
        s = self.summary(r)
        self.assertEqual(s['remote_job_id'], 41)
        self.assertEqual(s['remote_state'], 'uncertain')
        self.assertFalse((self.remote_home / 'fake-tasks/job-42.json').exists())

    def test_sigterm_cancels_remote_job_with_right_id(self):
        # Burn job ids 41/42 first so the cancelled id is not the default.
        self.offload('true', check_exit=0)
        self.offload('true', check_exit=0)
        argv = [sys.executable, '-B', str(CLIENT), '--worker', 'tester@worker.invalid',
                '--who', 'offload-test', '--', 'sleep', '30']
        proc = subprocess.Popen(argv, cwd=self.repo, env=self.env, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, text=True)
        started = self.remote_home / 'fake-tasks/started-43'
        deadline = time.monotonic() + 30
        while not started.exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertTrue(started.exists())
        time.sleep(0.2)
        proc.send_signal(signal.SIGTERM)
        out, err = proc.communicate(timeout=60)
        self.assertEqual(proc.returncode, 143, out + err)
        state = self.remote_home / 'fake-tasks'
        self.assertTrue((state / 'cancel-43').exists())
        self.assertFalse((state / 'cancel-41').exists())
        self.assertFalse((state / 'cancel-42').exists())
        s = json.loads(out.strip().splitlines()[-1])
        self.assertEqual(s['remote_job_id'], 43)
        self.assertEqual(s['remote_phase'], 'cancelled')
        self.assertEqual(s['cancel_requested'], True)

    def test_second_offload_of_same_checkout_waits_for_the_first(self):
        # A second sync would rewrite the worker copy under the first job's build.
        argv = [sys.executable, '-B', str(CLIENT), '--worker', 'tester@worker.invalid',
                '--who', 'offload-test', '--', 'sleep', '1']
        first = subprocess.Popen(argv, cwd=self.repo, env=self.env, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, text=True)
        state = self.remote_home / 'fake-tasks'
        deadline = time.monotonic() + 30
        while not (state / 'started-41').exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertTrue((state / 'started-41').exists())
        self.write('src/lib.rs', 'pub fn b() {}\n')
        second = self.offload('true', check_exit=0)
        first.communicate(timeout=60)
        self.assertEqual(first.returncode, 0)
        self.assertIn('waiting for another offload of this checkout', second.stderr)
        self.assertLessEqual((state / 'finished-41').stat().st_mtime_ns,
                             (state / 'started-42').stat().st_mtime_ns)

    def test_cargo_fetch_runs_remotely_before_job_and_failure_stops(self):
        fake_cargo = self.bin / 'cargo'
        fake_cargo.write_text('#!/bin/sh\nprintf "%s\\n" "$PWD" "$@" > "$HOME/cargo-call"\n'
                              'exit "${FAKE_CARGO_EXIT:-0}"\n')
        fake_cargo.chmod(0o755)
        self.env['PATH'] = f'{self.bin}:{self.env["PATH"]}'
        r = self.offload('true', opts=('--cargo-fetch',), check_exit=0)
        call = (self.remote_home / 'cargo-call').read_text().splitlines()
        self.assertEqual(call, [str(self.remote_dir()), 'fetch', '--locked'])
        self.assertIn('cargo_fetch_s', self.summary(r)['durations'])
        self.env['FAKE_CARGO_EXIT'] = '101'
        r = self.offload('true', opts=('--cargo-fetch',), check_exit=255)
        self.assertEqual(self.summary(r)['remote_state'], 'not_submitted')
        self.assertFalse((self.remote_home / 'fake-tasks/job-42.json').exists())

    def test_nested_repository_is_refused(self):
        nested = self.repo / 'vendor/inner'
        nested.mkdir(parents=True)
        subprocess.run(['git', 'init', '-q'], cwd=nested, env=self.env, check=True)
        (nested / 'f.txt').write_text('x\n')
        r = self.offload('true', check_exit=2)
        self.assertIn('submodule', r.stderr)
        self.assertFalse(self.ssh_log.exists())

    def test_host_style_sigterm_then_sigkill_still_cancels_remote(self):
        # Installed worker: SIGTERM to the job's process group, ~300 ms, SIGKILL.
        argv = [sys.executable, '-B', str(CLIENT), '--worker', 'tester@worker.invalid',
                '--who', 'offload-test', '--', 'sleep', '30']
        proc = subprocess.Popen(argv, cwd=self.repo, env=self.env, stdout=subprocess.DEVNULL,
                                stderr=subprocess.DEVNULL, start_new_session=True)
        state = self.remote_home / 'fake-tasks'
        deadline = time.monotonic() + 30
        while not (state / 'started-41').exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertTrue((state / 'started-41').exists())
        os.killpg(proc.pid, signal.SIGTERM)
        time.sleep(0.3)
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        proc.wait(10)
        deadline = time.monotonic() + 30
        while not (state / 'finished-41').exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertTrue((state / 'cancel-41').exists())
        self.assertTrue((state / 'finished-41').exists())
        time.sleep(0.3)  # let the orphaned ssh shim exit before temp cleanup

    def test_outside_git_checkout_fails_clearly(self):
        outside = Path(self.tmp.name) / 'plain'
        outside.mkdir()
        r = self.offload('true', cwd=outside, check_exit=2)
        self.assertIn('git', r.stderr.lower())
        self.assertFalse(self.ssh_log.exists())


if __name__ == '__main__':
    unittest.main()
