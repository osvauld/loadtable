#!/usr/bin/env python3
"""status-watch-ssh: poll a worker's `tasks status --json` over ssh (fallback for the
iroh status stream).

  status-watch-ssh --worker USER@HOST [--ssh-key PATH] [--interval SECS]
                   [--remote-cmd CMD] [--machine-id ID] [--count N]
                   [--max-bytes N] [--timeout SECS] [--backoff-max SECS]
                   [--no-multiplex] [--ssh-program P]

Prints exactly what `lan-peer status-watch` prints: one compact JSON line per
snapshot with `machine.online = true` and a `stream` object (`transport: "ssh"`),
or a synthetic marker line (`stream.marker`): `offline` (`machine.online =
false`) when ssh or the remote command fails at the transport level, and
`status_error` (machine online) when the worker answered but its status command
failed or printed something that is not a snapshot. After a failure it retries
with backoff (the interval doubling up to --backoff-max, reset by a snapshot).
Runs until killed, or until --count lines were printed.

Each poll runs only the configured remote command (default
`"$HOME"/.local/bin/tasks status --json`) through key-only, BatchMode ssh; the
remote command string is this tool's configuration, never data from the worker.
By default one ssh master connection is reused (ControlMaster=auto,
ControlPersist=60) so a 2-5 s poll does not pay a handshake each time.
"""
import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import time

DEFAULT_REMOTE_CMD = '"$HOME"/.local/bin/tasks status --json'
MAX_SNAPSHOT_BYTES = 4 * 1024 * 1024
WORKER_RE = re.compile(r'^([A-Za-z0-9._-]+@)?[A-Za-z0-9._:-]+$')
EXIT_USAGE = 2


def marker(kind, machine_id, worker, error, retry_in_s, now=None):
    """Same shape as `lan-peer status-watch` markers (docs/design/cluster-snapshot.md)."""
    return {
        'version': 1,
        'taken_at': time.time() if now is None else now,
        'machine': {'id': machine_id, 'online': kind != 'offline'},
        'jobs': [],
        'stream': {'transport': 'ssh', 'peer': worker, 'marker': kind,
                   'error': error, 'retry_in_s': retry_in_s},
    }


def annotate_live(value, worker, seq, now=None):
    if not isinstance(value.get('machine'), dict):
        value['machine'] = {}
    value['machine']['online'] = True
    value['stream'] = {'transport': 'ssh', 'peer': worker, 'seq': seq,
                       'received_at': time.time() if now is None else now}
    return value


def parse_snapshot(raw, max_bytes):
    """Returns (snapshot dict, None) or (None, error)."""
    if len(raw) > max_bytes:
        return None, f'snapshot too large (> {max_bytes} bytes)'
    try:
        value = json.loads(raw)
    except ValueError as error:
        return None, f'status output is not JSON: {error}'
    if not isinstance(value, dict) or not isinstance(value.get('version'), int) \
            or isinstance(value.get('version'), bool):
        return None, 'status output is not a snapshot object with a numeric version'
    return value, None


def ssh_argv(args):
    argv = [args.ssh_program, '-T', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10',
            '-o', 'ServerAliveInterval=10', '-o', 'ServerAliveCountMax=2']
    if args.ssh_key:
        argv += ['-i', args.ssh_key, '-o', 'IdentitiesOnly=yes']
    if not args.no_multiplex:
        runtime = os.environ.get('XDG_RUNTIME_DIR') or tempfile.gettempdir()
        argv += ['-o', 'ControlMaster=auto', '-o', 'ControlPersist=60',
                 '-o', f'ControlPath={runtime}/tasks-status-ssh-%C']
    return [*argv, '--', args.worker, args.remote_cmd]


