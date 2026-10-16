"""Read-only Linux worker setup evidence and non-executable dry-run plan.

No subprocesses (including --version), downloads, installs, device opens or
service changes. Executable discovery does not prove a toolchain is installed:
rustup shims in particular are never invoked. Apply is deliberately unsupported.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import re
import shlex
import stat

SCHEMA = 'task-manager-linux-worker-inspection-v1'
SUPPORTED = {('ubuntu', '24.04'), ('debian', '12')}
# NixOS: any well-formed reported VERSION_ID is accepted and recorded verbatim.
NIXOS_VERSION = re.compile(r'[0-9A-Za-z][0-9A-Za-z._+-]{0,63}')
VOLATILE_FS = frozenset({'tmpfs', 'ramfs', 'overlay', 'squashfs', 'rootfs'})
NIXOS_MODULE = 'nix/worker-module.nix'
ARCHES = {'x86_64', 'aarch64'}
TOOLS = ('cargo', 'rustc', 'cc', 'clang', 'cmake', 'ninja', 'pkg-config',
         'java', 'javac', 'adb', 'emulator', 'sdkmanager')
BASE_TOOLS = ('cargo', 'rustc', 'cc', 'cmake', 'ninja', 'pkg-config')


def _text(path):
    """Bounded known metadata only. Reject specials before opening, no writes."""
    try:
        if not path.is_file():
            return None
        fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK | os.O_CLOEXEC)
        with os.fdopen(fd, 'rb') as file:
            if not stat.S_ISREG(os.fstat(file.fileno()).st_mode):
                return None
            data = file.read(65537)
        return data.decode('utf-8') if len(data) <= 65536 else None
    except (OSError, UnicodeError):
        return None


def _release(text):
    result = {}
    for line in (text or '').splitlines():
        if '=' not in line or line.startswith('#'):
            continue
        key, value = line.split('=', 1)
        if key not in ('ID', 'VERSION_ID'):
            continue
        try:
            values = shlex.split(value, comments=True)
            if len(values) == 1:
                result[key] = values[0]
        except ValueError:
            continue
    return result


def _memory(text, key):
    for line in (text or '').splitlines():
        parts = line.split()
        if len(parts) == 3 and parts[0] == key + ':' and parts[2] == 'kB':
            try:
                number = int(parts[1])
                if 0 <= number < 2**50:
                    return number * 1024
            except ValueError:
                pass
    return None


def _host_cpus():
    """Usable CPUs for this process: affinity mask first, then os.cpu_count()."""
    try:
        count = len(os.sched_getaffinity(0))
        if count > 0:
            return count, 'sched_getaffinity'
    except (AttributeError, OSError):
        pass
    count = os.cpu_count()
    return (count, 'cpu_count') if count else (None, None)


def _cgroup_number(text, *, cpu):
    """Parse one cgroup v2 cpu.max/memory.max value; 'max'/malformed -> None."""
    fields = (text or '').split()
    try:
        if cpu:
            if len(fields) != 2 or fields[0] == 'max':
                return None
            quota, period = int(fields[0]), int(fields[1])
            if not (0 < quota < 2**63 and 0 < period < 2**63):
                return None
            return quota / period
        if len(fields) != 1 or fields[0] == 'max':
            return None
        value = int(fields[0])
        return value if 0 <= value < 2**63 else None
    except ValueError:
        return None


def _cgroup(fs_root):
    """Read-only cgroup v2 limits for this process's cgroup and its ancestors.

    Uses proc/self/cgroup's unified ('0::') path under fs_root, bounded _text
    reads only; the tightest limit found is reported. Missing/escaping paths
    fall back to the namespace root. Never writes or creates cgroups.
    """
    base = fs_root / 'sys/fs/cgroup'
    parts = []
    for line in (_text(fs_root / 'proc/self/cgroup') or '').splitlines():
        if line.startswith('0::/'):
            candidate = [p for p in line[4:].split('/') if p]
            if (len(candidate) <= 32 and all(p not in ('.', '..') and len(p) <= 255
                                             for p in candidate)):
                parts = candidate
            break
    cpu = memory = None
    for depth in range(len(parts), -1, -1):
        directory = base.joinpath(*parts[:depth])
        value = _cgroup_number(_text(directory / 'cpu.max'), cpu=True)
        if value is not None:
            cpu = value if cpu is None else min(cpu, value)
        value = _cgroup_number(_text(directory / 'memory.max'), cpu=False)
        if value is not None:
            memory = value if memory is None else min(memory, value)
    return {'cgroup_v2': (base / 'cgroup.controllers').is_file(),
            'cgroup_cpu_limit': cpu, 'cgroup_memory_max_bytes': memory}


def _exists(path):
    """lstat-only presence; never opens, reads or follows the final component."""
    try:
        os.lstat(path)
        return True
    except OSError:
        return False


def _mounts(text):
    """Parse bounded proc/mounts text into {mountpoint: fstype}; last mount wins."""
    if text is None:
        return None
    result = {}
    for line in text.splitlines()[:4096]:
        fields = line.split()
        if len(fields) >= 3:
            point = re.sub(r'\\([0-7]{3})', lambda m: chr(int(m.group(1), 8)), fields[1])
            result[point] = fields[2]
    return result


def _covering(mounts, path):
    best = None
    for point in mounts:
        if (path == point or path.startswith(point.rstrip('/') + '/')) and (
                best is None or len(point) > len(best)):
            best = point
    return mounts[best] if best is not None else None


def _nixos(fs_root, machine):
    """Read-only NixOS facts: marker, mounts, current-system and nix-ld metadata.

    current-system/nix-ld use lstat/is_dir only. A real system's absolute
    current-system symlink is resolved against the live host, so synthetic
    fixtures should use plain directories.
    """
    mounts = _mounts(_text(fs_root / 'proc/mounts'))
    root_fs = store_fs = None
    ro_squash = root_volatile = store_volatile = live = None
    marker = _exists(fs_root / 'etc/NIXOS')
    if mounts is not None:
        root_fs = mounts.get('/')
        store_fs = _covering(mounts, '/nix/store')
        ro_squash = mounts.get('/nix/.ro-store') == 'squashfs'
        root_volatile = root_fs in VOLATILE_FS if root_fs else None
        store_volatile = (store_fs in VOLATILE_FS or ro_squash) if store_fs else None
        if root_volatile is not None and store_volatile is not None:
            live = marker and root_volatile and store_volatile
    loader = ('lib64/ld-linux-x86-64.so.2' if machine == 'x86_64'
              else 'lib/ld-linux-aarch64.so.1')
    return {
        'etc_nixos_present': marker, 'root_fs_type': root_fs, 'nix_store_fs_type': store_fs,
        'nix_ro_store_squashfs': ro_squash, 'root_volatile': root_volatile,
        'nix_store_volatile': store_volatile, 'live_iso': live,
        'persistent': None if live is None else not live,
        'current_system_present': _exists(fs_root / 'run/current-system'),
        'nix_ld_present': (fs_root / 'run/current-system/sw/share/nix-ld').is_dir(),
        'ld_linux_present': _exists(fs_root / loader),
        'evidence': 'etc/NIXOS + proc/mounts fstypes + lstat metadata only; nothing executed',
        'limitation': ('NixOS >= 24.05 ships a stub loader by default, so ld_linux_present '
                       'alone is not nix-ld evidence; live_iso is a heuristic'),
    }


def _tools(environ):
    # Metadata checks only; neither resolved executables nor shims are launched.
    dirs = environ.get('PATH', '').split(os.pathsep)[:64]
    result = {}
    for tool in TOOLS:
        found = None
        for directory in dirs:
            # Ignore relative/empty PATH entries: never inspect an implicit cwd.
            if not directory or len(directory) > 4096 or not Path(directory).is_absolute():
                continue
            candidate = Path(directory) / tool
            try:
                if candidate.is_file() and os.access(candidate, os.X_OK):
                    found = str(candidate)
                    break
            except OSError:
                continue
        result[tool] = {'executable_path': found, 'version': None,
                        'validated': False, 'evidence': 'PATH metadata only; may be a shim'}
    return result


def _disk(cache):
    probe = cache
    for _ in range(64):
        try:
            vfs = os.statvfs(probe)
            return {'intended_cache': str(cache), 'measured_at': str(probe),
                    'available_bytes': vfs.f_bavail * vfs.f_frsize,
                    'total_bytes': vfs.f_blocks * vfs.f_frsize,
                    'cache_created': False, 'recursive_usage_measured': False}
        except OSError:
            if probe == probe.parent:
                break
            probe = probe.parent
    return {'intended_cache': str(cache), 'measured_at': None,
            'available_bytes': None, 'total_bytes': None, 'cache_created': False,
            'recursive_usage_measured': False}


def _device(path):
    try:
        meta = path.stat()
        present = stat.S_ISCHR(meta.st_mode)
        return {'present': present, 'accessible': present and os.access(path, os.R_OK | os.W_OK),
                'evidence': 'character-device metadata and access check only; not opened'}
    except OSError:
        return {'present': False, 'accessible': False,
                'evidence': 'missing or unreadable metadata; not opened'}


def inspect_host(*, fs_root=Path('/'), system=None, machine=None, environ=None, cache=None,
                 logical_cpus=None):
    """fs_root/system/machine/environ/logical_cpus injection is for fixture tests.

    Host CPU counts are only reported for the real root; a synthetic fs_root
    reports None unless logical_cpus is injected.
    """
    fs_root = Path(fs_root)
    if logical_cpus is not None:
        if type(logical_cpus) is not int or logical_cpus < 1:
            raise ValueError('logical_cpus must be a positive integer')
        cpus, cpu_source = logical_cpus, 'injected'
    elif os.path.realpath(fs_root) == '/':
        cpus, cpu_source = _host_cpus()
    else:
        cpus, cpu_source = None, None
    system = platform.system() if system is None else system
    machine = platform.machine() if machine is None else machine
    environ = os.environ if environ is None else environ
    cache = Path(cache) if cache is not None else Path(environ.get('HOME', '/nonexistent')) / 'build-cache'
    if not cache.is_absolute() or '..' in cache.parts:
        raise ValueError('cache location must be an absolute nonescaping path')
    release = _release(_text(fs_root / 'etc/os-release'))
    distro, version = release.get('ID'), release.get('VERSION_ID')
    is_nixos = distro == 'nixos'
    known = ((distro, version) in SUPPORTED
             or (is_nixos and isinstance(version, str) and NIXOS_VERSION.fullmatch(version)))
    supported = system == 'Linux' and bool(known) and machine in ARCHES
    mem = _text(fs_root / 'proc/meminfo')
    cpu = _text(fs_root / 'proc/cpuinfo')
    flags = set()
    for line in (cpu or '').splitlines():
        key, _, value = line.partition(':')
        if key.strip().lower() in ('flags', 'features'):
            flags.update(value.split())
    nodes = []
    try:
        with os.scandir(fs_root / 'dev/dri') as scan:
            for count, item in enumerate(scan):
                if count >= 32:
                    break
                if item.name.startswith(('renderD', 'card')):
                    nodes.append({'name': item.name, **_device(Path(item.path))})
    except OSError:
        pass
    return {
        'schema': SCHEMA, 'supported': supported,
        'support_scope': ('inspection/planning only: Ubuntu 24.04, Debian 12 or NixOS '
                          '(reported VERSION_ID recorded); x86_64/aarch64'),
        'unsupported_reason': None if supported else 'unsupported or unknown OS/distro version/architecture',
        'platform': {'system': system, 'distro': distro, 'version': version, 'architecture': machine},
        'resources': {'logical_cpus': cpus if system == 'Linux' else None,
                      'logical_cpus_source': cpu_source if system == 'Linux' else None,
                      **(_cgroup(fs_root) if system == 'Linux'
                         else {'cgroup_v2': None, 'cgroup_cpu_limit': None,
                               'cgroup_memory_max_bytes': None}),
                      'mem_total_bytes': _memory(mem, 'MemTotal'),
                      'mem_available_bytes': _memory(mem, 'MemAvailable'),
                      'swap_total_bytes': _memory(mem, 'SwapTotal')},
        'nixos': _nixos(fs_root, machine) if is_nixos and system == 'Linux' else None,
        'cache': _disk(cache),
        'cache_policy': {name: {'set': bool(environ.get(name)), 'modified': False}
                         for name in ('RUSTC_WRAPPER', 'CARGO_TARGET_DIR', 'GRADLE_USER_HOME')},
        'tools': _tools(environ),
        'kvm': _device(fs_root / 'dev/kvm'),
        'virtualization': {'cpu_flag_observed': bool(flags & {'vmx', 'svm'}) if cpu is not None else None,
                           'note': 'x86 flags are hints only; ARM/nested virtualization may differ'},
        'gpu': {'dri_nodes': sorted(nodes, key=lambda item: item['name']), 'driver_version': None,
                'workload_validated': False,
                'limitation': 'nodes/access do not establish driver, Vulkan/EGL or emulator compatibility'},
        'emulator': {'runtime_validated': False, 'images_inspected': False,
                     'licenses_inspected_or_accepted': False,
                     'limitation': 'no SDK inventory, AVD scan, emulator launch, GPU or KVM ioctl test'},
        'limitations': ['tool versions and shim backing toolchains unverified',
                        'bounded metadata only; no recursive cache sizing or credential reads',
                        'no build, install, networking or emulator readiness acceptance'],
    }


def make_plan(report):
    missing = [tool for tool in BASE_TOOLS if not report['tools'][tool]['executable_path']]
    blockers = []
    if not report['supported']:
        blockers.append(report['unsupported_reason'])
    if missing:
        blockers.append('missing build-tool PATH evidence: ' + ', '.join(missing))
    if not report['kvm']['accessible']:
        blockers.append('KVM access not established')
    nixos = report.get('nixos')
    if nixos is not None:
        if nixos['live_iso'] is None:
            blockers.append('NixOS persistence unknown (mount metadata unavailable)')
        elif nixos['live_iso']:
            blockers.append('NixOS live ISO state is not persistent: a durable worker needs '
                            'an installed system with the reviewed declarative module')
    blockers.extend(['worker package provenance, versions, checksums and download budgets not selected',
                     'toolchain versions/backing installations not validated',
                     'Android images/licenses and emulator graphics/runtime unvalidated',
                     'installations and privileged changes require separate explicit approval'])
    return {'schema': 'task-manager-linux-worker-plan-v1', 'inspection': report,
            'supported': report['supported'], 'dry_run': True, 'apply_supported': False,
            'ready_to_apply': False, 'blockers': blockers,
            'proposals': [
                {'kind': 'review-declarative-nixos-module', 'module': NIXOS_MODULE,
                 'flake': 'nix/flake.nix', 'missing_tools': missing,
                 'options': {'kvm.enable': False, 'service.enable': False,
                             'nixLd.enable': False, 'androidEmulator.enable': False},
                 'draft': True, 'validated': False, 'applied': False, 'approved': False}
                if nixos is not None else
                {'kind': 'review-build-prerequisites', 'missing_tools': missing,
                 'version_pins': None, 'checksums': None, 'download_bytes': None,
                 'destination': None, 'approved': False},
                {'kind': 'review-cache-location', 'destination': report['cache']['intended_cache'],
                 'creates_or_moves_outputs': False, 'approved': False},
                {'kind': 'review-optional-android-kvm-gpu', 'destination': None,
                 'permission_changes': None, 'download_bytes': None, 'approved': False}],
            'executable_commands': [], 'permission_changes': [], 'service_changes': [],
            'downloads': [], 'unknown_values_are_blockers': True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group(required=True)
    modes.add_argument('--inspect', action='store_true')
    modes.add_argument('--plan', action='store_true')
    modes.add_argument('--apply', action='store_true')
    parser.add_argument('--cache', type=Path, help='intended location only; never created')
    args = parser.parse_args()
    if args.apply:
        print(json.dumps({'apply_supported': False, 'error': 'apply is unsupported; separate review/approval required'}))
        return 2
    try:
        report = inspect_host(cache=args.cache)
        print(json.dumps(make_plan(report) if args.plan else report, sort_keys=True))
        return 0 if report['supported'] else 2
    except (ValueError, OSError) as error:
        print(json.dumps({'error': str(error), 'apply_supported': False}))
        return 2


if __name__ == '__main__':
    raise SystemExit(main())
