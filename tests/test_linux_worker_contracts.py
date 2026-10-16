"""Synthetic stdlib-only acceptance; no installed tool/device is executed."""
import dataclasses
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import linux_worker_setup as setup
import worker_source as source


class SetupTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='sol-linux-setup-')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / 'etc').mkdir()
        (self.root / 'etc/os-release').write_text('ID=ubuntu\nVERSION_ID="24.04"\n')
        (self.root / 'proc').mkdir()
        (self.root / 'proc/meminfo').write_text('MemTotal: 1024 kB\nMemAvailable: 512 kB\nSwapTotal: 0 kB\n')
        (self.root / 'proc/cpuinfo').write_text('processor : 0\nflags : vmx sse\n')
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        for tool in setup.TOOLS:
            p = self.bin / tool
            p.write_text('#!/bin/sh\ntouch "' + str(self.root / 'BAD') + '"\nexit 99\n')
            p.chmod(0o755)

    def inspect(self, **kw):
        return setup.inspect_host(fs_root=self.root, system='Linux', machine=kw.get('machine', 'x86_64'),
                                  environ={'PATH': str(self.bin), 'HOME': str(self.root)},
                                  cache=self.root / 'absent-cache')

    def test_inspect_and_plan_read_only_no_subprocess(self):
        before = sorted(str(p.relative_to(self.root)) for p in self.root.rglob('*'))
        with mock.patch('subprocess.run', side_effect=AssertionError('tool invoked')):
            report = self.inspect()
            plan = setup.make_plan(report)
        self.assertTrue(report['supported'])
        self.assertEqual(report['resources']['mem_total_bytes'], 1048576)
        self.assertTrue(report['virtualization']['cpu_flag_observed'])
        self.assertFalse(report['kvm']['accessible'])
        self.assertIn('KVM access not established', plan['blockers'])
        self.assertFalse(plan['apply_supported'])
        self.assertEqual(report['tools']['cargo']['version'], None)
        self.assertEqual(before, sorted(str(p.relative_to(self.root)) for p in self.root.rglob('*')))
        json.dumps(plan)

    def test_unsupported_and_unknown_platforms(self):
        self.assertFalse(self.inspect(machine='riscv64')['supported'])
        (self.root / 'etc/os-release').write_text('ID=arch\nVERSION_ID=rolling\n')
        self.assertFalse(self.inspect()['supported'])
        (self.root / 'etc/os-release').unlink()
        self.assertFalse(self.inspect()['supported'])

    def test_malformed_or_oversized_facts_are_unknown(self):
        (self.root / 'proc/meminfo').write_text('MemTotal: -1 kB\n' + 'x' * 70000)
        self.assertIsNone(self.inspect()['resources']['mem_total_bytes'])

    def test_kvm_permission_evidence_without_opening_device(self):
        real_stat = Path.stat
        kvm = self.root / 'dev/kvm'
        def fake_stat(path, *args, **kw):
            if path == kvm:
                return os.stat_result((0o20660, 0, 0, 1, 0, 0, 0, 0, 0, 0))
            return real_stat(path, *args, **kw)
        with mock.patch.object(Path, 'stat', fake_stat), mock.patch.object(setup.os, 'access', return_value=False):
            report = self.inspect()
        self.assertTrue(report['kvm']['present'])
        self.assertFalse(report['kvm']['accessible'])
        self.assertIn('KVM access not established', setup.make_plan(report)['blockers'])

    def test_cli_unsupported_returns_json_nonzero(self):
        report = self.inspect(machine='riscv64')
        with mock.patch.object(setup, 'inspect_host', return_value=report), \
                mock.patch.object(sys, 'argv', ['setup', '--inspect']), \
                mock.patch('builtins.print') as output:
            self.assertEqual(setup.main(), 2)
        self.assertFalse(json.loads(output.call_args.args[0])['supported'])

    def test_logical_cpus_injectable_and_not_host_for_synthetic_root(self):
        self.assertIsNone(self.inspect()['resources']['logical_cpus'])
        report = setup.inspect_host(fs_root=self.root, system='Linux', machine='x86_64',
                                    environ={'PATH': str(self.bin)}, cache=self.root / 'c',
                                    logical_cpus=3)
        self.assertEqual(report['resources']['logical_cpus'], 3)
        self.assertEqual(report['resources']['logical_cpus_source'], 'injected')
        for bad in (0, -1, True, 1.5, 'x'):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                setup.inspect_host(fs_root=self.root, system='Linux', machine='x86_64',
                                   environ={}, cache=self.root / 'c', logical_cpus=bad)

    def test_logical_cpus_prefers_affinity_on_real_root(self):
        kw = dict(fs_root=Path('/'), system='Linux', machine='x86_64',
                  environ={'PATH': ''}, cache=self.root / 'c')
        with mock.patch.object(setup.os, 'sched_getaffinity', create=True, return_value={0, 2}), \
                mock.patch.object(setup.os, 'cpu_count', return_value=64):
            resources = setup.inspect_host(**kw)['resources']
        self.assertEqual((resources['logical_cpus'], resources['logical_cpus_source']),
                         (2, 'sched_getaffinity'))
        with mock.patch.object(setup.os, 'sched_getaffinity', create=True, side_effect=OSError), \
                mock.patch.object(setup.os, 'cpu_count', return_value=64):
            resources = setup.inspect_host(**kw)['resources']
        self.assertEqual((resources['logical_cpus'], resources['logical_cpus_source']),
                         (64, 'cpu_count'))

    def cgroup(self, rel, cpu=None, memory=None):
        directory = self.root / 'sys/fs/cgroup' / rel
        directory.mkdir(parents=True, exist_ok=True)
        if cpu is not None:
            (directory / 'cpu.max').write_text(cpu)
        if memory is not None:
            (directory / 'memory.max').write_text(memory)

    def test_cgroup_v2_limits_read_only_fixtures(self):
        resources = self.inspect()['resources']
        self.assertIsNone(resources['cgroup_cpu_limit'])
        self.assertIsNone(resources['cgroup_memory_max_bytes'])
        (self.root / 'proc/self').mkdir(parents=True)
        (self.root / 'proc/self/cgroup').write_text('0::/user.slice/job.scope\n')
        self.cgroup('', cpu='max 100000\n', memory='max\n')
        (self.root / 'sys/fs/cgroup/cgroup.controllers').write_text('cpu memory\n')
        self.cgroup('user.slice', cpu='200000 100000\n', memory='max\n')
        self.cgroup('user.slice/job.scope', cpu='150000 100000\n', memory='1073741824\n')
        before = sorted(str(p.relative_to(self.root)) for p in self.root.rglob('*'))
        resources = self.inspect()['resources']
        self.assertEqual(resources['cgroup_cpu_limit'], 1.5)  # tightest ancestor wins
        self.assertEqual(resources['cgroup_memory_max_bytes'], 1073741824)
        self.assertTrue(resources['cgroup_v2'])
        self.cgroup('user.slice', cpu='50000 100000\n', memory='536870912\n')
        resources = self.inspect()['resources']
        self.assertEqual(resources['cgroup_cpu_limit'], 0.5)
        self.assertEqual(resources['cgroup_memory_max_bytes'], 536870912)
        self.assertEqual(before, sorted(str(p.relative_to(self.root)) for p in self.root.rglob('*')))

    def test_cgroup_max_malformed_and_escaping_are_unknown(self):
        (self.root / 'proc/self').mkdir(parents=True)
        cgroup_file = self.root / 'proc/self/cgroup'
        for cpu, memory in [('max 100000', 'max'), ('', ''), ('abc 100000', '-5'),
                            ('100000 0', '1 2'), ('100000', 'x' * 70000),
                            ('-1 100000', '99999999999999999999999')]:
            cgroup_file.write_text('0::/\n')
            self.cgroup('', cpu=cpu, memory=memory)
            with self.subTest(cpu=cpu[:20], memory=memory[:20]):
                resources = self.inspect()['resources']
                self.assertIsNone(resources['cgroup_cpu_limit'])
                self.assertIsNone(resources['cgroup_memory_max_bytes'])
        outside = self.root / 'sys/fs/escape'
        outside.mkdir(parents=True)
        (outside / 'cpu.max').write_text('100000 100000')
        (outside / 'memory.max').write_text('4096')
        cgroup_file.write_text('0::/../escape\n')
        resources = self.inspect()['resources']
        self.assertIsNone(resources['cgroup_cpu_limit'])
        self.assertIsNone(resources['cgroup_memory_max_bytes'])

    LIVE_MOUNTS = ('tmpfs / tmpfs rw,relatime,mode=755 0 0\n'
                   '/dev/root /iso iso9660 ro,relatime 0 0\n'
                   '/dev/loop0 /nix/.ro-store squashfs ro,relatime 0 0\n'
                   'tmpfs /nix/.rw-store tmpfs rw,relatime 0 0\n'
                   'overlay /nix/store overlay rw,lowerdir=/nix/.ro-store,upperdir=/nix/.rw-store/store 0 0\n')
    DISK_MOUNTS = ('/dev/nvme0n1p2 / ext4 rw,relatime 0 0\n'
                   '/dev/nvme0n1p1 /boot vfat rw 0 0\n'
                   '/dev/nvme0n1p2 /nix/store ext4 ro,nosuid,nodev,relatime 0 0\n')

    def nixos(self, version='26.05', mounts=None, installed_extras=False):
        release = 'ID=nixos\nNAME=NixOS\n'
        if version is not None:
            release += 'VERSION_ID="' + version + '"\n'
        (self.root / 'etc/os-release').write_text(release)
        (self.root / 'etc/NIXOS').write_text('')
        if mounts is not None:
            (self.root / 'proc/mounts').write_text(mounts)
        if installed_extras:
            (self.root / 'run/current-system/sw/share/nix-ld/lib').mkdir(parents=True)
            (self.root / 'lib64').mkdir()
            (self.root / 'lib64/ld-linux-x86-64.so.2').write_bytes(b'stub')

    def test_nixos_live_iso_detection_and_blocking_plan(self):
        self.nixos(mounts=self.LIVE_MOUNTS)
        before = sorted(str(p.relative_to(self.root)) for p in self.root.rglob('*'))
        with mock.patch('subprocess.run', side_effect=AssertionError('tool invoked')):
            report = self.inspect()
            plan = setup.make_plan(report)
        self.assertTrue(report['supported'])
        self.assertEqual(report['platform']['distro'], 'nixos')
        self.assertEqual(report['platform']['version'], '26.05')
        nixos = report['nixos']
        self.assertTrue(nixos['etc_nixos_present'])
        self.assertEqual(nixos['root_fs_type'], 'tmpfs')
        self.assertEqual(nixos['nix_store_fs_type'], 'overlay')
        self.assertTrue(nixos['nix_ro_store_squashfs'])
        self.assertTrue(nixos['root_volatile'])
        self.assertTrue(nixos['nix_store_volatile'])
        self.assertIs(nixos['live_iso'], True)
        self.assertIs(nixos['persistent'], False)
        self.assertFalse(nixos['current_system_present'])
        self.assertFalse(nixos['nix_ld_present'])
        self.assertTrue(any('live ISO' in b and 'not persistent' in b for b in plan['blockers']))
        kinds = [item['kind'] for item in plan['proposals']]
        self.assertIn('review-declarative-nixos-module', kinds)
        self.assertNotIn('review-build-prerequisites', kinds)
        module = plan['proposals'][kinds.index('review-declarative-nixos-module')]
        self.assertEqual(module['module'], 'nix/worker-module.nix')
        self.assertIs(module['applied'], False)
        self.assertIs(module['approved'], False)
        self.assertEqual(plan['executable_commands'], [])
        self.assertFalse(plan['ready_to_apply'])
        text = json.dumps(plan['proposals']) + json.dumps(plan['blockers'])
        for forbidden in ('apt', 'dpkg', 'nix-env', 'nixos-rebuild', 'sudo'):
            self.assertNotIn(forbidden, text)
        self.assertEqual(before, sorted(str(p.relative_to(self.root)) for p in self.root.rglob('*')))

    def test_nixos_installed_disk_system_and_nix_ld_metadata(self):
        self.nixos(version='26.11pre-git', mounts=self.DISK_MOUNTS, installed_extras=True)
        for machine in ('x86_64', 'aarch64'):
            report = self.inspect(machine=machine)
            self.assertTrue(report['supported'])
            self.assertEqual(report['platform']['version'], '26.11pre-git')
        self.assertFalse(report['nixos']['ld_linux_present'])  # aarch64 loader path absent
        nixos = self.inspect()['nixos']
        self.assertEqual((nixos['root_fs_type'], nixos['nix_store_fs_type']), ('ext4', 'ext4'))
        self.assertFalse(nixos['nix_ro_store_squashfs'])
        self.assertIs(nixos['live_iso'], False)
        self.assertIs(nixos['persistent'], True)
        self.assertTrue(nixos['current_system_present'])
        self.assertTrue(nixos['nix_ld_present'])
        self.assertTrue(nixos['ld_linux_present'])
        plan = setup.make_plan(self.inspect())
        self.assertFalse(any('live ISO' in b or 'persistence' in b for b in plan['blockers']))
        self.assertIn('review-declarative-nixos-module', [p['kind'] for p in plan['proposals']])

    def test_nixos_unknown_mounts_versions_and_architecture(self):
        self.nixos(mounts=None)
        report = self.inspect()
        self.assertIsNone(report['nixos']['live_iso'])
        self.assertIsNone(report['nixos']['persistent'])
        self.assertIn('NixOS persistence unknown (mount metadata unavailable)',
                      setup.make_plan(report)['blockers'])
        self.assertFalse(self.inspect(machine='riscv64')['supported'])
        for version in (None, '', 'bad\x01', 'x' * 65, 'two words'):
            with self.subTest(version=version):
                self.nixos(version=version, mounts=self.LIVE_MOUNTS)
                self.assertFalse(self.inspect()['supported'])

    def test_non_nixos_has_no_nixos_section_or_module_proposal(self):
        report = self.inspect()
        self.assertIsNone(report['nixos'])
        kinds = [item['kind'] for item in setup.make_plan(report)['proposals']]
        self.assertNotIn('review-declarative-nixos-module', kinds)
        self.assertIn('review-build-prerequisites', kinds)

    def test_nix_draft_static_safety(self):
        nix = Path(__file__).resolve().parents[1] / 'nix'
        module = (nix / 'worker-module.nix').read_text()
        flake = (nix / 'flake.nix').read_text()
        self.assertTrue((nix / 'README.md').is_file())
        self.assertIn('nixosModules', flake)
        self.assertIn('taskworker', module)
        self.assertIn('/var/lib/taskworker', module)
        self.assertIn('networking.firewall.interfaces', module)
        self.assertNotRegex(module, r'networking\.firewall\.allowed(UDP|TCP)Port')
        self.assertNotRegex(module + flake, r'accept_license\s*=\s*true')
        for option in ('kvm.enable', 'service.enable', 'nixLd.enable', 'androidEmulator.enable'):
            self.assertIn(option.split('.')[0], module)
        self.assertNotRegex(module, r'mkEnableOption[^;]*;\s*#?\s*default\s*=\s*true')
        self.assertIn('programs.nix-ld.enable', module)

    def test_apply_gated_cli(self):
        script = Path(__file__).resolve().parents[1] / 'scripts/setup-linux-worker.sh'
        p = subprocess.run(['/bin/sh', str(script), '--apply'], capture_output=True, text=True, timeout=5)
        self.assertEqual(p.returncode, 2)
        self.assertFalse(json.loads(p.stdout)['apply_supported'])


class SourceTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='sol-source-')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / 'src').mkdir()
        (self.root / 'src/main.rs').write_bytes(b'dirty or untracked\n')
        (self.root / 'run.sh').write_bytes(b'echo synthetic\n')
        self.paths = ['src/main.rs', 'run.sh']

    def capture(self, paths=None, **kw):
        return source.capture(self.root, self.paths if paths is None else paths, **kw)

    def test_no_git_head_determinism_and_frozen_bytes(self):
        a = self.capture()
        b = self.capture(reversed(self.paths))
        self.assertEqual(a, b)
        self.assertEqual(a.receipt, b.receipt)
        self.assertEqual(a.manifest()['hash_algorithm'], 'sha256')
        self.assertIsNone(a.manifest()['iroh_blobs_hash'])
        for entry in a.manifest()['files']:
            self.assertIn('iroh_blobs_hash', entry)
            self.assertIsNone(entry['iroh_blobs_hash'])
        self.assertNotIn(str(self.root), json.dumps(a.manifest()))
        with self.assertRaises(dataclasses.FrozenInstanceError):
            a.files = ()
        altered = a.manifest()
        altered['files'].clear()
        self.assertEqual(len(a.files), 2)
        blob = a.files[0].sha256
        original = a.blob(blob, expected_receipt=a.receipt)
        (self.root / a.files[0].path).write_bytes(b'changed after approval')
        with mock.patch.object(source, '_read_file', side_effect=AssertionError('mutable reread')):
            self.assertEqual(a.blob(blob, expected_receipt=a.receipt), original)
            self.assertEqual(len(a.missing([], expected_receipt=a.receipt)), 2)
        self.assertNotEqual(self.capture().receipt, a.receipt)

    def test_edits_rename_exec_and_deletion(self):
        a = self.capture()
        (self.root / 'src/main.rs').write_bytes(b'edited')
        b = self.capture()
        self.assertEqual(source.diff(a, b)['changed'], ['src/main.rs'])
        (self.root / 'run.sh').chmod(0o755)
        c = self.capture()
        self.assertNotEqual(b.receipt, c.receipt)
        self.assertEqual(source.diff(b, c)['changed'], ['run.sh'])
        self.assertEqual(c.missing([f.sha256 for f in b.files], expected_receipt=c.receipt), ())
        (self.root / 'run.sh').rename(self.root / 'renamed.sh')
        d = self.capture(['renamed.sh', 'src/main.rs'])
        self.assertEqual(source.diff(c, d), {'added': ['renamed.sh'], 'removed': ['run.sh'], 'changed': []})
        self.assertEqual(d.missing([f.sha256 for f in c.files], expected_receipt=d.receipt), ())
        (self.root / 'src/main.rs').unlink()
        e = self.capture(['renamed.sh'])
        self.assertEqual(source.diff(d, e)['removed'], ['src/main.rs'])

    def test_missing_dedup_eviction_and_receipt_binding(self):
        (self.root / 'duplicate').write_bytes((self.root / 'run.sh').read_bytes())
        a = self.capture(self.paths + ['duplicate'])
        missing = a.missing([], expected_receipt=a.receipt)
        self.assertEqual(len(missing), 2)
        self.assertEqual(a.missing(missing, expected_receipt=a.receipt), ())
        self.assertEqual(a.missing(missing[:1], expected_receipt=a.receipt), missing[1:])
        for digest in missing:
            self.assertEqual(hashlib.sha256(a.blob(digest, expected_receipt=a.receipt)).hexdigest(), digest)
        with self.assertRaises(ValueError):
            a.missing([], expected_receipt='0' * 64)
        with self.assertRaises(ValueError):
            a.blob('0' * 64, expected_receipt=a.receipt)
        with self.assertRaises(ValueError):
            a.missing(['iroh:not-sha256'], expected_receipt=a.receipt)

    def test_reject_paths_excluded_credentials_and_duplicates(self):
        for path in ['../escape', '/tmp/escape', './run.sh', 'src//main.rs', 'src/../run.sh',
                     'src\\main.rs', 'bad\nname', 'target/a', 'build/a', '.git/config',
                     '.env', 'nested/.env.local', 'id_ed25519', 'key.pem', 'key.jks',
                     '.aws/config', '.cargo/credentials.toml', 'local.properties', 'a.pyc',
                     '.netrc', '.npmrc', '.pypirc', '.git-credentials', 'state.sqlite-wal',
                     'node_modules/a', 'sdk/a', '.gradle/a', 'state.sqlite', 'x' * 1025]:
            with self.subTest(path=path), self.assertRaises(ValueError):
                self.capture([path])
        with self.assertRaises(ValueError):
            self.capture(['run.sh', 'run.sh'])

    def write(self, rel, data=b'x'):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)

    def test_output_names_allowed_as_ordinary_source(self):
        for marker in ['Cargo.toml', 'build.gradle.kts', 'settings.gradle', 'app/build.gradle.kts']:
            self.write(marker)
        allowed = ['app/src/main/java/com/x/sdk/Foo.kt', 'src/build/mod.rs', 'src/target/mod.rs',
                   'lib/out/x.rs', 'lib/dist/x.rs', 'py/cache/x.py', 'py/caches/x.py',
                   'native/ndk/x.c', 'app/src/outputs/x.kt', 'app/src/build/x.kt',
                   'crate/target.rs', 'build.gradle.kts', 'app/build.gradle.kts']
        for path in allowed:
            if not (self.root / path).exists():
                self.write(path)
        captured = self.capture(allowed)
        self.assertEqual(sorted(f.path for f in captured.files), sorted(allowed))

    def test_output_locations_still_denied(self):
        self.write('crate/Cargo.toml')
        self.write('app/build.gradle.kts')
        self.write('lib/settings.gradle.kts')
        self.write('mod/build.gradle')
        self.write('kmp/settings.gradle')
        denied = ['build/x', 'out/x', 'dist/x', 'outputs/x', 'sdk/x', 'ndk/x', 'cache/x',
                  'caches/x', 'target/x', 'Target/x', 'crate/target/debug/x', 'app/build/outputs/x',
                  'lib/build/x', 'mod/build/x', 'kmp/build/x', 'a/b/node_modules/x',
                  'src/__pycache__/x', 'app/.gradle/x', 'deep/app/.cxx/x', 'app/.kotlin/x',
                  'src/.git/config', 'src/.ssh/id', 'src/.aws/x', 'src/.env.local', 'src/key.pem',
                  'src/.netrc', 'src/.cache/x']
        for path in denied:
            self.write(path)
            with self.subTest(path=path), self.assertRaisesRegex(ValueError, 'excluded'):
                self.capture([path])
        # A symlink/any entry named like a marker counts (stat only, no follow/read).
        (self.root / 'crate2').mkdir()
        os.symlink('/nonexistent', self.root / 'crate2/Cargo.toml')
        self.write('crate2/target/x')
        with self.assertRaisesRegex(ValueError, 'excluded'):
            self.capture(['crate2/target/x'])

    def test_marker_check_does_not_open_or_read_marker(self):
        self.write('crate/Cargo.toml')
        self.write('crate/target/x')
        real_open = os.open
        def guarded(path, *args, **kw):
            if os.fspath(path) in ('Cargo.toml', 'target') or str(path).endswith('Cargo.toml'):
                raise AssertionError('marker/output opened: ' + str(path))
            return real_open(path, *args, **kw)
        with mock.patch.object(source.os, 'open', side_effect=guarded):
            with self.assertRaisesRegex(ValueError, 'excluded'):
                self.capture(['crate/target/x'])

    def test_raised_ceiling_opt_in_and_conservative_defaults(self):
        self.assertEqual(source.Limits(), source.Limits(max_files=256, max_file_bytes=4 * 1024 * 1024,
                                                        max_bytes=16 * 1024 * 1024, max_depth=32))
        big = source.Limits(max_files=20000, max_file_bytes=64 * 1024 * 1024,
                            max_bytes=512 * 1024 * 1024)
        big.validate()
        for field, value in [('max_files', 20001), ('max_file_bytes', 64 * 1024 * 1024 + 1),
                             ('max_bytes', 512 * 1024 * 1024 + 1), ('max_depth', 0),
                             ('max_files', True)]:
            with self.subTest(field=field), self.assertRaises(ValueError):
                dataclasses.replace(source.Limits(), **{field: value}).validate()
        for index in range(300):
            self.write(f'many/{index}.txt', str(index).encode())
        paths = [f'many/{index}.txt' for index in range(300)]
        with self.assertRaises(ValueError):
            self.capture(paths)
        captured = self.capture(paths, limits=big)
        self.assertEqual(len(captured.files), 300)
        digests = [f.sha256 for f in captured.files]
        self.assertEqual(captured.missing(digests, expected_receipt=captured.receipt), ())

    def test_root_through_symlinked_parent_is_resolved_once(self):
        outer = Path(tempfile.mkdtemp(prefix='osv-source-outer-'))
        self.addCleanup(shutil.rmtree, outer)
        (outer / 'real/project/src').mkdir(parents=True)
        (outer / 'real/project/src/lib.rs').write_bytes(b'lib')
        (outer / 'linked').symlink_to(outer / 'real', target_is_directory=True)
        direct = source.capture(outer / 'real/project', ['src/lib.rs'])
        via_link = source.capture(outer / 'linked/project', ['src/lib.rs'])
        self.assertEqual(direct.receipt, via_link.receipt)
        self.assertEqual(direct.manifest(), via_link.manifest())
        self.assertEqual(via_link.resolved_root, os.path.realpath(outer / 'real/project'))
        self.assertNotIn('resolved_root', via_link.manifest())
        (outer / 'real/project/src/inner').symlink_to(outer / 'real/project/src',
                                                       target_is_directory=True)
        with self.assertRaises((ValueError, OSError)):
            source.capture(outer / 'linked/project', ['src/inner/lib.rs'])
        for bad in ['relative/root', outer / 'real/project/src/lib.rs', outer / 'missing']:
            with self.subTest(root=str(bad)), self.assertRaises(ValueError):
                source.capture(bad, ['src/lib.rs'])
        script = Path(__file__).resolve().parents[1] / 'scripts/worker_source.py'
        result = subprocess.run([sys.executable, '-B', str(script), '--root',
                                 str(outer / 'linked/project'), '--file', 'src/lib.rs'],
                                capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report['diagnostics']['resolved_root'],
                         os.path.realpath(outer / 'real/project'))
        self.assertEqual(report['manifest'], direct.manifest())

    def test_reject_links_special_files_and_directory(self):
        (self.root / 'link').symlink_to('/etc/passwd')
        (self.root / 'alias').symlink_to(self.root / 'src', target_is_directory=True)
        os.link(self.root / 'run.sh', self.root / 'hard')
        os.mkfifo(self.root / 'pipe')
        for path in ['link', 'alias/main.rs', 'hard', 'pipe', 'src']:
            with self.subTest(path=path), self.assertRaises((ValueError, OSError)):
                self.capture([path])
        # Intentional change: the root itself is resolved once (realpath), so a
        # symlinked root is accepted; links BELOW the root remain rejected above.
        self.assertEqual(source.capture(self.root / 'alias', ['main.rs']).receipt,
                         source.capture(self.root / 'src', ['main.rs']).receipt)

    def test_cli_explicit_files_without_git(self):
        script = Path(__file__).resolve().parents[1] / 'scripts/worker_source.py'
        command = [sys.executable, '-B', str(script), '--root', str(self.root), '--file', 'run.sh']
        result = subprocess.run(command, capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertFalse(report['transport_implemented'])
        self.assertEqual(report['manifest'], self.capture(['run.sh']).manifest())
        rejected = subprocess.run(command + ['--file', '../escape'], capture_output=True, text=True, timeout=5)
        self.assertEqual(rejected.returncode, 2)
        self.assertIn('error', json.loads(rejected.stdout))

    def test_receipt_framing_golden_and_empty_capture(self):
        # Fixed independent vector for the domain/path/mode/length/content framing.
        (self.root / 'a').write_bytes(b'abc')
        captured = self.capture(['a'])
        framed = (b'task-manager-local-source-manifest-v1\0' + b'\x01\0\0\0\0\0\0\0a\0'
                  + b'\x03\0\0\0\0\0\0\0' + hashlib.sha256(b'abc').digest())
        self.assertEqual(captured.receipt, hashlib.sha256(framed).hexdigest())
        self.assertEqual(self.capture([]).files, ())
        self.assertEqual(self.capture([]).missing([], expected_receipt=self.capture([]).receipt), ())

    def test_bounds_and_ordinary_concurrent_mutation(self):
        for limits in [source.Limits(max_files=1), source.Limits(max_file_bytes=2),
                       source.Limits(max_bytes=20), source.Limits(max_depth=1)]:
            with self.assertRaises(ValueError):
                self.capture(limits=limits)
        real_read = source._read_file
        calls = 0
        def mutate(*args, **kw):
            nonlocal calls
            result = real_read(*args, **kw)
            calls += 1
            if calls == len(self.paths):
                (self.root / 'src/main.rs').write_bytes(b'concurrent edit')
            return result
        with mock.patch.object(source, '_read_file', side_effect=mutate):
            with self.assertRaisesRegex(ValueError, 'changed'):
                self.capture()



class SourceIrohBlobsHashTests(unittest.TestCase):
    """Phase 2: per-file iroh_blobs_hash = hex BLAKE3(content), never a substitute digest."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='osv-blob-source-')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / 'src').mkdir()
        (self.root / 'src/main.rs').write_bytes(b'synthetic main\n')
        (self.root / 'docs.txt').write_bytes(b'abc')
        self.paths = ['src/main.rs', 'docs.txt']

    def capture(self):
        return source.capture(self.root, self.paths)

    def test_hash_is_null_without_blake3_module(self):
        with mock.patch.object(source, '_blake3', None):
            manifest = self.capture().manifest()
            self.assertIsNone(source.iroh_blobs_hash(b'abc'))
        self.assertTrue(all(f['iroh_blobs_hash'] is None for f in manifest['files']))

    def test_hash_comes_from_blake3_module_and_never_changes_receipt(self):
        seen = []

        class FakeDigest:
            def __init__(self, content):
                seen.append(content)

            def hexdigest(self):
                return 'ab' * 32

        fake = mock.Mock(blake3=FakeDigest)
        with mock.patch.object(source, '_blake3', None):
            plain = self.capture()
        with mock.patch.object(source, '_blake3', fake):
            hashed = self.capture()
            manifest = hashed.manifest()
        self.assertEqual(plain.receipt, hashed.receipt)
        self.assertEqual([f['iroh_blobs_hash'] for f in manifest['files']], ['ab' * 32] * 2)
        self.assertIn(b'abc', seen)
        self.assertIsNone(manifest['iroh_blobs_hash'])
        bad = mock.Mock(blake3=lambda content: mock.Mock(hexdigest=lambda: 'XYZ'))
        with mock.patch.object(source, '_blake3', bad):
            with self.assertRaisesRegex(ValueError, 'BLAKE3'):
                self.capture().manifest()

    def hashed_copy(self, manifest):
        other = json.loads(json.dumps(manifest))
        for index, entry in enumerate(other['files']):
            entry['iroh_blobs_hash'] = f'{index:064x}'
        return other

    def test_attach_hashes_from_rust_manifest_requires_identical_capture(self):
        with mock.patch.object(source, '_blake3', None):
            manifest = self.capture().manifest()
        rust = self.hashed_copy(manifest)
        merged = source.attach_iroh_blobs_hashes(manifest, rust)
        self.assertEqual([f['iroh_blobs_hash'] for f in merged['files']],
                         [f'{0:064x}', f'{1:064x}'])
        self.assertEqual(merged['receipt'], manifest['receipt'])
        self.assertTrue(all(f['iroh_blobs_hash'] is None for f in manifest['files']),
                        'input not mutated')
        for mutate in (
                lambda m: m.__setitem__('receipt', '0' * 64),
                lambda m: m['files'][0].__setitem__('sha256', '0' * 64),
                lambda m: m['files'][0].__setitem__('size', 999),
                lambda m: m['files'][0].__setitem__('executable', True),
                lambda m: m['files'][0].__setitem__('path', 'other.rs'),
                lambda m: m['files'].pop(),
                lambda m: m['files'][0].__setitem__('iroh_blobs_hash', 'A' * 64),
                lambda m: m['files'][0].__setitem__('iroh_blobs_hash', None),
                lambda m: m.__setitem__('schema', 'other')):
            other = self.hashed_copy(manifest)
            mutate(other)
            with self.assertRaises(ValueError):
                source.attach_iroh_blobs_hashes(manifest, other)


if __name__ == '__main__':
    unittest.main()
