#!/usr/bin/env python3
"""Download immutable real-media fixtures before entering the isolated Docker network."""

import hashlib
import json
from pathlib import Path
import shutil
import sys
import tempfile
import time
import urllib.request
import zipfile


USER_AGENT = "plex-postgresql media-fixture-tests (+https://github.com/cgnl/plex-postgresql)"


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def prepare(directory):
    directory.mkdir(parents=True, exist_ok=True)
    manifest = json.loads(Path(__file__).with_name("media-fixtures.json").read_text())
    for item in manifest:
        target = directory / item["name"]
        if target.is_file() and digest(target) == item["sha256"]:
            print("PASS cached fixture: " + item["name"], flush=True)
            continue
        with tempfile.TemporaryDirectory(prefix="plex-media-download-", dir=directory) as temporary:
            download = Path(temporary) / "download"
            for attempt in range(3):
                try:
                    print("Downloading " + item["name"] + " from " + item["url"], flush=True)
                    deadline = time.monotonic() + 240
                    size = 0
                    # Blender rejects urllib's default Python User-Agent. Identify
                    # the actual project instead of impersonating a web browser.
                    request = urllib.request.Request(item["url"], headers={"User-Agent": USER_AGENT})
                    with urllib.request.urlopen(request, timeout=30) as response, download.open("wb") as output:
                        while chunk := response.read(1024 * 1024):
                            size += len(chunk)
                            if size > 300 * 1024 * 1024 or time.monotonic() > deadline:
                                raise RuntimeError("Media download exceeded size/time limit")
                            output.write(chunk)
                    if "zip_member" in item:
                        if digest(download) != item["archive_sha256"]:
                            raise ValueError("Archive checksum mismatch: " + item["name"])
                        unpacked = Path(temporary) / "media"
                        with zipfile.ZipFile(download) as archive:
                            if archive.getinfo(item["zip_member"]).file_size > 300 * 1024 * 1024:
                                raise RuntimeError("Oversized archived media")
                            with archive.open(item["zip_member"]) as source, unpacked.open("wb") as output:
                                shutil.copyfileobj(source, output)
                    else:
                        unpacked = download
                    if digest(unpacked) != item["sha256"]:
                        raise ValueError("Media checksum mismatch: " + item["name"])
                    break
                except (OSError, TimeoutError, ValueError, zipfile.BadZipFile) as error:
                    print(f"Fixture {item['name']} attempt {attempt + 1}/3 failed: {error}",
                          file=sys.stderr, flush=True)
                    if attempt == 2:
                        raise
                    time.sleep(2)
            unpacked.replace(target)
            print("PASS downloaded fixture: " + item["name"], flush=True)
    (directory / "media-fixtures.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print("PASS pinned real-media fixtures: Big Buck Bunny and Beverly Hillbillies S01E01/E02")


if __name__ == "__main__":
    prepare(Path(sys.argv[1]))
