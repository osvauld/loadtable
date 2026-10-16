"""tasks-offload worker-side helper (installed by the client; stdlib only).

Installed to ~/.local/lib/tasks-offload/agent.py next to a copy of
worker_source.py, whose path rules decide what counts as prunable source.
Subcommands (JSON on stdout, exit 0 ok / 2 refused):

  prepare --dir REL --identity TEXT   create ~/REL; bind it to one local checkout
  apply   --dir REL < manifest.json   prune non-manifest source, verify manifest

Prune deletes only paths that worker_source would accept as source AND that
are absent from the manifest. Excluded names (target/, caches, credentials,
logs, ...) are never deleted: they are worker-generated outputs or state.
Nothing outside ~/REL is touched; symlinks are never followed.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import stat
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import worker_source  # noqa: E402  (installed beside this file)

ROOT_PREFIX = 'offload/checkouts/'
# tasks' cache-output link names (local_command::place_outputs), kept on prune.
PRESERVED = ('target', 'android/build', 'android/app/build')


def _checkout(rel):
    if (not rel.startswith(ROOT_PREFIX) or rel.count('/') != 2
            or not rel[len(ROOT_PREFIX):].isalnum()):
        raise ValueError('refusing unexpected offload directory')
    return Path.home() / rel


def prepare(rel, identity):
    path = _checkout(rel)
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    marker = path.parent / (path.name + '.identity')
    if marker.exists():
        if marker.read_text() != identity:
            raise ValueError('remote checkout id collision: directory bound to another checkout')
    else:
        marker.write_text(identity)
    return {'ok': True, 'dir': str(path)}


def _sha256_at(dir_fd, name):
    fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=dir_fd)
    digest = hashlib.sha256()
    with os.fdopen(fd, 'rb') as f:
        for chunk in iter(lambda: f.read(1 << 20), b''):
            digest.update(chunk)
    return digest.hexdigest()


def _prunable(rel, parent_fd, name):
    if rel in PRESERVED:
        return False
    try:
        worker_source.check_path(rel)
    except ValueError:
        return False
    return not worker_source.is_marked_output(parent_fd, name)


def apply(rel, manifest):
    path = _checkout(rel)
    entries = {e['path']: e for e in manifest['files']}
    needed_dirs = set()
    for p in entries:
        parts = p.split('/')
        for i in range(1, len(parts)):
            needed_dirs.add('/'.join(parts[:i]))
    pruned, kept, candidates = [], 0, []
    root_fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        # Top-down so kept (excluded/preserved) directories are never descended.
        for dirpath, dirnames, filenames, dir_fd in os.fwalk('.', dir_fd=root_fd, topdown=True,
                                                              follow_symlinks=False):
            base = '' if dirpath == '.' else dirpath[2:]
            descend = []
            for name in list(dirnames):
                if stat.S_ISLNK(os.stat(name, dir_fd=dir_fd, follow_symlinks=False).st_mode):
                    filenames.append(name)  # never follow; a link is a leaf entry
                    continue
                rel_path = f'{base}/{name}' if base else name
                if not _prunable(rel_path, dir_fd, name):
                    kept += 1
                    continue
                descend.append(name)
                if rel_path not in needed_dirs:
                    candidates.append(rel_path)
            dirnames[:] = descend
            for name in filenames:
                rel_path = f'{base}/{name}' if base else name
                if rel_path in entries:
                    continue
                if not _prunable(rel_path, dir_fd, name):
                    kept += 1
                    continue
                os.unlink(name, dir_fd=dir_fd)
                pruned.append(rel_path)
        for rel_path in sorted(candidates, key=lambda r: -r.count('/')):
            try:
                os.rmdir(rel_path, dir_fd=root_fd)
                pruned.append(rel_path + '/')
            except OSError:
                pass  # still holds kept non-source content
    finally:
        os.close(root_fd)
    mismatched = []
    root_fd = worker_source.open_root(str(path))[0]
    try:
        for p, e in sorted(entries.items()):
            parts = worker_source.check_path(p)
            try:
                parent = worker_source.open_parent(root_fd, parts)
            except (OSError, ValueError):
                mismatched.append(p)
                continue
            try:
                meta = os.stat(parts[-1], dir_fd=parent, follow_symlinks=False)
                if e['type'] == 'link':
                    ok = (stat.S_ISLNK(meta.st_mode) and hashlib.sha256(os.fsencode(
                        os.readlink(parts[-1], dir_fd=parent))).hexdigest() == e['sha256'])
                else:
                    ok = (stat.S_ISREG(meta.st_mode) and meta.st_size == e['size']
                          and bool(meta.st_mode & 0o111) == e['executable']
                          and _sha256_at(parent, parts[-1]) == e['sha256'])
            except OSError:
                ok = False
            finally:
                os.close(parent)
            if not ok:
                mismatched.append(p)
    finally:
        os.close(root_fd)
    return {'ok': not mismatched, 'pruned': sorted(pruned), 'kept_non_source': kept,
            'verified': len(entries) - len(mismatched), 'mismatched': mismatched}


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest='cmd', required=True)
    p = sub.add_parser('prepare')
    p.add_argument('--dir', required=True)
    p.add_argument('--identity', required=True)
    a = sub.add_parser('apply')
    a.add_argument('--dir', required=True)
    args = parser.parse_args()
    try:
        if args.cmd == 'prepare':
            result = prepare(args.dir, args.identity)
        else:
            result = apply(args.dir, json.load(sys.stdin))
    except (ValueError, OSError, KeyError) as error:
        print(json.dumps({'ok': False, 'error': str(error)}))
        return 2
    print(json.dumps(result))
    return 0 if result['ok'] else 3


if __name__ == '__main__':
    raise SystemExit(main())
