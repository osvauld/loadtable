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

# Fake `lan-peer` (iroh transport seam). Same CLI and last-line JSON as the real binary, but
# local: sync-push writes into FAKE_REMOTE_HOME/offload/checkouts/NAME, fetch copies back, and
# each side keeps a fake content-addressed store so "blobs sent" counts are realistic. The
# iroh_blobs_hash is a stand-in digest (no blake3 module here); the real binary is exercised
# by IrohLoopbackTests below when it is built.
FAKE_LAN_PEER = r'''#!/usr/bin/env python3
import hashlib, json, os, shutil, sys
args = sys.argv[1:]
with open(os.environ['FAKE_LAN_PEER_LOG'], 'a') as log:
    log.write(json.dumps(args) + '\n')
def opt(name, many=False):
    vals = [args[i + 1] for i, a in enumerate(args[:-1]) if a == name]
    return vals if many else (vals[0] if vals else None)
def out(value, code=0):
    print(json.dumps(value))
    sys.exit(code)
remote = os.path.join(os.environ['FAKE_REMOTE_HOME'], 'offload', 'checkouts')
domain = b'task-manager-local-source-manifest-v1\0'
cmd = args[0]
if cmd == 'manifest':
    root = args[1]
    paths = [p for p in open(opt('--files-from0'), 'rb').read().decode().split('\0') if p]
    files, digest = [], hashlib.sha256(domain)
    for p in sorted(paths, key=lambda p: p.encode()):
        data = open(os.path.join(root, p), 'rb').read()
        ex = bool(os.stat(os.path.join(root, p)).st_mode & 0o111)
        sha = hashlib.sha256(data).hexdigest()
        if os.environ.get('FAKE_LAN_PEER_CORRUPT'):
            sha = hashlib.sha256(data + b'x').hexdigest()
        files.append({'path': p, 'size': len(data), 'executable': ex, 'sha256': sha,
                      'iroh_blobs_hash': hashlib.blake2s(data).hexdigest()})
        digest.update(len(p.encode()).to_bytes(8, 'little') + p.encode() + bytes([ex])
                      + len(data).to_bytes(8, 'little') + bytes.fromhex(sha))
    out({'schema': 'task-manager-local-source-manifest-v1', 'hash_algorithm': 'sha256',
         'receipt': digest.hexdigest(), 'iroh_blobs_hash': None, 'files': files})
if os.environ.get('FAKE_LAN_PEER_FAIL') == cmd:
    out({'status': 'rejected', 'stage': 'receiver', 'reason': 'NotAllowlisted'}, 3)
if cmd == 'sync-push':
    manifest = json.load(open(opt('--manifest')))
    name, src = opt('--checkout'), opt('--source')
    store = os.path.join(os.environ['FAKE_REMOTE_HOME'], 'fake-store')
    os.makedirs(store, exist_ok=True)
    dest = os.path.join(remote, name)
    created = not os.path.isdir(dest)
    os.makedirs(dest, exist_ok=True)
    sent = sent_bytes = written = unchanged = 0
    for f in manifest['files']:
        blob = os.path.join(store, f['iroh_blobs_hash'])
        data = open(os.path.join(src, f['path']), 'rb').read()
        if not os.path.exists(blob) and f['size']:
            open(blob, 'wb').write(data)
            sent, sent_bytes = sent + 1, sent_bytes + len(data)
        target = os.path.join(dest, f['path'])
        mode = 0o755 if f['executable'] else 0o644
        if (os.path.isfile(target) and open(target, 'rb').read() == data
                and os.stat(target).st_mode & 0o777 == mode):
            unchanged += 1
            continue
        os.makedirs(os.path.dirname(target), exist_ok=True)
        open(target, 'wb').write(data)
        os.chmod(target, mode)
        written += 1
    keep = {f['path'] for f in manifest['files']}
    deleted = []
    for dirpath, dirnames, filenames in os.walk(dest):
        rel = os.path.relpath(dirpath, dest)
        if rel == '.':
            dirnames[:] = [d for d in dirnames if d != 'target']
        for fn in filenames:
            p = fn if rel == '.' else rel + '/' + fn
            if p not in keep:
                os.unlink(os.path.join(dest, p))
                deleted.append(p)
    out({'status': 'ok', 'response_fresh': True, 'checkout_name': name, 'files': len(keep),
         'blobs_sent': sent, 'bytes_sent': sent_bytes, 'blobs_missing': [],
         'receiver': {'mode': 'sync', 'checkout': dest, 'created': created,
                      'files': len(keep), 'unchanged': unchanged, 'written': written,
                      'mode_fixed': 0, 'deleted': len(deleted), 'deleted_paths': sorted(deleted),
                      'kept_non_source': 0, 'blobs_received': sent,
                      'bytes_received': sent_bytes}})
if cmd == 'fetch':
    dest, store = opt('--dest'), opt('--store')
    if os.path.lexists(dest):
        out({'status': 'error', 'stage': 'local', 'error': 'local: Exists'}, 3)
    os.makedirs(store, exist_ok=True)
    src = os.path.join(remote, opt('--checkout'))
    n = got = got_bytes = 0
    for rel in opt('--path', many=True):
        for dirpath, _, filenames in (os.walk(os.path.join(src, rel))
                                      if os.path.isdir(os.path.join(src, rel))
                                      else [(os.path.dirname(os.path.join(src, rel)), [],
                                             [os.path.basename(rel)])]):
            for fn in filenames:
                full = os.path.join(dirpath, fn)
                data = open(full, 'rb').read()
                blob = os.path.join(store, hashlib.blake2s(data).hexdigest())
                if not os.path.exists(blob):
                    open(blob, 'wb').write(data)
                    got, got_bytes = got + 1, got_bytes + len(data)
                target = os.path.join(dest, os.path.relpath(full, src))
                os.makedirs(os.path.dirname(target), exist_ok=True)
                shutil.copy2(full, target)
                n += 1
    out({'status': 'ok', 'files': n, 'blobs_received': got, 'bytes_received': got_bytes,
         'blobs_missing': [], 'dest': dest, 'skipped': []})
out({'status': 'error', 'stage': 'usage', 'error': 'unknown command'}, 2)
'''


