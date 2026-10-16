"""Local, in-memory source contracts; NOT a transport, store or execution API.

Explicit file selection, no Git/HEAD requirement. Reuses the Rust workspace
capsule's two-inventory pattern and digest framing, with a distinct domain.
See docs/design/linux-worker-local-contracts.md for limits and threat boundary.
"""
import argparse
from dataclasses import dataclass, field
import hashlib
import json
import os
from pathlib import Path
import stat

SCHEMA = 'task-manager-local-source-manifest-v1'
DOMAIN = SCHEMA.encode('ascii') + b'\0'


@dataclass(frozen=True)
class Limits:
    """Conservative defaults; callers may opt in up to CEILING, never beyond.

    Capture is in-memory: max_bytes is resident memory (twice transiently while
    the second inventory is compared). Large trees need the future blob store.
    """
    max_files: int = 256
    max_file_bytes: int = 4 * 1024 * 1024
    max_bytes: int = 16 * 1024 * 1024
    max_depth: int = 32

    def validate(self):
        for name in self.__dataclass_fields__:
            value = getattr(self, name)
            if type(value) is not int or not 1 <= value <= getattr(CEILING, name):
                raise ValueError('limits must be integers within the built-in ceiling')


CEILING = Limits(max_files=20000, max_file_bytes=64 * 1024 * 1024,
                 max_bytes=512 * 1024 * 1024, max_depth=32)

# Defense in depth, not a secret scanner. Callers must review selected content.
# Denied as ANY path component (credentials, VCS, local state, tool caches).
FORBIDDEN_PARTS = frozenset({
    '.git', '.hg', '.jj', '.svn',
    '.gradle', '.kotlin', '.cxx', '__pycache__', 'node_modules', '.cache',
    '.android', '.avd', '.ssh', '.aws',
    '.gnupg', '.config', '.local', 'credentials', 'secrets', 'captures',
    '.cargo', '.rustup', 'local.properties', 'gradle.properties',
    '.netrc', '.npmrc', '.pypirc', '.git-credentials', '.docker', '.kube',
})
# Build-output/cache/SDK names are denied only where they are output locations:
# (a) as the FIRST component under the selected root (any type), and
# (b) as an intermediate directory whose parent directory contains one of the
#     listed build markers (checked by no-follow fstatat only; never opened/read).
# Elsewhere (e.g. src/build/mod.rs, java/com/x/sdk/Foo.kt) they are source.
TOP_LEVEL_OUTPUT_PARTS = frozenset({
    'target', 'build', 'dist', 'out', 'outputs', 'sdk', 'ndk', 'cache', 'caches',
})
MARKED_OUTPUT_DIRS = {
    'target': ('Cargo.toml',),
    'build': ('build.gradle', 'build.gradle.kts', 'settings.gradle', 'settings.gradle.kts'),
}
FORBIDDEN_SUFFIXES = ('.pem', '.key', '.jks', '.keystore', '.p12', '.pfx',
                      '.pyc', '.sqlite', '.sqlite3', '.db', '.log',
                      '.sqlite-wal', '.sqlite-shm', '.sqlite3-wal', '.sqlite3-shm',
                      '.db-wal', '.db-shm')


def _path(value, limits):
    if not isinstance(value, str):
        raise ValueError('paths must be relative UTF-8 strings')
    try:
        encoded = value.encode('utf-8')
    except UnicodeError as error:
        raise ValueError('paths must be UTF-8') from error
    parts = value.split('/')
    if (not encoded or len(encoded) > 1024 or len(parts) > limits.max_depth
            or '\\' in value or any(ord(c) < 32 or ord(c) == 127 for c in value)
            or any(p in ('', '.', '..') for p in parts)):
        raise ValueError('invalid, escaping or oversized source path')
    lower = [p.lower() for p in parts]
    if (any(p in FORBIDDEN_PARTS or p.startswith(('.env', 'id_rsa', 'id_ed25519', 'id_ecdsa'))
            for p in lower)
            or lower[0] in TOP_LEVEL_OUTPUT_PARTS
            or lower[-1].endswith(FORBIDDEN_SUFFIXES)
            or lower[-1] in ('credentials.json', 'credentials.toml', 'token', 'tokens.json')):
        raise ValueError('excluded credential/cache/output path')
    return parts