def poll(args, argv):
    """One poll. Returns ('snapshot', value) | ('status_error', msg) | ('offline', msg)."""
    try:
        result = subprocess.run(argv, stdin=subprocess.DEVNULL, capture_output=True,
                                timeout=args.timeout)
    except subprocess.TimeoutExpired:
        return 'offline', f'ssh poll timed out after {args.timeout:g}s'
    except OSError as error:
        return 'offline', f'cannot run ssh: {error}'
    stderr = result.stderr.decode(errors='replace').strip()[-500:]
    if result.returncode == 255:
        return 'offline', f'ssh failed (exit 255): {stderr}'
    if result.returncode != 0:
        return 'status_error', f'status command failed (exit {result.returncode}): {stderr}'
    value, error = parse_snapshot(result.stdout, args.max_bytes)
    if error:
        return 'status_error', error
    return 'snapshot', value


def parse_args(argv):
    parser = argparse.ArgumentParser(prog='status-watch-ssh',
                                     description=__doc__.split('\n\n')[0])
    parser.add_argument('--worker', required=True, help='ssh destination USER@HOST')
    parser.add_argument('--ssh-key', help='identity file (IdentitiesOnly=yes)')
    parser.add_argument('--interval', type=float, default=3.0,
                        help='seconds between polls (2-5 recommended; minimum 1)')
    parser.add_argument('--remote-cmd', default=DEFAULT_REMOTE_CMD,
                        help='remote POSIX-shell command printing one snapshot')
    parser.add_argument('--machine-id',
                        help='machine id for markers before the first snapshot '
                             '(default: the host part of --worker)')
    parser.add_argument('--count', type=int, help='exit after printing N lines')
    parser.add_argument('--max-bytes', type=int, default=MAX_SNAPSHOT_BYTES)
    parser.add_argument('--timeout', type=float, default=20.0, help='seconds per poll')
    parser.add_argument('--backoff-max', type=float, default=30.0)
    parser.add_argument('--no-multiplex', action='store_true',
                        help='one fresh ssh connection per poll (no ControlMaster)')
    parser.add_argument('--ssh-program', default=os.environ.get('TASKS_OFFLOAD_SSH', 'ssh'))
    args = parser.parse_args(argv)
    if not WORKER_RE.match(args.worker) or args.worker.startswith('-'):
        parser.error('--worker must be USER@HOST')
    if args.interval < 1:
        parser.error('--interval must be at least 1 second')
    if not 1 <= args.max_bytes <= MAX_SNAPSHOT_BYTES:
        parser.error(f'--max-bytes must be 1..{MAX_SNAPSHOT_BYTES}')
    if args.count is not None and args.count < 1:
        parser.error('--count must be positive')
    if args.timeout <= 0 or args.backoff_max < args.interval:
        parser.error('--timeout must be positive and --backoff-max at least --interval')
    return args


def main(argv=None):
    args = parse_args(sys.argv[1:] if argv is None else argv)
    argv = ssh_argv(args)
    machine_id = args.machine_id or args.worker.rsplit('@', 1)[-1]
    printed, seq, backoff = 0, 0, args.interval

    def emit(value):
        nonlocal printed
        sys.stdout.write(json.dumps(value, separators=(',', ':')) + '\n')
        sys.stdout.flush()
        printed += 1
        return args.count is not None and printed >= args.count

    try:
        while True:
            started = time.monotonic()
            kind, value = poll(args, argv)
            if kind == 'snapshot':
                seq += 1
                if isinstance(value.get('machine'), dict) and \
                        isinstance(value['machine'].get('id'), str):
                    machine_id = value['machine']['id']
                backoff = args.interval
                if emit(annotate_live(value, args.worker, seq)):
                    return 0
                time.sleep(max(0.0, args.interval - (time.monotonic() - started)))
                continue
            retry = backoff if kind == 'offline' else args.interval
            if emit(marker(kind, machine_id, args.worker, value, retry)):
                return 0
            time.sleep(retry)
            if kind == 'offline':
                backoff = min(backoff * 2, args.backoff_max)
    except KeyboardInterrupt:
        return 130
    except BrokenPipeError:
        return 0


if __name__ == '__main__':
    raise SystemExit(main())
