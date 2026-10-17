#!/usr/bin/env python3
"""tasks-offload: run one command on a LAN worker's own task manager (SSH v0).

Usage (from inside a Git checkout; HEAD may be unborn):

  tasks-offload --worker USER@HOST [--ssh-key PATH] --who OWNER
                [--fetch RELPATH]... [--fetch-dest DIR] [--cargo-fetch]
                [--remote-run | --device SERIAL] [--checksum] [--dry-run]
                [--transport ssh|iroh [--iroh-data-dir DIR --iroh-allow FILE
                 --iroh-addr IP:PORT [--iroh-bind IP:PORT] [--iroh-store DIR]
                 [--lan-peer PATH]]] -- CMD [ARGS...]

Steps: select the checkout's permitted files (git ls-files -c -o
--exclude-standard, filtered by worker_source path rules), rsync them to
~/offload/checkouts/<id>/ on the worker, prune/verify there with a small
installed helper, then `tasks run --who OWNER@<local-host> -- CMD` on the
worker. Prints a one-line JSON summary as the last stdout line.

Transport v0 is ssh + rsync only (key-only, BatchMode). `--transport iroh`
(experimental, phase 2b) replaces only the file movement: the same selected files
are BLAKE3-hashed by `lan-peer manifest`, pushed as content-addressed blobs (only
the ones the worker lacks) into the same stable checkout id with `lan-peer
sync-push`, and `--fetch` returns artifacts with `lan-peer fetch` into a NEW
directory. The command itself still runs over ssh with the worker's `tasks run`;
iroh never executes anything. See scripts/README-offload.md.

Exit status: the remote job's exit code; 2 usage/source refusal; 255 transport
failure or UNCERTAIN remote state (summary `remote_state` disambiguates);
130/143 after cancelling the remote job on SIGINT/SIGTERM; 1 fetch failure.
"""
import argparse
from dataclasses import dataclass
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import worker_source  # noqa: E402  (path rules are shared, not duplicated)

SCRIPTS_DIR = Path(__file__).resolve().parent
# Installed on the worker as ~/.local/lib/tasks-offload/<name>.
AGENT_FILES = {'agent.py': SCRIPTS_DIR / 'offload_agent.py',
               'worker_source.py': SCRIPTS_DIR / 'worker_source.py'}
REMOTE_LIB = '.local/lib/tasks-offload'
REMOTE_TASKS = '"$HOME"/.local/bin/tasks'
SUBMITTED_RE = re.compile(r'^tasks: submitted job (\d+);')
FINAL_RE = re.compile(r'^job (\d+) ([a-z_]+) \(exit (-?\d+)\) owner=')
OWNER_RE = re.compile(r'^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$')
WORKER_RE = re.compile(r'^([A-Za-z0-9._-]+@)?[A-Za-z0-9._:-]+$')
EXIT_USAGE, EXIT_TRANSPORT, EXIT_FETCH = 2, 255, 1
ENDPOINT_RE = re.compile(r'^[0-9a-z]{52,64}$')
# Device serial as the worker's `tasks run --device` accepts it (`:` maps to `_`).
DEVICE_RE = re.compile(r'^[A-Za-z0-9._-]{1,128}$')
# Devices agents must never use, mirrored from src/device_denylist.rs. The
# built-in entries cannot be removed; TASKS_DENIED_DEVICES and
# $XDG_CONFIG_HOME/tasks/denied-devices only add entries.
DENIED_DEVICES_BUILTIN = ('ZD22257P6G',)  # the user's personal phone (moto g52)
DENIED_DEVICES_ENV = 'TASKS_DENIED_DEVICES'
DENIED_ENTRY_RE = re.compile(r'^[A-Za-z0-9._:-]{6,128}$')


class OffloadError(Exception):
    def __init__(self, message, code=EXIT_USAGE):
        super().__init__(message)
        self.code = code


def log(message):
    sys.stderr.write(f'tasks-offload: {message}\n')
    sys.stderr.flush()


@dataclass(frozen=True)
class Bounds:
    max_files: int = 50000
    max_file_bytes: int = 1 << 30
    max_bytes: int = 4 << 30


# --------------------------------------------------------------------------
# Source set
# --------------------------------------------------------------------------

def git_toplevel(cwd):
    try:
        out = subprocess.run(['git', 'rev-parse', '--show-toplevel'], cwd=cwd,
                             capture_output=True, check=True).stdout
    except (OSError, subprocess.CalledProcessError) as error:
        raise OffloadError('not inside a git checkout (git rev-parse --show-toplevel failed)') \
            from error
    return os.path.realpath(os.fsdecode(out.rstrip(b'\n')))


def git_paths(top):
    # --recurse-submodules cannot be combined with -o; submodules are unsupported
    # and their gitlink directories are refused below.
    try:
        out = subprocess.run(['git', 'ls-files', '-z', '-c', '-o', '--exclude-standard'],
                             cwd=top, capture_output=True, check=True).stdout
    except (OSError, subprocess.CalledProcessError) as error:
        raise OffloadError(f'git ls-files failed: {error}') from None
    return sorted({os.fsdecode(p) for p in out.split(b'\0') if p})


