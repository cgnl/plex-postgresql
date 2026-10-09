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


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def prepare(directory):
    directory.mkdir(parents=True, exist_ok=True)
    manifest = json.loads(Path(__file__).with_name("media-fixtures.json").read_text())
    for item in manifest:
        target = directory / item["name"]
        if target.is_file() and digest(target) == item["sha256"]:
            continue
        with tempfile.TemporaryDirectory(prefix="plex-media-download-", dir=directory) as temporary:
            download = Path(temporary) / "download"
            for attempt in range(3):
                try:
                    deadline = time.monotonic() + 240
                    size = 0
                    with urllib.request.urlopen(item["url"], timeout=30) as response, download.open("wb") as output:
                        while chunk := response.read(1024 * 1024):
                            size += len(chunk)
                            if size > 300 * 1024 * 1024 or time.monotonic() > deadline:
                                raise RuntimeError("Media download exceeded size/time limit")
                            output.write(chunk)
                    break
                except (OSError, TimeoutError):
                    if attempt == 2:
                        raise
                    time.sleep(2)
            if "zip_member" in item:
                if digest(download) != item["archive_sha256"]:
                    raise RuntimeError("Archive checksum mismatch: " + item["name"])
                unpacked = Path(temporary) / "media"
                with zipfile.ZipFile(download) as archive:
                    if archive.getinfo(item["zip_member"]).file_size > 300 * 1024 * 1024:
                        raise RuntimeError("Oversized archived media")
                    with archive.open(item["zip_member"]) as source, unpacked.open("wb") as output:
                        shutil.copyfileobj(source, output)
            else:
                unpacked = download
            if digest(unpacked) != item["sha256"]:
                raise RuntimeError("Media checksum mismatch: " + item["name"])
            unpacked.replace(target)
    (directory / "media-fixtures.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print("PASS pinned real-media fixtures: Big Buck Bunny and Beverly Hillbillies S01E01/E02")


if __name__ == "__main__":
    prepare(Path(sys.argv[1]))
