#!/usr/bin/env python3
"""Download immutable real-media fixtures before entering the isolated Docker network."""

import hashlib
import json
from pathlib import Path
import re
import shutil
import sys
import tempfile
import time
import urllib.request
import urllib.parse
import zipfile


USER_AGENT = "plex-postgresql media-fixture-tests (+https://github.com/cgnl/plex-postgresql)"


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def archive_fallbacks(url):
    """Resolve official storage endpoints for the same Archive.org file only."""
    parsed = urllib.parse.urlsplit(url)
    if parsed.scheme != "https" or parsed.netloc != "archive.org":
        return []
    parts = parsed.path.split("/")
    if len(parts) != 4 or parts[1] != "download" or parsed.query or parsed.fragment:
        return []
    identifier, filename = map(urllib.parse.unquote, parts[2:])
    if not re.fullmatch(r"[A-Za-z0-9_.-]+", identifier) or "/" in filename or filename in ("", ".", ".."):
        raise ValueError("Invalid Archive.org fixture path")
    metadata_url = "https://archive.org/metadata/" + identifier
    request = urllib.request.Request(metadata_url, headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(request, timeout=30) as response:
        body = response.read(1024 * 1024 + 1)
    if len(body) > 1024 * 1024:
        raise ValueError("Archive.org metadata exceeded size limit")
    metadata = json.loads(body)
    if metadata.get("metadata", {}).get("identifier") != identifier:
        raise ValueError("Archive.org metadata identifies a different item")
    matches = [entry for entry in metadata.get("files", []) if entry.get("name") == filename]
    if len(matches) != 1 or not 0 < int(matches[0].get("size", "0")) <= 300 * 1024 * 1024:
        raise ValueError("Archive.org metadata lacks the exact bounded fixture file")
    directory = metadata.get("dir", "")
    if not re.fullmatch(r"/[0-9]{1,3}/items/" + re.escape(identifier), directory):
        raise ValueError("Unexpected Archive.org storage directory")
    urls = []
    for key in ("d1", "d2"):
        host = metadata.get(key)
        if not host:
            continue
        if not isinstance(host, str) or not re.fullmatch(r"(?:ia|dn)[0-9]+\.(?:us|eu)\.archive\.org", host):
            raise ValueError("Unexpected Archive.org storage host")
        endpoint = "https://" + host + directory + "/" + urllib.parse.quote(filename, safe="")
        if endpoint not in urls:
            urls.append(endpoint)
    return urls


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
            urls = [item["url"]]
            for attempt in range(3):
                try:
                    url = urls[min(attempt, len(urls) - 1)]
                    print("Downloading " + item["name"] + " from " + url, flush=True)
                    deadline = time.monotonic() + 240
                    size = 0
                    # Blender rejects urllib's default Python User-Agent. Identify
                    # the actual project instead of impersonating a web browser.
                    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
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
                except (OSError, TimeoutError) as error:
                    print(f"Fixture {item['name']} attempt {attempt + 1}/3 failed: {error}",
                          file=sys.stderr, flush=True)
                    if attempt == 2:
                        raise
                    if attempt == 0:
                        try:
                            urls += archive_fallbacks(item["url"])
                        except (OSError, TimeoutError, ValueError) as fallback_error:
                            print("Official Archive.org fallback unavailable: " + str(fallback_error),
                                  file=sys.stderr, flush=True)
                    time.sleep(2)
            unpacked.replace(target)
            print("PASS downloaded fixture: " + item["name"], flush=True)
    (directory / "media-fixtures.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print("PASS pinned real-media fixtures: Big Buck Bunny and Beverly Hillbillies S01E01/E02")


if __name__ == "__main__":
    prepare(Path(sys.argv[1]))