class OffloadFixture(unittest.TestCase):
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


class OffloadTests(OffloadFixture):
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


class IrohTransportTests(OffloadFixture):
    """--transport iroh against the fake lan-peer: content-addressed sync, ssh only for run."""

    def setUp(self):
        super().setUp()
        base = Path(self.tmp.name)
        self.peer_log = base / 'lan-peer.log'
        fake = self.bin / 'fake-lan-peer'
        fake.write_text(FAKE_LAN_PEER)
        fake.chmod(0o755)
        self.allow = base / 'worker-attestation.json'
        self.allow.write_text(json.dumps({'synthetic': True, 'account_did': 'did:key:zSynthetic',
                                          'endpoint_id': 'a' * 64, 'attestation': {}}))
        self.env.update(TASKS_OFFLOAD_LAN_PEER=str(fake), FAKE_LAN_PEER_LOG=str(self.peer_log))
        self.iroh = ('--transport', 'iroh', '--iroh-data-dir', str(base / 'peer-data'),
                     '--iroh-allow', str(self.allow), '--iroh-addr', '127.0.0.1:7842')

    def peer_calls(self):
        if not self.peer_log.exists():
            return []
        return [json.loads(line) for line in self.peer_log.read_text().splitlines()]

    def test_iroh_sync_runs_in_reported_checkout_without_rsync_or_agent(self):
        self.write('src/lib.rs', 'pub fn dirty() {}\n')
        r = self.offload('pwd', opts=self.iroh, check_exit=0)
        s = self.summary(r)
        root = self.remote_dir()
        self.assertEqual((root / 'src/lib.rs').read_text(), 'pub fn dirty() {}\n')
        self.assertEqual(s['transport'], 'iroh')
        self.assertEqual(s['remote_dir'], str(root))
        rec = self.job_record(s['remote_job_id'])
        self.assertEqual(rec['cwd'], str(root))
        self.assertTrue(rec['who'].startswith('offload-test@'))
        sync = s['iroh']['sync']
        self.assertEqual((sync['files'], sync['blobs_sent'], sync['written']), (2, 2, 2))
        calls = self.peer_calls()
        self.assertEqual([c[0] for c in calls], ['manifest', 'sync-push'])
        push = calls[1]
        self.assertEqual(push[push.index('--checkout') + 1], root.name)
        self.assertEqual(push[push.index('--peer') + 1], 'a' * 64)
        self.assertEqual(push[push.index('--addr') + 1], '127.0.0.1:7842')
        self.assertNotIn('--bind', push)
        ssh = self.ssh_log.read_text()
        self.assertNotIn('rsync', ssh)
        self.assertNotIn('install-agent', ssh)
        self.assertNotIn('agent.py', ssh)
        # The same checkout id as the ssh transport: the warm target is shared.
        r = self.offload('true', check_exit=0)
        self.assertEqual(self.summary(r)['remote_dir'], '~/offload/checkouts/' + root.name)

    def test_iroh_second_sync_sends_nothing_and_one_edit_sends_one_blob(self):
        self.write('a.txt', 'A\n')
        self.offload('true', opts=self.iroh, check_exit=0)
        r = self.offload('true', opts=self.iroh, check_exit=0)
        sync = self.summary(r)['iroh']['sync']
        self.assertEqual((sync['blobs_sent'], sync['bytes_sent'], sync['written']), (0, 0, 0))
        self.assertEqual(sync['unchanged'], 3)
        self.write('a.txt', 'A2\n')
        (self.repo / 'src/lib.rs').unlink()
        self.git('rm', '-q', '--cached', 'src/lib.rs')
        r = self.offload('true', opts=self.iroh, check_exit=0)
        s = self.summary(r)
        self.assertEqual(s['iroh']['sync']['blobs_sent'], 1)
        self.assertEqual(s['pruned'], ['src/lib.rs'])
        self.assertEqual(s['pruned_count'], 1)

    def test_iroh_bind_is_forwarded(self):
        self.offload('true', opts=(*self.iroh, '--iroh-bind', '127.0.0.1:0'), check_exit=0)
        push = self.peer_calls()[1]
        self.assertEqual(push[push.index('--bind') + 1], '127.0.0.1:0')

    def test_iroh_requires_peer_options_and_valid_attestation_file(self):
        for drop in ('--iroh-data-dir', '--iroh-allow', '--iroh-addr'):
            opts = list(self.iroh)
            i = opts.index(drop)
            del opts[i:i + 2]
            r = self.offload('true', opts=opts, check_exit=2)
            self.assertIn(drop, r.stderr)
        self.allow.write_text('{"endpoint_id": "../../x"}')
        self.offload('true', opts=self.iroh, check_exit=2)
        self.assertFalse(self.ssh_log.exists())
        self.assertEqual(self.peer_calls(), [])

    def test_iroh_refuses_symlinks_before_contacting_anything(self):
        os.symlink('src/lib.rs', self.repo / 'link.rs')
        r = self.offload('true', opts=self.iroh, check_exit=2)
        self.assertIn('symlink', r.stderr)
        self.assertIn('--transport ssh', r.stderr)
        self.assertEqual(self.peer_calls(), [])
        self.assertFalse(self.ssh_log.exists())

    def test_iroh_hash_disagreement_refuses_to_sync(self):
        self.env['FAKE_LAN_PEER_CORRUPT'] = '1'
        r = self.offload('true', opts=self.iroh, check_exit=2)
        self.assertIn('changed', r.stderr)
        self.assertEqual([c[0] for c in self.peer_calls()], ['manifest'])
        self.assertFalse(self.ssh_log.exists())

    def test_iroh_rejection_is_reported_and_nothing_submitted(self):
        self.env['FAKE_LAN_PEER_FAIL'] = 'sync-push'
        r = self.offload('true', opts=self.iroh, check_exit=255)
        self.assertIn('NotAllowlisted', r.stderr)
        s = self.summary(r)
        self.assertEqual(s['remote_state'], 'not_submitted')
        self.assertFalse(self.ssh_log.exists())

    def test_iroh_fetch_into_new_destinations_reuses_local_blobs(self):
        build = ('sh', '-c', 'mkdir -p target/release && echo artifact > target/release/foo')
        r = self.offload(*build, opts=(*self.iroh, '--fetch', 'target/release/foo'),
                         check_exit=0)
        s = self.summary(r)
        dest = self.local_home / 'build-cache/offload/offload-test' / str(s['remote_job_id'])
        self.assertEqual((dest / 'target/release/foo').read_text(), 'artifact\n')
        self.assertEqual(s['fetched'], ['target/release/foo'])
        self.assertEqual(s['fetch_dest'], str(dest))
        self.assertEqual(s['iroh']['fetch']['bytes_received'], len('artifact\n'))
        fetch = [c for c in self.peer_calls() if c[0] == 'fetch'][0]
        self.assertEqual(fetch[fetch.index('--store') + 1],
                         str(self.local_home / 'build-cache/offload/.iroh-store'))
        self.assertEqual(fetch[fetch.index('--checkout') + 1], self.remote_dir().name)
        r = self.offload(*build, opts=(*self.iroh, '--fetch', 'target/release/foo'),
                         check_exit=0)
        s2 = self.summary(r)
        self.assertNotEqual(s2['fetch_dest'], s['fetch_dest'])
        self.assertEqual((s2['iroh']['fetch']['blobs_received'],
                          s2['iroh']['fetch']['bytes_received']), (0, 0))

    def test_iroh_existing_fetch_dest_refused_before_submission(self):
        existing = Path(self.tmp.name) / 'already-there'
        existing.mkdir()
        r = self.offload('true', opts=(*self.iroh, '--fetch', 'x', '--fetch-dest', str(existing)),
                         check_exit=2)
        self.assertIn('must not exist', r.stderr)
        self.assertFalse(self.ssh_log.exists())

    def test_iroh_dry_run_contacts_nothing(self):
        r = self.offload('true', opts=(*self.iroh, '--dry-run'), check_exit=0)
        self.assertEqual(self.peer_calls(), [])
        self.assertFalse(self.ssh_log.exists())
        self.assertIn('iroh', r.stderr)
        self.assertEqual(self.summary(r)['transport'], 'iroh')


