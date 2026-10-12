import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class NativeLogLimits(unittest.TestCase):
    def run_driver(self, **overrides):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            driver = root / 'plex-container-e2e.sh'
            shutil.copyfile(Path(__file__).resolve().parents[1] / driver.name, driver)
            (root / 'prepare-media-fixtures.py').write_text(
                'import pathlib,sys\n'
                'p=pathlib.Path(sys.argv[1]); p.mkdir(exist_ok=True)\n'
                '(p/"media-fixtures.json").write_text("{}")\n')
            (root / 'plex-host-core-diagnostic.py').write_text('pass\n')
            docker = root / 'docker'
            docker.write_text('''#!/usr/bin/env python3
import json,os,sys
a=sys.argv[1:]
with open(os.environ['COMMAND_LOG'],'a') as f: f.write(json.dumps(a)+'\\n')
if a[:2]==['image','inspect']:
    if a[-2]=='{{.Id}}': print('sha256:'+'a'*64)
    elif a[-2]=='{{.Architecture}}': print('amd64')
    else: print('{}')
elif a[:1]==['info']: print('amd64')
elif a[:1]==['run'] and '--rm' in a:
    print('1.2.3' if 'bash' in a else '000 T sqlite3_open')
elif a[:1]==['run'] and '--name' in a and a[a.index('--name')+1].endswith('-plex'):
    sys.exit(47)  # Stop before any actual workload or readiness loop.
''')
            docker.chmod(0o755)
            command_log = root / 'commands.jsonl'
            env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ['PATH'],
                       COMMAND_LOG=str(command_log), CANDIDATE_IMAGE='sha256:' + 'a' * 64,
                       POSTGRES_IMAGE='postgres@sha256:' + 'b' * 64,
                       EXPECTED_PLEX_VERSION='1.2.3', EXPECTED_ARCH='amd64',
                       VARIANT='linuxserver', EVIDENCE_DIR=str(root / 'evidence'),
                       PLEX_E2E_MEDIA_CACHE_DIR=str(root / 'media'))
            for key in ('PLEX_E2E_SHIM_LOG_LEVEL', 'PLEX_E2E_POSTGRES_LOG_STATEMENT'):
                env.pop(key, None)
            env.update(overrides)
            result = subprocess.run(['bash', str(driver)], env=env,
                                    capture_output=True, text=True, timeout=15)
            commands = [json.loads(line) for line in command_log.read_text().splitlines()] if command_log.exists() else []
            return result, commands

    def assert_container_limits(self, commands, shim_level, sql_level):
        runs = [a for a in commands if a[:2] == ['run', '-d']]
        self.assertEqual(len(runs), 2)
        for run in runs:
            self.assertEqual(run[run.index('--log-driver') + 1], 'json-file')
            options = [run[i + 1] for i, arg in enumerate(run) if arg == '--log-opt']
            self.assertEqual(options, ['max-size=20m', 'max-file=3'])
        self.assertIn('log_statement=' + sql_level, runs[0])
        self.assertIn('log_min_error_statement=error', runs[0])
        self.assertIn('PLEX_PG_LOG_LEVEL=' + shim_level, runs[1])

    def test_default_execution_bounds_logs_and_retains_error_statements(self):
        result, commands = self.run_driver()
        self.assertEqual(result.returncode, 47, result.stderr)
        self.assert_container_limits(commands, 'INFO', 'none')

    def test_explicit_diagnostics_preserve_rotation(self):
        result, commands = self.run_driver(PLEX_E2E_SHIM_LOG_LEVEL='DEBUG',
                                           PLEX_E2E_POSTGRES_LOG_STATEMENT='all')
        self.assertEqual(result.returncode, 47, result.stderr)
        self.assert_container_limits(commands, 'DEBUG', 'all')

    def test_invalid_options_fail_before_docker(self):
        for override in ({'PLEX_E2E_SHIM_LOG_LEVEL': 'TRACE'},
                         {'PLEX_E2E_POSTGRES_LOG_STATEMENT': 'error'}):
            with self.subTest(override=override):
                result, commands = self.run_driver(**override)
                self.assertEqual(result.returncode, 2)
                self.assertEqual(commands, [])


if __name__ == '__main__':
    unittest.main()