def _hash_file(parent_fd, name, meta, bounds):
    fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
                 dir_fd=parent_fd)
    digest, size = hashlib.sha256(), 0
    with os.fdopen(fd, 'rb') as f:
        for chunk in iter(lambda: f.read(1 << 20), b''):
            size += len(chunk)
            if size > bounds.max_file_bytes:
                raise OffloadError(f'{name}: file exceeds {bounds.max_file_bytes} bytes')
            digest.update(chunk)
        after = os.fstat(f.fileno())
    if size != after.st_size or after.st_mtime_ns != meta.st_mtime_ns:
        raise OffloadError(f'{name}: changed while hashing; keep the checkout stable and retry')
    return digest.hexdigest(), size


def build_manifest(top, bounds=Bounds()):
    """Stream-hash the permitted source set; never holds file contents in memory."""
    files, excluded, deleted, total = [], [], 0, 0
    root_fd, _ = worker_source.open_root(top)
    try:
        for path in git_paths(top):
            if path.endswith('/'):
                raise OffloadError(f'{path}: nested git repository or submodule; unsupported')
            try:
                path.encode('utf-8')
            except UnicodeEncodeError:
                raise OffloadError(f'{path!r}: non-UTF-8 path unsupported') from None
            try:
                parts = worker_source.check_path(path)
                parent = worker_source.open_parent(root_fd, parts)
            except ValueError as error:
                if str(error).startswith('excluded'):
                    excluded.append(path)
                    continue
                raise OffloadError(f'{path}: {error}') from None
            except FileNotFoundError:
                deleted += 1
                continue
            except OSError as error:
                raise OffloadError(f'{path}: parent is a symlink or not a directory ({error})') \
                    from None
            try:
                try:
                    meta = os.stat(parts[-1], dir_fd=parent, follow_symlinks=False)
                except FileNotFoundError:
                    deleted += 1  # tracked but deleted in the worktree
                    continue
                if stat.S_ISLNK(meta.st_mode):
                    target = os.readlink(parts[-1], dir_fd=parent)
                    resolved = os.path.normpath(os.path.join(os.path.dirname(path), target))
                    if os.path.isabs(target) or resolved == '..' or resolved.startswith('../'):
                        raise OffloadError(f'{path}: symlink escapes the checkout ({target})')
                    raw = os.fsencode(target)
                    entry = {'path': path, 'type': 'link', 'size': len(raw), 'mode': 'link',
                             'executable': False, 'sha256': hashlib.sha256(raw).hexdigest()}
                elif stat.S_ISREG(meta.st_mode):
                    if meta.st_size > bounds.max_file_bytes:
                        raise OffloadError(f'{path}: file exceeds {bounds.max_file_bytes} bytes')
                    digest, size = _hash_file(parent, parts[-1], meta, bounds)
                    entry = {'path': path, 'type': 'file', 'size': size,
                             'mode': oct(meta.st_mode & 0o7777),
                             'executable': bool(meta.st_mode & 0o111), 'sha256': digest}
                elif stat.S_ISDIR(meta.st_mode):
                    raise OffloadError(f'{path}: is a directory (git submodule or nested '
                                       'repository); submodules are unsupported')
                else:
                    raise OffloadError(f'{path}: special file unsupported')
            finally:
                os.close(parent)
            files.append(entry)
            total += entry['size']
            if len(files) > bounds.max_files:
                raise OffloadError(f'source set exceeds {bounds.max_files} files')
            if total > bounds.max_bytes:
                raise OffloadError(f'source set exceeds {bounds.max_bytes} bytes')
    finally:
        os.close(root_fd)
    return {'schema': 'tasks-offload-manifest-v0', 'files': files, 'excluded': excluded,
            'deleted_tracked': deleted, 'bytes': total}


def _device_key(serial):
    return serial.replace(':', '_').upper()


def denied_devices_config_path():
    xdg = os.environ.get('XDG_CONFIG_HOME')
    base = Path(xdg) if xdg and os.path.isabs(xdg) else Path.home() / '.config'
    return base / 'tasks/denied-devices'


def load_denylist():
    """[(serial, source)]: built-in first, then env, then the config file. Fails closed."""
    entries = [(s, 'built-in') for s in DENIED_DEVICES_BUILTIN]

    def add(serial, source):
        if not DENIED_ENTRY_RE.match(serial):
            raise OffloadError(f'{source}: invalid denied device serial {serial!r} (6-128 '
                               'characters of A-Z a-z 0-9 . _ - :); fix the device denylist')
        entries.append((serial, source))

    for serial in re.split(r'[,\s]+', os.environ.get(DENIED_DEVICES_ENV, '')):
        if serial:
            add(serial, DENIED_DEVICES_ENV)
    path = denied_devices_config_path()
    try:
        text = path.read_text()
    except FileNotFoundError:
        text = ''
    except OSError as error:
        raise OffloadError(f'device denylist {path}: {error}')
    for line in text.splitlines():
        serial = line.split('#', 1)[0].strip()
        if serial:
            add(serial, str(path))
    return entries


def _refusal(what, serial, source):
    return OffloadError(f'{what} denylisted device {serial} ({source}): agents must never '
                        'use it on any host. Use a dedicated test device (see '
                        'deploy/worker/README.md, "Test phone on the worker")')