LAN_PEER_BIN = os.environ.get('LAN_PEER_BIN') or str(
    Path(__file__).resolve().parents[1]
    / 'experiments/osvauld-foundation/target/debug/lan-peer')


@unittest.skipUnless(os.access(LAN_PEER_BIN, os.X_OK),
                     f'real lan-peer binary not built ({LAN_PEER_BIN})')
class IrohLoopbackTests(OffloadFixture):
    """--transport iroh against a REAL `lan-peer serve` on 127.0.0.1 (synthetic account)."""

    def setUp(self):
        super().setUp()
        base = Path(self.tmp.name)
        self.server_data = base / 'lan-peer-server'
        self.client_data = base / 'lan-peer-client'
        client = self.lan_peer('init', '--data-dir', str(self.client_data))
        checkouts = self.remote_home / 'offload/checkouts'
        checkouts.mkdir(parents=True)
        self.server = subprocess.Popen(
            [LAN_PEER_BIN, 'serve', '--bind', '127.0.0.1:0', '--data-dir', str(self.server_data),
             '--allow', client['attestation_file'], '--store', str(base / 'server-store'),
             '--checkouts', str(checkouts)],
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        self.addCleanup(self.stop_server)
        ready = json.loads(self.server.stdout.readline())
        self.assertEqual(ready['event'], 'listening', ready)
        self.iroh = ('--transport', 'iroh', '--lan-peer', LAN_PEER_BIN,
                     '--iroh-data-dir', str(self.client_data),
                     '--iroh-allow', ready['attestation_file'], '--iroh-addr', ready['addr'])

    def stop_server(self):
        self.server.kill()
        self.server.wait()
        self.server.stdout.close()

    def lan_peer(self, *args):
        out = subprocess.run([LAN_PEER_BIN, *args], capture_output=True, text=True, timeout=120)
        self.assertEqual(out.returncode, 0, out.stdout + out.stderr)
        return json.loads(out.stdout.strip().splitlines()[-1])

    def test_real_loopback_sync_run_and_fetch(self):
        self.write('a.txt', 'A\n')
        self.write('run.sh', '#!/bin/sh\necho ran-$1\n', mode=0o755)
        r = self.offload('./run.sh', 'one', opts=self.iroh, check_exit=0)
        s = self.summary(r)
        self.assertIn('ran-one', r.stdout)
        root = Path(s['remote_dir'])
        self.assertEqual(root, self.remote_dir())
        self.assertEqual(s['iroh']['sync']['blobs_sent'], 4)
        self.assertTrue(os.access(root / 'run.sh', os.X_OK))
        os.utime(root / 'a.txt', ns=(10**18, 10**18))

        r = self.offload('true', opts=self.iroh, check_exit=0)
        sync = self.summary(r)['iroh']['sync']
        self.assertEqual((sync['blobs_sent'], sync['bytes_sent'], sync['written']), (0, 0, 0))
        self.assertEqual((root / 'a.txt').stat().st_mtime_ns, 10**18)

        self.write('run.sh', '#!/bin/sh\necho ran2-$1\n', mode=0o755)
        build = ('sh', '-c', 'mkdir -p target/debug && printf bin > target/debug/app')
        r = self.offload(*build, opts=(*self.iroh, '--fetch', 'target/debug/app'), check_exit=0)
        s = self.summary(r)
        self.assertEqual(s['iroh']['sync']['blobs_sent'], 1)
        self.assertEqual((root / 'a.txt').stat().st_mtime_ns, 10**18)
        self.assertEqual(Path(s['fetch_dest'], 'target/debug/app').read_text(), 'bin')
        self.assertEqual(s['iroh']['fetch']['bytes_received'], 3)

        r = self.offload('true', opts=(*self.iroh, '--fetch', 'target/debug/app'), check_exit=0)
        s = self.summary(r)
        self.assertEqual(Path(s['fetch_dest'], 'target/debug/app').read_text(), 'bin')
        self.assertEqual((s['iroh']['fetch']['blobs_received'],
                          s['iroh']['fetch']['bytes_received']), (0, 0))
        self.assertTrue((root / 'target/debug/app').is_file(), 'target is never pruned')


if __name__ == '__main__':
    unittest.main()
