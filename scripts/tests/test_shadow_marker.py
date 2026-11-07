"""A shadow database is recognised by its marker, with or without its sidecar."""
from contextlib import closing
import pathlib
import sqlite3
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]


def is_shadow(path):
    return subprocess.run(
        ['bash', '-c', 'source "$1"; is_shadow_database "$2"', 'shadow-test',
         str(ROOT / 'scripts/migrate_lib.sh'), str(path)],
        capture_output=True, text=True, timeout=30).returncode == 0


class ShadowMarkerTests(unittest.TestCase):
    def database(self, root, name, marker=None):
        path = root / name
        with closing(sqlite3.connect(path)) as connection, connection:
            connection.execute('CREATE TABLE metadata_items(id INTEGER PRIMARY KEY)')
            if marker is not None:
                connection.execute('CREATE TABLE plex_pg_shadow_marker(identity TEXT NOT NULL)')
                connection.execute('INSERT INTO plex_pg_shadow_marker VALUES (?)', (marker,))
        return path

    def test_marker_and_sidecar(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            matching = self.database(root, 'matching.db', 'identity-1')
            pathlib.Path(str(matching) + '.plex-pg-shadow').write_text('identity-1\n')
            self.assertTrue(is_shadow(matching))

            mismatched = self.database(root, 'mismatched.db', 'identity-1')
            pathlib.Path(str(mismatched) + '.plex-pg-shadow').write_text('identity-2\n')
            self.assertFalse(is_shadow(mismatched))

    def test_marker_without_sidecar_after_interrupted_install(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            self.assertTrue(is_shadow(self.database(root, 'shadow.db', 'identity-1')))

    def test_real_library_is_never_a_shadow(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            library = self.database(root, 'library.db')
            self.assertFalse(is_shadow(library))
            pathlib.Path(str(library) + '.plex-pg-shadow').write_text('identity-1\n')
            self.assertFalse(is_shadow(library))
            self.assertFalse(is_shadow(root / 'missing.db'))


if __name__ == '__main__':
    unittest.main()