def check_devices(device, command):
    """Refuse a denylisted --device and any argv element naming a denylisted serial."""
    if device is not None and (not DEVICE_RE.match(device.replace(':', '_'))
                               or device.startswith('-')):
        raise OffloadError(f'invalid device serial {device!r} for --device')
    entries = load_denylist()
    if device is not None:
        for serial, source in entries:
            if _device_key(serial) == _device_key(device):
                raise _refusal('refusing', serial, source)
    keys = [_device_key(a) for a in command]
    for serial, source in entries:
        if any(_device_key(serial) in a for a in keys):
            raise _refusal('refusing a command that names', serial, source)


def checkout_id(hostname, top):
    return hashlib.sha256(f'{hostname}\0{top}'.encode('utf-8', 'surrogateescape')).hexdigest()[:16]


def safe_relpath(value, what):
    parts = value.split('/')
    if (not value or value.startswith('/') or any(p in ('', '.', '..') for p in parts)
            or any(ord(c) < 32 for c in value)):
        raise OffloadError(f'{what} must be a normalized relative path: {value!r}')
    return value


def lock_checkout(cid):
    """Serialize offloads of one checkout: a second sync would rewrite the
    worker copy under the first job's build. Held until this process exits."""
    lock_dir = Path(os.environ.get('XDG_STATE_HOME') or Path.home() / '.local/state')
    lock_dir = lock_dir / 'tasks-offload/locks'
    lock_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd = os.open(lock_dir / f'{cid}.lock', os.O_RDWR | os.O_CREAT | os.O_CLOEXEC, 0o600)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        log('waiting for another offload of this checkout to finish (its worker copy is in use)')
        fcntl.flock(fd, fcntl.LOCK_EX)
    return fd


def _inside(child, parent):
    child, parent = os.path.realpath(child), os.path.realpath(parent)
    return child == parent or child.startswith(parent.rstrip('/') + '/')


# --------------------------------------------------------------------------
# Transport seam (ssh + rsync v0). iroh replaces this class later.
# --------------------------------------------------------------------------

class Transport:
    def __init__(self, worker, ssh_program, rsync_program, ssh_key=None):
        self.worker = worker
        self.ssh_argv = [ssh_program, '-T', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=15',
                         '-o', 'ServerAliveInterval=15', '-o', 'ServerAliveCountMax=4']
        if ssh_key:
            self.ssh_argv += ['-i', ssh_key, '-o', 'IdentitiesOnly=yes']
        self.rsync = rsync_program

    def ssh(self, command):
        """argv for one remote POSIX-shell command string (caller quotes it)."""
        return [*self.ssh_argv, '--', self.worker, command]

    def run(self, command, *, input=None, check=True, what='ssh'):
        result = subprocess.run(self.ssh(command), input=input, capture_output=True)
        if check and result.returncode != 0:
            detail = result.stderr.decode(errors='replace').strip()[-400:]
            raise OffloadError(f'{what} failed (exit {result.returncode}): {detail}',
                               EXIT_TRANSPORT)
        return result

    def rsync_argv(self, *extra):
        return [self.rsync, '-a', '-s', '-e', shlex.join(self.ssh_argv), *extra]


def remote_path(rel):
    return '"$HOME"/' + shlex.quote(rel)


def install_agent(transport, remote_python):
    names = ' '.join(AGENT_FILES)
    check = transport.run(f'cd "$HOME" && for f in {names}; do '
                          f'if [ -f {REMOTE_LIB}/$f ]; then sha256sum {REMOTE_LIB}/$f; fi; done',
                          what='agent hash check')
    have = {}
    for line in check.stdout.decode(errors='replace').splitlines():
        digest, _, path = line.partition('  ')
        have[os.path.basename(path.strip())] = digest
    installed = []
    for name, source in AGENT_FILES.items():
        data = source.read_bytes()
        if have.get(name) == hashlib.sha256(data).hexdigest():
            continue
        dest = remote_path(f'{REMOTE_LIB}/{name}')
        tmp = remote_path(f'{REMOTE_LIB}/.{name}.tmp')
        transport.run(f': install-agent; umask 022 && mkdir -p {remote_path(REMOTE_LIB)} && '
                      f'cat > {tmp} && mv -f {tmp} {dest}', input=data, what='agent install')
        installed.append(name)
    return [remote_python, '-I', '-B', f'{REMOTE_LIB}/agent.py']


def agent_call(transport, agent, args, *, input=None):
    command = 'cd "$HOME" && exec ' + shlex.join([*agent, *args])
    result = transport.run(command, input=input, check=False)
    try:
        reply = json.loads(result.stdout.decode().strip().splitlines()[-1])
    except (ValueError, IndexError):
        detail = result.stderr.decode(errors='replace').strip()[-400:]
        raise OffloadError(f'remote helper failed (exit {result.returncode}; is python3 on the '
                           f'worker PATH?): {detail}', EXIT_TRANSPORT) from None
    if result.returncode not in (0, 3):
        raise OffloadError(f'remote helper refused: {reply.get("error")}', EXIT_TRANSPORT)
    return reply