def _open_root(root):
    """Resolve the caller's root ONCE (symlinked ancestors allowed), then open it.

    The resolved path is walked with no-follow opens, so a component swapped
    to a symlink after resolution fails closed. Everything below the root is
    traversed no-follow by _read_file. Returns (fd, resolved_root).
    """
    root = Path(root)
    if not root.is_absolute() or '..' in root.parts:
        raise ValueError('an absolute nonescaping source root is required')
    resolved = Path(os.path.realpath(root))
    if not resolved.is_absolute() or not resolved.is_dir():
        raise ValueError('source root must be an existing directory')
    fd = os.open('/', os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        for part in resolved.parts[1:]:
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                            dir_fd=fd)
            os.close(fd)
            fd = child
        return fd, str(resolved)
    except BaseException:
        os.close(fd)
        raise


def _marked_output(parent_fd, name):
    """True if `name` is an output dir beside a build marker (stat only, no follow)."""
    for marker in MARKED_OUTPUT_DIRS.get(name.lower(), ()):
        try:
            os.stat(marker, dir_fd=parent_fd, follow_symlinks=False)
            return True
        except FileNotFoundError:
            continue
        except OSError as error:
            raise ValueError('cannot establish build-marker absence; refusing') from error
    return False


def _identity(meta):
    return (meta.st_dev, meta.st_ino, meta.st_mode, meta.st_nlink,
            meta.st_size, meta.st_mtime_ns, meta.st_ctime_ns)


@dataclass(frozen=True)
class CapturedFile:
    path: str
    content: bytes
    executable: bool

    @property
    def sha256(self):
        return hashlib.sha256(self.content).hexdigest()

    def entry(self):
        # iroh-blobs addresses content by BLAKE3, not SHA-256; a future
        # transport fills iroh_blobs_hash. It is never part of the receipt.
        return {'path': self.path, 'size': len(self.content),
                'executable': self.executable, 'sha256': self.sha256,
                'iroh_blobs_hash': None}


def open_parent(root_fd, parts):
    """Public: no-follow descent to the parent of normalized `parts`, applying
    the marker-based output rule to every intermediate directory. Caller closes."""
    parent = os.dup(root_fd)
    try:
        for part in parts[:-1]:
            if _marked_output(parent, part):
                raise ValueError('excluded build-output path beside a build marker')
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                            dir_fd=parent)
            os.close(parent)
            parent = child
        return parent
    except BaseException:
        os.close(parent)
        raise


def check_path(value, limits=Limits()):
    """Public name-based path rule used by capture; raises ValueError, returns parts."""
    return _path(value, limits)


open_root = _open_root
is_marked_output = _marked_output


def _read_file(root_fd, path, limits):
    parts = _path(path, limits)
    parent = open_parent(root_fd, parts)
    try:
        before = os.stat(parts[-1], dir_fd=parent, follow_symlinks=False)
        if not stat.S_ISREG(before.st_mode) or before.st_nlink != 1:
            raise ValueError('links/special files/directories are unsupported')
        if before.st_size > limits.max_file_bytes:
            raise ValueError('oversized source file')
        fd = os.open(parts[-1], os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
                     dir_fd=parent)
        with os.fdopen(fd, 'rb') as file:
            opened = os.fstat(file.fileno())
            if _identity(opened) != _identity(before):
                raise ValueError('source changed while opening')
            content = file.read(limits.max_file_bytes + 1)
            after = os.fstat(file.fileno())
        current = os.stat(parts[-1], dir_fd=parent, follow_symlinks=False)
        if (len(content) > limits.max_file_bytes or len(content) != opened.st_size
                or _identity(opened) != _identity(after)
                or _identity(after) != _identity(current)):
            raise ValueError('source changed while reading')
        return CapturedFile(path, content, bool(opened.st_mode & 0o111)), _identity(after)
    finally:
        os.close(parent)


def _digest(value):
    if (not isinstance(value, str) or len(value) != 64
            or any(c not in '0123456789abcdef' for c in value)):
        raise ValueError('expected a lowercase SHA-256 digest, not an iroh-blobs hash')
    return value


