import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location(
    "export_pg_to_sqlite", Path(__file__).resolve().parents[1] / "export_pg_to_sqlite.py")
EXPORT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(EXPORT)


class PublicationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.output = Path(self.temporary.name)
        self.stage = self.output / "stage"
        self.stage.mkdir()
        for name in (EXPORT.LIBRARY, EXPORT.ARTWORK):
            (self.output / name).write_bytes(b"old-" + name.encode())
            (self.stage / name).write_bytes(b"new-" + name.encode())

    def assert_originals(self):
        for name in (EXPORT.LIBRARY, EXPORT.ARTWORK):
            self.assertEqual((self.output / name).read_bytes(), b"old-" + name.encode())

    def test_success_retains_backups_after_staging_removed(self):
        backups = EXPORT.promote(self.stage, self.output)
        self.stage.rmdir()
        for name, backup in backups.items():
            self.assertEqual(backup.read_bytes(), b"old-" + name.encode())
            self.assertEqual((self.output / name).read_bytes(), b"new-" + name.encode())
            self.assertEqual(backup.parent.stat().st_mode & 0o777, 0o700)

    def test_second_replace_failure_restores_pair_and_keeps_backups(self):
        replace = EXPORT.os.replace

        def fail_second(source, target):
            if source == self.stage / EXPORT.ARTWORK:
                raise OSError("injected publication failure")
            return replace(source, target)

        with mock.patch.object(EXPORT.os, "replace", side_effect=fail_second):
            with self.assertRaisesRegex(OSError, "injected"):
                EXPORT.promote(self.stage, self.output)
        self.assert_originals()
        backups = list(self.output.glob(".pg-to-sqlite-backup-*"))
        self.assertEqual(len(backups), 1)
        for name in (EXPORT.LIBRARY, EXPORT.ARTWORK):
            self.assertEqual((backups[0] / name).read_bytes(), b"old-" + name.encode())

    def test_backup_failure_does_not_publish(self):
        with mock.patch.object(EXPORT.os, "link", side_effect=OSError("backup failed")):
            with self.assertRaisesRegex(OSError, "backup failed"):
                EXPORT.promote(self.stage, self.output)
        self.assert_originals()

    def test_backup_sync_failure_does_not_publish(self):
        fsync = EXPORT.os.fsync
        calls = 0

        def fail_backup_sync(fd):
            nonlocal calls
            calls += 1
            if calls == 3:  # Both staged outputs synced, then first backup.
                raise OSError("backup sync failed")
            return fsync(fd)

        with mock.patch.object(EXPORT.os, "fsync", side_effect=fail_backup_sync):
            with self.assertRaisesRegex(OSError, "backup sync failed"):
                EXPORT.promote(self.stage, self.output)
        self.assert_originals()

    def test_sidecars_fail_before_backup_or_publication(self):
        for directory in (self.output, self.stage):
            for suffix in ("-wal", "-shm", "-journal"):
                with self.subTest(directory=directory, suffix=suffix):
                    sidecar = directory / (EXPORT.LIBRARY + suffix)
                    sidecar.write_bytes(b"pending SQLite state")
                    with self.assertRaisesRegex(ValueError, "sidecar"):
                        EXPORT.promote(self.stage, self.output)
                    sidecar.unlink()
                    self.assert_originals()
                    self.assertEqual(list(self.output.glob(".pg-to-sqlite-backup-*")), [])

    def test_absent_output_is_removed_on_pair_failure(self):
        (self.output / EXPORT.LIBRARY).unlink()
        replace = EXPORT.os.replace

        def fail_second(source, target):
            if source == self.stage / EXPORT.ARTWORK:
                raise OSError("injected failure")
            return replace(source, target)

        with mock.patch.object(EXPORT.os, "replace", side_effect=fail_second):
            with self.assertRaises(OSError):
                EXPORT.promote(self.stage, self.output)
        self.assertFalse((self.output / EXPORT.LIBRARY).exists())
        self.assertEqual((self.output / EXPORT.ARTWORK).read_bytes(),
                         b"old-" + EXPORT.ARTWORK.encode())


if __name__ == "__main__":
    unittest.main()