def rsync_files(transport, top, paths, remote_rel, checksum):
    with tempfile.NamedTemporaryFile(prefix='tasks-offload-files-') as listing:
        listing.write(b''.join(os.fsencode(p) + b'\0' for p in paths))
        listing.flush()
        argv = transport.rsync_argv('--from0', f'--files-from={listing.name}',
                                    *(['--checksum'] if checksum else []),
                                    '--', f'{top}/', f'{transport.worker}:{remote_rel}/')
        result = subprocess.run(argv, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    if result.returncode != 0:
        raise OffloadError('rsync to worker failed (exit %d): %s' % (
            result.returncode, result.stderr.decode(errors='replace').strip()[-400:]),
            EXIT_TRANSPORT)


# --------------------------------------------------------------------------
# iroh transport (experimental): lan-peer moves files, ssh still runs the job
# --------------------------------------------------------------------------

class IrohPeer:
    """Arguments for `lan-peer` subcommands that dial the worker.

    The worker endpoint id is read from its attestation file, which lan-peer
    itself verifies (account DID, expiry, endpoint binding) before dialing.
    """

    def __init__(self, args):
        missing = [flag for flag, value in (('--iroh-data-dir', args.iroh_data_dir),
                                            ('--iroh-allow', args.iroh_allow),
                                            ('--iroh-addr', args.iroh_addr)) if not value]
        if missing:
            raise OffloadError('--transport iroh needs ' + ', '.join(missing))
        try:
            with open(args.iroh_allow, encoding='utf-8') as f:
                peer = json.load(f).get('endpoint_id')
        except (OSError, ValueError, AttributeError) as error:
            raise OffloadError(f'--iroh-allow {args.iroh_allow}: {error}') from None
        if not isinstance(peer, str) or not ENDPOINT_RE.match(peer):
            raise OffloadError(f'--iroh-allow {args.iroh_allow}: no valid endpoint_id')
        self.program = args.lan_peer
        self.peer = peer
        self.addr = args.iroh_addr
        self.dial = ['--peer', peer, '--addr', args.iroh_addr,
                     '--data-dir', args.iroh_data_dir, '--allow', args.iroh_allow]
        if args.iroh_bind:
            self.dial += ['--bind', args.iroh_bind]
        self.store = args.iroh_store or str(Path.home() / 'build-cache/offload/.iroh-store')

    def call(self, argv, what):
        """Runs lan-peer; returns its last-line JSON or raises OffloadError."""
        try:
            result = subprocess.run([self.program, *argv], stdin=subprocess.DEVNULL,
                                    capture_output=True)
        except OSError as error:
            raise OffloadError(f'{what}: cannot run lan-peer ({self.program}): {error}',
                               EXIT_TRANSPORT) from None
        try:
            reply = json.loads(result.stdout.decode().strip().splitlines()[-1])
        except (ValueError, IndexError):
            detail = result.stderr.decode(errors='replace').strip()[-400:]
            raise OffloadError(f'{what}: lan-peer exit {result.returncode}: {detail}',
                               EXIT_TRANSPORT) from None
        if result.returncode != 0:
            stage = reply.get('stage')
            reason = reply.get('reason') or reply.get('error')
            code = EXIT_USAGE if stage in ('usage', 'source') else EXIT_TRANSPORT
            raise OffloadError(f'{what} failed (lan-peer exit {result.returncode}, stage '
                               f'{stage}): {reason}', code)
        return reply


def source_manifest(entries):
    """worker_source schema (SHA-256 receipt) for the selected regular files."""
    files = sorted(({'path': e['path'], 'size': e['size'], 'executable': e['executable'],
                     'sha256': e['sha256'], 'iroh_blobs_hash': None} for e in entries),
                   key=lambda f: f['path'].encode('utf-8'))
    digest = hashlib.sha256(worker_source.DOMAIN)
    for f in files:
        path = f['path'].encode('utf-8')
        digest.update(len(path).to_bytes(8, 'little'))
        digest.update(path)
        digest.update(bytes([f['executable']]))
        digest.update(f['size'].to_bytes(8, 'little'))
        digest.update(bytes.fromhex(f['sha256']))
    return {'schema': worker_source.SCHEMA, 'hash_algorithm': 'sha256',
            'receipt': digest.hexdigest(), 'iroh_blobs_hash': None, 'files': files}


def iroh_sync(peer, top, manifest, cid, workdir):
    """BLAKE3-hash the selection with `lan-peer manifest`, check it describes exactly the
    bytes we scanned, then `lan-peer sync-push` into the stable checkout `cid`."""
    listing = Path(workdir) / 'files0'
    listing.write_bytes(b''.join(f['path'].encode('utf-8') + b'\0' for f in manifest['files']))
    hashed = peer.call(['manifest', top, '--files-from0', str(listing)], 'lan-peer manifest')
    try:
        merged = worker_source.attach_iroh_blobs_hashes(source_manifest(manifest['files']),
                                                       hashed)
    except ValueError as error:
        raise OffloadError(f'checkout changed while hashing ({error}); keep it stable and '
                           'retry') from None
    manifest_path = Path(workdir) / 'manifest.json'
    manifest_path.write_text(json.dumps(merged))
    reply = peer.call(['sync-push', *peer.dial, '--source', top, '--manifest',
                       str(manifest_path), '--checkout', cid], 'iroh sync')
    checkout = (reply.get('receiver') or {}).get('checkout')
    if (not isinstance(checkout, str) or not checkout.startswith('/')
            or os.path.basename(checkout) != cid or any(ord(c) < 32 for c in checkout)):
        raise OffloadError(f'iroh sync: worker reported an unexpected checkout {checkout!r}',
                           EXIT_TRANSPORT)
    return reply


# --------------------------------------------------------------------------
# Remote run with live output, cancel and uncertainty reporting
# --------------------------------------------------------------------------

class RemoteRun:
    def __init__(self, transport, command):
        self.transport = transport
        self.job_id = None
        self.final = None  # (job, phase, exit)
        self.submitted = threading.Event()
        self.cancel_wanted = False
        self.canceller = None
        # Own session: terminal/host group signals reach only this client,
        # which then cancels the remote job instead of silently detaching it.
        self.proc = subprocess.Popen(transport.ssh(command), stdin=subprocess.DEVNULL,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                     start_new_session=True)
        self.threads = [threading.Thread(target=self._pump, args=(self.proc.stdout, sys.stdout),
                                         daemon=True),
                        threading.Thread(target=self._pump, args=(self.proc.stderr, sys.stderr),
                                         daemon=True)]
        for t in self.threads:
            t.start()

    def _pump(self, stream, sink):
        for line in iter(stream.readline, b''):
            sink.buffer.write(line)
            sink.flush()
            text = line.decode(errors='replace').rstrip('\n')
            if (m := SUBMITTED_RE.match(text)) and self.job_id is None:
                self.job_id = int(m.group(1))
                self.submitted.set()
                if self.cancel_wanted:
                    self.start_cancel()
            elif m := FINAL_RE.match(text):
                # stdout/stderr threads may interleave; reconciled in wait().
                self.final = (int(m.group(1)), m.group(2), int(m.group(3)))

    def wait(self, timeout=None):
        try:
            self.proc.wait(timeout)
        except subprocess.TimeoutExpired:
            return None
        for t in self.threads:
            t.join(5)
        if self.final and self.job_id is None:
            self.job_id = self.final[0]
        elif self.final and self.final[0] != self.job_id:
            self.final = None  # not our job's terminal line
        return self.proc.returncode

    def kill(self):
        if self.proc.poll() is None:
            self.proc.kill()
        return self.wait()

    def start_cancel(self):
        """Idempotent; called from the signal handler itself because the host
        worker escalates SIGTERM to SIGKILL after ~300 ms. The canceller runs in
        its own session with output in an unlinked temp file, so it completes
        even if this client is killed. Only our own job id is ever cancelled."""
        self.cancel_wanted = True
        if self.canceller is not None or self.job_id is None:
            return
        sink = tempfile.TemporaryFile()
        command = f'exec {REMOTE_TASKS} cancel {int(self.job_id)}'
        self.canceller = (subprocess.Popen(self.transport.ssh(command),
                                           stdin=subprocess.DEVNULL, stdout=sink, stderr=sink,
                                           start_new_session=True), sink)


# --------------------------------------------------------------------------
# Main flow
# --------------------------------------------------------------------------

def parse_args(argv):
    parser = argparse.ArgumentParser(
        prog='tasks-offload', description=__doc__.split('\n\n')[0],
        usage='tasks-offload --worker USER@HOST [--ssh-key PATH] --who OWNER [options] '
              '-- CMD [ARGS...]')
    parser.add_argument('--worker', required=True, help='ssh destination USER@HOST')
    parser.add_argument('--ssh-key', help='identity file (IdentitiesOnly=yes)')
    parser.add_argument('--who', required=True, help='owner; remote owner is OWNER@<local-host>')
    parser.add_argument('--fetch', action='append', default=[],
                        help='path relative to the remote checkout to copy back; repeatable')
    parser.add_argument('--fetch-dest', help='default ~/build-cache/offload/OWNER/REMOTE_JOB_ID')
    parser.add_argument('--cargo-fetch', action='store_true',
                        help='run `cargo fetch --locked` on the worker (online) first')
    parser.add_argument('--remote-run', action='store_true',
                        help='submit with `tasks run --run` (run-only slot, must not compile)')
    parser.add_argument('--device', metavar='SERIAL',
                        help='submit with `tasks run --device SERIAL` on the worker (that '
                             "device's exclusive slot in the worker ledger)")
    parser.add_argument('--checksum', action='store_true', help='rsync --checksum')
    parser.add_argument('--dry-run', action='store_true',
                        help='build the manifest and print the plan; contact nothing')
    parser.add_argument('--ssh-program', default=os.environ.get('TASKS_OFFLOAD_SSH', 'ssh'))
    parser.add_argument('--rsync-program', default=os.environ.get('TASKS_OFFLOAD_RSYNC', 'rsync'))
    parser.add_argument('--remote-python', default='python3')
    parser.add_argument('--max-files', type=int, default=Bounds.max_files)
    parser.add_argument('--max-bytes', type=int, default=Bounds.max_bytes)
    parser.add_argument('--cancel-wait', type=float, default=120.0,
                        help='seconds to wait for the remote job to finish cancelling')
    parser.add_argument('--transport', choices=('ssh', 'iroh'), default='ssh',
                        help='file movement: ssh+rsync (default) or iroh blobs via lan-peer; '
                             'the command always runs over ssh')
    parser.add_argument('--lan-peer', default=os.environ.get('TASKS_OFFLOAD_LAN_PEER',
                                                             'lan-peer'),
                        help='lan-peer binary (iroh transport)')
    parser.add_argument('--iroh-data-dir', help='local lan-peer installation dir (iroh)')
    parser.add_argument('--iroh-allow', help="worker's attestation.json (iroh); its "
                                             'endpoint_id is the dialed peer')
    parser.add_argument('--iroh-addr', help='worker lan-peer serve address IP:PORT (iroh)')
    parser.add_argument('--iroh-bind', help='local LAN IP:0 to bind (required off loopback)')
    parser.add_argument('--iroh-store', help='local blob store for --fetch '
                                             '(default ~/build-cache/offload/.iroh-store)')
    if '--' not in argv:
        parser.error('-- CMD [ARGS...] is required')
    split = argv.index('--')
    args = parser.parse_args(argv[:split])
    args.command = argv[split + 1:]
    if not args.command:
        parser.error('empty command')
    return args


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    started = time.monotonic()
    args = parse_args(argv)
    summary = {'worker': args.worker, 'transport': args.transport, 'remote_dir': None, 'remote_cwd': None,
               'remote_job_id': None, 'remote_phase': None, 'remote_state': None,
               'exit': None, 'synced_files': 0, 'synced_bytes': 0, 'excluded_files': 0,
               'deleted_tracked': 0, 'pruned': [], 'fetched': [], 'fetch_dest': None,
               'cancel_requested': False, 'dry_run': args.dry_run, 'durations': {},
               'device': args.device}
    durations = summary['durations']

    def finish(code):
        summary['exit'] = code
        durations['total_s'] = round(time.monotonic() - started, 3)
        sys.stdout.flush()
        print(json.dumps(summary, sort_keys=True), flush=True)
        return code

    try:
        if not OWNER_RE.match(args.who):
            raise OffloadError('--who must match [A-Za-z0-9][A-Za-z0-9._-]{0,63}')
        if not WORKER_RE.match(args.worker) or args.worker.startswith('-'):
            raise OffloadError('--worker must be USER@HOST')
        if args.device is not None and args.remote_run:
            raise OffloadError('--device and --remote-run conflict: --device already selects '
                               "the worker's device slot (no compile slot)")
        check_devices(args.device, args.command)
        summary['device'] = args.device
        fetches = [safe_relpath(f, '--fetch') for f in args.fetch]
        hostname = socket.gethostname()
        remote_owner = f'{args.who}@{hostname}'
        top = git_toplevel(os.getcwd())
        cwd = os.path.realpath(os.getcwd())
        sub = os.path.relpath(cwd, top)
        if sub == '.':
            sub = ''
        if args.fetch_dest and _inside(args.fetch_dest, top):
            raise OffloadError('--fetch-dest must be outside the local checkout')
        peer = IrohPeer(args) if args.transport == 'iroh' else None
        if peer and fetches and args.fetch_dest and os.path.lexists(args.fetch_dest):
            raise OffloadError('--fetch-dest must not exist with --transport iroh '
                               '(artifacts are materialized into a new directory)')
        default_dest_root = Path.home() / 'build-cache/offload' / args.who
        if _inside(default_dest_root, top):
            raise OffloadError('default fetch destination would be inside the checkout')

        t = time.monotonic()
        bounds = Bounds(max_files=args.max_files, max_bytes=args.max_bytes)
        manifest = build_manifest(top, bounds)
        durations['scan_s'] = round(time.monotonic() - t, 3)
        paths = [f['path'] for f in manifest['files']]
        if peer:
            links = [f['path'] for f in manifest['files'] if f['type'] != 'file']
            if links:
                raise OffloadError(f'{links[0]}: in-tree symlinks are not supported by '
                                   '--transport iroh (its manifest carries regular files '
                                   'only); use --transport ssh')
            summary['iroh'] = {'peer': peer.peer, 'addr': peer.addr}
        summary.update(synced_files=len(paths), synced_bytes=manifest['bytes'],
                       excluded_files=len(manifest['excluded']),
                       deleted_tracked=manifest['deleted_tracked'])
        cid = checkout_id(hostname, top)
        remote_rel = f'offload/checkouts/{cid}'
        remote_cwd_rel = f'{remote_rel}/{sub}' if sub else remote_rel
        summary['remote_dir'] = f'~/{remote_rel}'
        summary['remote_cwd'] = f'~/{remote_cwd_rel}'
        if args.device is not None:
            resource = f'--device {shlex.quote(args.device)} '
        else:
            resource = '--run ' if args.remote_run else ''
        run_cmd = (f'mkdir -p {remote_path(remote_cwd_rel)} && '
                   f'cd {remote_path(remote_cwd_rel)} && exec {REMOTE_TASKS} run --who '
                   f'{shlex.quote(remote_owner)} {resource}-- {shlex.join(args.command)}')
        transport = Transport(args.worker, args.ssh_program, args.rsync_program, args.ssh_key)

        if args.dry_run:
            if peer:
                log(f'dry run: {len(paths)} files / {manifest["bytes"]} bytes would sync over '
                    f'iroh to {peer.peer[:12]}... at {peer.addr}, checkout {cid}')
            log(f'dry run: {len(paths)} files / {manifest["bytes"]} bytes would sync to '
                f'{args.worker}:~/{remote_rel}/ ({len(manifest["excluded"])} excluded)')
            for p in manifest['excluded'][:20]:
                log(f'  excluded: {p}')
            if args.cargo_fetch:
                log(f'would run: cd ~/{remote_cwd_rel} && cargo fetch --locked')
            log(f'would run: {shlex.join(transport.ssh(run_cmd))}')
            summary['remote_state'] = 'dry_run'
            return finish(0)

        checkout_lock = lock_checkout(cid)  # noqa: F841  (held until exit)
        t = time.monotonic()
        if peer:
            # Content-addressed sync; the worker locks, prunes and verifies the checkout.
            with tempfile.TemporaryDirectory(prefix='tasks-offload-iroh-') as workdir:
                reply = iroh_sync(peer, top, manifest, cid, workdir)
            receiver = reply['receiver']
            remote_abs = receiver['checkout']
            remote_cwd_abs = f'{remote_abs}/{sub}' if sub else remote_abs
            summary['remote_dir'] = remote_abs
            summary['remote_cwd'] = remote_cwd_abs
            summary['pruned'] = list(receiver.get('deleted_paths') or [])[:200]
            summary['pruned_count'] = receiver.get('deleted', 0)
            summary['iroh']['sync'] = {
                'files': reply.get('files'), 'blobs_sent': reply.get('blobs_sent'),
                'bytes_sent': reply.get('bytes_sent'),
                **{k: receiver.get(k) for k in ('created', 'unchanged', 'written',
                                                'mode_fixed', 'deleted', 'kept_non_source')}}
            q = shlex.quote(remote_cwd_abs)
            run_cmd = (f'mkdir -p {q} && cd {q} && exec {REMOTE_TASKS} run --who '
                       f'{shlex.quote(remote_owner)} {resource}-- {shlex.join(args.command)}')
            remote_cwd_shell = q
            durations['sync_s'] = round(time.monotonic() - t, 3)
        else:
            remote_cwd_shell = remote_path(remote_cwd_rel)
            sync_over_ssh(transport, args, hostname, top, paths, manifest, remote_rel, summary)
            durations['sync_s'] = round(time.monotonic() - t, 3)

        if args.cargo_fetch:
            t = time.monotonic()
            rc = subprocess.run(transport.ssh(f'cd {remote_cwd_shell} && '
                                              'exec cargo fetch --locked'),
                                stdin=subprocess.DEVNULL).returncode
            durations['cargo_fetch_s'] = round(time.monotonic() - t, 3)
            if rc != 0:
                raise OffloadError(f'remote cargo fetch --locked failed (exit {rc})',
                                   EXIT_TRANSPORT)
    except OffloadError as error:
        log(f'error: {error}')
        summary['remote_state'] = 'not_submitted'
        return finish(error.code)
    except KeyboardInterrupt:
        log('interrupted before remote submission; nothing was submitted')
        summary['remote_state'] = 'not_submitted'
        return finish(130)
    return run_and_fetch(args, transport, run_cmd, summary, durations, finish, fetches,
                         default_dest_root, remote_rel, peer, remote_owner)


def sync_over_ssh(transport, args, hostname, top, paths, manifest, remote_rel, summary):
    """v0: install helper, bind dir, rsync, prune + verify."""
    agent = install_agent(transport, args.remote_python)
    identity = json.dumps({'host': hostname, 'toplevel': top}, sort_keys=True)
    agent_call(transport, agent, ['prepare', '--dir', remote_rel, '--identity', identity])
    rsync_files(transport, top, paths, remote_rel, args.checksum)
    wire = json.dumps({'files': manifest['files']}).encode()
    applied = agent_call(transport, agent, ['apply', '--dir', remote_rel], input=wire)
    if applied['mismatched']:
        log(f'{len(applied["mismatched"])} files differ after sync; retrying them once '
            'with --checksum')
        rsync_files(transport, top, applied['mismatched'], remote_rel, True)
        applied = agent_call(transport, agent, ['apply', '--dir', remote_rel], input=wire)
        if applied['mismatched']:
            raise OffloadError('worker copy does not match the manifest (checkout changed '
                               'during sync?): ' + ', '.join(applied['mismatched'][:10]))
    summary['pruned'] = applied['pruned'][:200]
    summary['pruned_count'] = len(applied['pruned'])


def run_and_fetch(args, transport, run_cmd, summary, durations, finish, fetches,
                  default_dest_root, remote_rel, peer, remote_owner):
    # Run.
    signals = []
    run = None

    def on_signal(signum, _frame):
        signals.append(signum)
        if run is not None:
            run.start_cancel()

    previous = {s: signal.signal(s, on_signal) for s in (signal.SIGINT, signal.SIGTERM)}
    t = time.monotonic()
    run = RemoteRun(transport, run_cmd)
    if signals:
        run.start_cancel()
    rc = None
    cancel_signal = None
    while rc is None:
        rc = run.wait(0.05)
        if signals and cancel_signal is None:
            cancel_signal = signals[0]
            rc = cancel_remote(run, args, summary)
    for s, handler in previous.items():
        signal.signal(s, handler)
    durations['run_s'] = round(time.monotonic() - t, 3)
    summary['remote_job_id'] = run.job_id
    if run.final:
        summary['remote_phase'] = run.final[1]
        summary['remote_state'] = 'terminal'
    if cancel_signal is not None:
        if not run.final:
            report_uncertain(run, args, summary, rc, cancelled=True)
        return finish(128 + cancel_signal)
    if run.final:
        code = run.final[2]
    elif run.job_id is not None:
        report_uncertain(run, args, summary, rc)
        return finish(EXIT_TRANSPORT)
    elif rc == 255:
        log(f'UNCERTAIN: ssh to {args.worker} failed before a job id was seen; a job may '
            f'still have been submitted. Check `tasks list` on the worker for owner '
            f'{remote_owner} before retrying. Not resubmitting.')
        summary['remote_state'] = 'uncertain'
        return finish(EXIT_TRANSPORT)
    else:
        summary['remote_state'] = 'not_submitted'
        return finish(rc)

    # Fetch (only after a passed job).
    if fetches and code == 0 and peer:
        t = time.monotonic()
        dest = Path(args.fetch_dest) if args.fetch_dest else default_dest_root / str(run.job_id)
        summary['fetch_dest'] = str(dest)
        try:
            dest.parent.mkdir(parents=True, exist_ok=True)
            Path(peer.store).mkdir(parents=True, exist_ok=True)
            argv = ['fetch', *peer.dial, '--checkout', os.path.basename(summary['remote_dir']),
                    '--store', peer.store, '--dest', str(dest)]
            for rel in fetches:
                argv += ['--path', rel]
            reply = peer.call(argv, 'iroh fetch')
            summary['fetched'] = list(fetches)
            summary['iroh']['fetch'] = {k: reply.get(k) for k in (
                'files', 'blobs_received', 'bytes_received', 'skipped')}
        except (OffloadError, OSError) as error:
            log(f'error: {error}')
            durations['fetch_s'] = round(time.monotonic() - t, 3)
            return finish(EXIT_FETCH)
        durations['fetch_s'] = round(time.monotonic() - t, 3)
    elif fetches and code == 0:
        t = time.monotonic()
        dest = Path(args.fetch_dest) if args.fetch_dest else default_dest_root / str(run.job_id)
        summary['fetch_dest'] = str(dest)
        try:
            for rel in fetches:
                parent = dest / os.path.dirname(rel)
                parent.mkdir(parents=True, exist_ok=True)
                result = subprocess.run(transport.rsync_argv(
                    '--', f'{args.worker}:{remote_rel}/{rel}', f'{parent}/'),
                    stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
                if result.returncode != 0:
                    raise OffloadError('fetch %s failed (rsync exit %d): %s' % (
                        rel, result.returncode,
                        result.stderr.decode(errors='replace').strip()[-300:]), EXIT_FETCH)
                summary['fetched'].append(rel)
        except (OffloadError, OSError) as error:
            log(f'error: {error}')
            durations['fetch_s'] = round(time.monotonic() - t, 3)
            return finish(EXIT_FETCH)
        durations['fetch_s'] = round(time.monotonic() - t, 3)
    elif fetches:
        log(f'remote job exited {code}; skipping --fetch')
    return finish(code)


def cancel_remote(run, args, summary):
    """Cancel only our own remote job, wait for it, never resubmit."""
    if run.job_id is None:
        log('signal received; waiting up to 10s for the remote job id before cancelling')
        run.submitted.wait(10)
    if run.job_id is None:
        log('no remote job id seen; disconnecting without cancel')
        return run.kill()
    summary['cancel_requested'] = True
    log(f'cancelling remote job {run.job_id} on {args.worker}')
    run.start_cancel()
    canceller, sink = run.canceller
    try:
        canceller.wait(args.cancel_wait)
    except subprocess.TimeoutExpired:
        log('remote cancel request did not return in time')
    if canceller.returncode not in (None, 0):
        sink.seek(0)
        log(f'remote cancel exited {canceller.returncode}: '
            f'{sink.read().decode(errors="replace").strip()[-300:]}')
    rc = run.wait(args.cancel_wait)
    if rc is None:
        log('remote job did not reach a terminal phase in time; disconnecting')
        rc = run.kill()
    return rc


def report_uncertain(run, args, summary, rc, cancelled=False):
    summary['remote_state'] = 'uncertain'
    if run.job_id is None:
        log(f'UNCERTAIN: no remote job id was seen (ssh exit {rc}); a job may still have been '
            f'submitted. Check `tasks list` on {args.worker} before retrying. Not resubmitting.')
        return
    what = ('cancel was requested but its terminal phase was not observed' if cancelled
            else f'lost the connection to {args.worker} (ssh exit {rc}) after it was submitted')
    log(f'UNCERTAIN: remote job {run.job_id}: {what}; it may still be queued or running. '
        f'Query it on the worker: ssh {args.worker} \'~/.local/bin/tasks logs {run.job_id}\' '
        '(or tasks list). Not resubmitting.')


if __name__ == '__main__':
    raise SystemExit(main())