@dataclass(frozen=True)
class Capture:
    files: tuple[CapturedFile, ...]
    # Local diagnostic only: excluded from equality, receipt and manifest.
    resolved_root: str | None = field(default=None, compare=False)

    @property
    def receipt(self):
        digest = hashlib.sha256(DOMAIN)
        for file in self.files:
            path = file.path.encode('utf-8')
            digest.update(len(path).to_bytes(8, 'little'))
            digest.update(path)
            digest.update(bytes([file.executable]))
            digest.update(len(file.content).to_bytes(8, 'little'))
            digest.update(bytes.fromhex(file.sha256))
        return digest.hexdigest()

    def manifest(self):
        # Fresh JSON-compatible values; mutation cannot affect the capture.
        return {'schema': SCHEMA, 'hash_algorithm': 'sha256', 'receipt': self.receipt,
                'iroh_blobs_hash': None, 'files': [f.entry() for f in self.files]}

    def _approved(self, expected_receipt):
        if _digest(expected_receipt) != self.receipt:
            raise ValueError('capture differs from the expected receipt')

    def missing(self, possessed_sha256, *, expected_receipt):
        """Local possession model only; peers/claims are NOT authenticated here."""
        self._approved(expected_receipt)
        have = set()
        for count, digest in enumerate(possessed_sha256, 1):
            if count > CEILING.max_files:
                raise ValueError('possession list exceeds bound')
            have.add(_digest(digest))
        return tuple(sorted({f.sha256 for f in self.files} - have))

    def blob(self, sha256, *, expected_receipt):
        """Return only retained immutable bytes, never workspace paths."""
        self._approved(expected_receipt)
        _digest(sha256)
        for file in self.files:
            if file.sha256 == sha256:
                return file.content
        raise ValueError('blob is not referenced by this capture')


def capture(root, paths, *, limits=Limits()):
    """Two bounded inventories, no writes or Git access; not an atomic snapshot.

    Removed/new paths must be reflected in the caller's next explicit selection.
    Ordinary concurrent edits fail without retry. A hostile same-UID writer is
    outside this contract; in particular reads are not a filesystem transaction.
    """
    limits.validate()
    selected = set()
    for count, path in enumerate(paths, 1):
        if count > limits.max_files:
            raise ValueError('source file count exceeds bound')
        _path(path, limits)
        if path in selected:
            raise ValueError('duplicate source path')
        selected.add(path)
    ordered = sorted(selected)
    fd, resolved_root = _open_root(root)
    try:
        def inventory():
            result = []
            total = 0
            for path in ordered:
                item = _read_file(fd, path, limits)
                total += len(item[0].content)
                if total > limits.max_bytes:
                    raise ValueError('aggregate source bytes exceed bound')
                result.append(item)
            return tuple(result)
        first = inventory()
        if inventory() != first:
            raise ValueError('source changed between inventories')
        return Capture(tuple(file for file, _ in first), resolved_root)
    finally:
        os.close(fd)


def diff(previous, current):
    """Renames are removed+added paths; unchanged content still reuses a blob."""
    old = {f.path: f.entry() for f in previous.files}
    new = {f.path: f.entry() for f in current.files}
    return {'added': sorted(new.keys() - old.keys()),
            'removed': sorted(old.keys() - new.keys()),
            'changed': sorted(p for p in old.keys() & new.keys() if old[p] != new[p])}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', required=True, type=Path)
    parser.add_argument('--file', action='append', required=True, dest='files',
                        help='explicit permitted relative file; repeat; no directories/globs')
    parser.add_argument('--have-sha256', action='append', default=[])
    args = parser.parse_args()
    try:
        result = capture(args.root, args.files)
        print(json.dumps({'manifest': result.manifest(),
                          'missing_sha256': result.missing(args.have_sha256,
                                                          expected_receipt=result.receipt),
                          'diagnostics': {'resolved_root': result.resolved_root},
                          'transport_implemented': False}, sort_keys=True))
        return 0
    except (ValueError, OSError) as error:
        print(json.dumps({'error': str(error), 'transport_implemented': False}))
        return 2


if __name__ == '__main__':
    raise SystemExit(main())
