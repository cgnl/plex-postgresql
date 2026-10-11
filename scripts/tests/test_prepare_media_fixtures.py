"""Pinned media transport fallback must never change the fixture content."""

import contextlib
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import urllib.error


SPEC = importlib.util.spec_from_file_location(
    "prepare_media_fixtures", Path(__file__).resolve().parents[1] / "prepare-media-fixtures.py")
fixtures = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(fixtures)
CANONICAL = "https://archive.org/download/Test_Item/exact.mp4"
DIRECT = "https://dn801200.us.archive.org/0/items/Test_Item/exact.mp4"
CONTENT = b"exact pinned movie bytes"


def metadata(**changes):
    value = {"metadata": {"identifier": "Test_Item"}, "d1": "dn801200.us.archive.org",
             "d2": None, "dir": "/0/items/Test_Item",
             "files": [{"name": "exact.mp4", "size": str(len(CONTENT))}]}
    value.update(changes)
    return json.dumps(value).encode()


class PrepareMediaFixturesTests(unittest.TestCase):
    def prepare(self, directory, opener):
        manifest = [{"name": "fixture.mp4", "url": CANONICAL,
                     "sha256": hashlib.sha256(CONTENT).hexdigest()}]
        with patch.object(fixtures.Path, "read_text", return_value=json.dumps(manifest)), \
                patch.object(fixtures.urllib.request, "urlopen", side_effect=opener), \
                patch.object(fixtures.time, "sleep"), \
                contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            fixtures.prepare(directory)

    def test_transport_fallback_downloads_exact_pinned_content(self):
        calls = []
        def open_url(request, **kwargs):
            calls.append(request.full_url)
            if request.full_url == CANONICAL:
                raise urllib.error.HTTPError(CANONICAL, 500, "upstream failure", {}, None)
            return io.BytesIO(metadata() if "/metadata/" in request.full_url else CONTENT)
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            self.prepare(directory, open_url)
            self.assertEqual((directory / "fixture.mp4").read_bytes(), CONTENT)
        self.assertEqual(calls, [CANONICAL, "https://archive.org/metadata/Test_Item", DIRECT])

    def test_wrong_digest_rejected_without_retry_or_cache_replacement(self):
        calls = []
        def open_url(request, **kwargs):
            calls.append(request.full_url)
            return io.BytesIO(b"wrong movie content")
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            target = directory / "fixture.mp4"
            target.write_bytes(b"existing cache retained")
            with self.assertRaisesRegex(ValueError, "Media checksum mismatch"):
                self.prepare(directory, open_url)
            self.assertEqual(target.read_bytes(), b"existing cache retained")
        self.assertEqual(calls, [CANONICAL])

    def test_three_total_media_attempts_even_with_fallback(self):
        calls = []
        def open_url(request, **kwargs):
            calls.append(request.full_url)
            if "/metadata/" in request.full_url:
                return io.BytesIO(metadata())
            raise urllib.error.HTTPError(request.full_url, 500, "upstream failure", {}, None)
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            with self.assertRaises(urllib.error.HTTPError):
                self.prepare(directory, open_url)
            self.assertFalse((directory / "fixture.mp4").exists())
        self.assertEqual(calls, [CANONICAL, "https://archive.org/metadata/Test_Item", DIRECT, DIRECT])

    def test_fallback_wrong_digest_is_rejected(self):
        calls = []
        def open_url(request, **kwargs):
            calls.append(request.full_url)
            if request.full_url == CANONICAL:
                raise urllib.error.HTTPError(CANONICAL, 500, "upstream failure", {}, None)
            return io.BytesIO(metadata() if "/metadata/" in request.full_url else b"wrong fallback content")
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            with self.assertRaisesRegex(ValueError, "Media checksum mismatch"):
                self.prepare(directory, open_url)
            self.assertFalse((directory / "fixture.mp4").exists())
        self.assertEqual(calls, [CANONICAL, "https://archive.org/metadata/Test_Item", DIRECT])

    def test_cached_exact_content_needs_no_network(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            (directory / "fixture.mp4").write_bytes(CONTENT)
            self.prepare(directory, lambda *args, **kwargs: self.fail("Cache unexpectedly downloaded"))

    def test_provider_metadata_cannot_substitute_host_item_or_filename(self):
        cases = [{"d1": "attacker.example"}, {"dir": "/0/items/Other_Item"},
                 {"metadata": {"identifier": "Other_Item"}},
                 {"files": [{"name": "other.mp4", "size": "12"}]}]
        for changes in cases:
            with self.subTest(changes=changes), \
                    patch.object(fixtures.urllib.request, "urlopen", return_value=io.BytesIO(metadata(**changes))):
                with self.assertRaises(ValueError):
                    fixtures.archive_fallbacks(CANONICAL)

    def test_non_archive_download_has_no_provider_fallback(self):
        with patch.object(fixtures.urllib.request, "urlopen") as opener:
            self.assertEqual(fixtures.archive_fallbacks("https://download.blender.org/movie.zip"), [])
            opener.assert_not_called()


if __name__ == "__main__":
    unittest.main()
