"""Shadow databases need their sidecar; a marker alone only earns a recovery hint."""
from contextlib import closing
import pathlib
import sqlite3
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]


def run(function, path):
    return subprocess.run(
        ['bash', '-c', 'source "$1"; ' + function + ' "$2"', 'shadow-test',
         str(ROOT / 'scripts/migrate_lib.sh'), str(path)],
        capture_output=True, text=True, timeout=30)


class ShadowMarkerTests(unittest.TestCase):
    def database(self, root, name, marker=None):
        path = root / name
        with closing(sqlite3.connect(path)) as connection, connection:
            connection.execute('CREATE TABLE metadata_items(id INTEGER PRIMARY KEY)')
            if marker is not None:
                connection.execute('CREATE TABLE plex_pg_shadow_marker(identity TEXT NOT NULL)')
                connection.execute('INSERT INTO plex_pg_shadow_marker VALUES (?)', (marker,))
        return path

    def test_shadow_requires_matching_sidecar(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            matching = self.database(root, 'matching.db', 'identity-1')
            pathlib.Path(str(matching) + '.plex-pg-shadow').write_text('identity-1\n')
            self.assertEqual(run('is_shadow_database', matching).returncode, 0)

            mismatched = self.database(root, 'mismatched.db', 'identity-1')
            pathlib.Path(str(mismatched) + '.plex-pg-shadow').write_text('identity-2\n')
            self.assertNotEqual(run('is_shadow_database', mismatched).returncode, 0)

            # A marker without sidecar may since be a real library: refused.
            unmarked = self.database(root, 'unmarked.db', 'identity-1')
            self.assertNotEqual(run('is_shadow_database', unmarked).returncode, 0)

    def test_recovery_hint_only_for_marker_without_sidecar(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            interrupted = self.database(root, 'interrupted.db', 'identity-1')
            self.assertIn('shadow installation was interrupted',
                          run('shadow_recovery_hint', interrupted).stderr)

            library = self.database(root, 'library.db')
            self.assertEqual(run('shadow_recovery_hint', library).stderr, '')

            marked = self.database(root, 'marked.db', 'identity-1')
            pathlib.Path(str(marked) + '.plex-pg-shadow').write_text('identity-1\n')
            self.assertEqual(run('shadow_recovery_hint', marked).stderr, '')


if __name__ == '__main__':
    unittest.main()
