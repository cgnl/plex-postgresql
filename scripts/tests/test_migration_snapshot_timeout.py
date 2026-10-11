"""Snapshot time limits must reject invalid input before touching the source."""
import hashlib
from contextlib import closing
import os
import pathlib
import sqlite3
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]


class SnapshotTimeoutTests(unittest.TestCase):
    def test_configurable_limits_preserve_sources(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            source = root / 'library.db'
            with closing(sqlite3.connect(source)) as connection, connection:
                connection.execute('CREATE TABLE metadata_items(id INTEGER PRIMARY KEY, title TEXT)')
                connection.execute("INSERT INTO metadata_items VALUES (1, 'timeout proof')")
            before = hashlib.sha256(source.read_bytes()).hexdigest()
            for timeout in (None, '3600', '86400', '0.000000001', '0', '-1', '86401', 'nan', 'inf', 'invalid'):
                with self.subTest(timeout=timeout):
                    destination = root / ('snapshot-' + str(timeout))
                    environment = os.environ.copy()
                    environment.pop('MIGRATION_SNAPSHOT_TIMEOUT', None)
                    if timeout is not None:
                        environment['MIGRATION_SNAPSHOT_TIMEOUT'] = timeout
                    result = subprocess.run(
                        ['bash', '-c', 'source "$1"; snapshot_sqlite_sources "$2" "$3"',
                         'snapshot-test', str(ROOT / 'scripts/migrate_lib.sh'), str(source), str(destination)],
                        env=environment, capture_output=True, text=True, timeout=10)
                    if timeout in (None, '3600', '86400'):
                        self.assertEqual(result.returncode, 0, result.stderr)
                        with closing(sqlite3.connect(destination / 'library.db')) as connection:
                            self.assertEqual(connection.execute('SELECT title FROM metadata_items').fetchone(), ('timeout proof',))
                    elif timeout == '0.000000001':
                        self.assertNotEqual(result.returncode, 0)
                        self.assertIn('snapshot timed out', result.stderr)
                        self.assertEqual(list(destination.iterdir()), [])
                    else:
                        self.assertNotEqual(result.returncode, 0)
                        self.assertFalse(destination.exists())
                    self.assertEqual(hashlib.sha256(source.read_bytes()).hexdigest(), before)
                    self.assertFalse(pathlib.Path(str(source) + '-wal').exists())
                    self.assertFalse(pathlib.Path(str(source) + '-shm').exists())


if __name__ == '__main__':
    unittest.main()
