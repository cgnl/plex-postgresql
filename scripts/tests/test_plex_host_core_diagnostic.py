"""Host crash diagnostics must stay owned, bounded and free of argument output."""

import importlib.util, io, json, pathlib, subprocess, sys, tempfile, unittest
from unittest.mock import patch
source = pathlib.Path(__file__).resolve().parents[1] / 'plex-host-core-diagnostic.py'
spec = importlib.util.spec_from_file_location('host_core', source)
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)

class HostCoreTests(unittest.TestCase):

    def test_stdout_cap(self):
        result, out = m.bounded([sys.executable, '-c', 'import sys;sys.stdout.buffer.write(b"x"*10000)'], limit=100)
        self.assertEqual(result['status'], 'size_limit')
        self.assertLessEqual(len(out), 100)

    def test_stream_cap(self):
        destination = io.BytesIO()
        result, out = m.bounded([sys.executable, '-c', 'import sys;sys.stdout.buffer.write(b"x"*10000)'], limit=100, destination=destination)
        self.assertEqual(result['status'], 'size_limit')
        self.assertLessEqual(len(destination.getvalue()), 100)

    def test_timeout(self):
        result, _ = m.bounded([sys.executable, '-c', 'import time;time.sleep(5)'], timeout=0.1)
        self.assertEqual(result['status'], 'timeout')

    def test_stream_success(self):
        destination = io.BytesIO()
        result, out = m.bounded([sys.executable, '-c', 'print("core")'], destination=destination)
        self.assertEqual(result['status'], 'captured')
        self.assertEqual(destination.getvalue(), b'core\n')
        self.assertEqual(out, b'')

    def test_nonzero(self):
        result, _ = m.bounded([sys.executable, '-c', 'raise SystemExit(1)'])
        self.assertEqual(result['status'], 'unavailable')

    def test_permission_status(self):
        result, _ = m.bounded([sys.executable, '-c', 'import sys;sys.stderr.write("Permission denied\\n");raise SystemExit(1)'])
        self.assertEqual(result['status'], 'permission_denied')

    def test_metadata_secrets_filtered(self):
        fields, matched = m.safe_info(b' PID: 123 (Plex Media Serv)\n Executable: ' + m.PMS_EXE.encode() + b'\n Command Line: SECRET\n Environment: SECRET\n Stack trace of thread 123: SECRET\n Signal: 11 (SEGV)\n', 123)
        self.assertTrue(matched)
        self.assertNotIn('SECRET', str(fields))
        self.assertEqual(fields['Signal'], '11 (SEGV)')

    def test_wrong_pid(self):
        self.assertFalse(m.safe_info(('PID: 124\nExecutable: ' + m.PMS_EXE).encode(), 123)[1])

    def test_wrong_exe(self):
        self.assertFalse(m.safe_info(b'PID: 123\nExecutable: /usr/bin/other', 123)[1])

    def test_observed_exact_pms_only(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            (root / 'before-pms-lifecycle.json').write_text(json.dumps({'host_processes': '123 1 Sl 200 Plex Media Serv\n124 1 Sl 200 Plex Script Hos\n125 1 S 200 bash'}))
            self.assertEqual(m.observed_pids(root), [123])

    def test_observed_symlink_skipped(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            (root / 'other').write_text(json.dumps({'host_processes': '123 1 Sl 200 Plex Media Serv'}))
            (root / 'a-pms-lifecycle.json').symlink_to(root / 'other')
            self.assertEqual(m.observed_pids(root), [])

    def test_restart_pid_selected_before_stale_pid_when_cleanup_pms_absent(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            snapshots = [
                ('first-start', 123),
                ('restart-100', 456),
                ('cleanup', None),
            ]
            for index, (stage, pid) in enumerate(snapshots):
                path = root / (stage + '-pms-lifecycle.json')
                processes = '' if pid is None else str(pid) + ' 1 Sl 200 Plex Media Serv'
                path.write_text(json.dumps({'host_processes': processes}))
                m.os.utime(path, ns=(index + 1, index + 1))
            self.assertEqual(m.observed_pids(root), [456, 123])

    def test_gdb_safe(self):
        commands = m.stack_commands(pathlib.Path('/proc/123/root'), pathlib.Path('/proc/123/root/usr/lib/plexmediaserver/Plex Media Server'), pathlib.Path('/tmp/core'))
        self.assertIn('set auto-load off', commands)
        self.assertIn('set print frame-arguments none', commands)
        self.assertIn('thread apply all bt 12', commands)
        self.assertFalse(any((value in '\n'.join(commands) for value in ['bt full', 'info args', 'info locals', 'show environment', 'attach '])))

    def mocked_collect(self, mode):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            (root / 'before-pms-lifecycle.json').write_text(json.dumps({'host_processes': '123 1 Sl 200 Plex Media Serv'}))
            seen = []

            def bounded(command, **kwargs):
                seen.append(command)
                if command[0] == 'docker':
                    label = 'other' if mode == 'unowned' else 'fixture'
                    return ({'status': 'captured'}, (json.dumps({'Pid': 777, 'Running': True, 'StartedAt': '2026-10-10T14:22:09.123456789Z'}) + ' ' + label).encode())
                if 'info' in command:
                    if mode == 'permission':
                        return ({'status': 'permission_denied'}, b'')
                    return ({'status': 'captured'}, ('PID: 123\nExecutable: ' + m.PMS_EXE + '\nCommand Line: SECRET').encode())
                if 'dump' in command:
                    kwargs['destination'].write(b'coredata')
                    return ({'status': 'size_limit' if mode == 'oversize' else 'captured', 'bytes': 8}, b'')
                self.assertIn('--nx', command)
                self.assertIn('--nh', command)
                return ({'status': 'captured'}, b'Core was generated by `SECRET\n#0 PlexCrash ()\n')
            with patch.object(m, 'bounded', side_effect=bounded), patch.object(m.shutil, 'which', return_value='/usr/bin/tool'), patch.object(m.os, 'geteuid', return_value=1000):
                result = m.collect(root, 'container', 'fixture')
            if mode == 'unowned':
                self.assertEqual(result['status'], 'ownership_rejected')
                self.assertEqual(len(seen), 1)
            elif mode == 'permission':
                self.assertEqual(result['cores'][0]['status'], 'matching_core_unavailable')
                self.assertEqual(len(seen), 2)
            elif mode == 'oversize':
                self.assertEqual(result['cores'][0]['status'], 'core_not_captured')
                self.assertFalse(list(root.glob('*.partial')))
                self.assertFalse(list(root.glob('*.core')))
            else:
                self.assertEqual(result['cores'][0]['status'], 'core_captured')
                self.assertEqual((root / 'host-pms-123.core').read_bytes(), b'coredata')
                self.assertNotIn('SECRET', (root / 'host-pms-123-stack.txt').read_text())
                self.assertFalse(list(root.glob('*.partial')))
                for command in seen[1:3]:
                    self.assertIn('COREDUMP_PID=123', command)
                    self.assertIn('COREDUMP_EXE=' + m.PMS_EXE, command)
                    self.assertIn('--since=2026-10-10 14:22:09 UTC', command)

    def test_owned_capture(self):
        self.mocked_collect('success')

    def test_unowned(self):
        self.mocked_collect('unowned')

    def test_permission(self):
        self.mocked_collect('permission')

    def test_oversize_cleanup(self):
        self.mocked_collect('oversize')
if __name__ == '__main__':
    unittest.main(verbosity=2)
