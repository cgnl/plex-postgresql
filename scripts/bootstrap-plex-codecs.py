#!/usr/bin/env python3
"""Install official build-matched Plex H264/AAC decoders and record evidence.

Run inside the disposable native Plex fixture while outbound HTTPS is enabled:
python3 /tmp/bootstrap-plex-codecs.py /tmp/codec-bootstrap.json
"""

import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET


CODECS_ROOT = Path("/config/Library/Application Support/Plex Media Server/Codecs")
TRANSCODER = "/usr/lib/plexmediaserver/Plex Transcoder"
MAX_CODEC_BYTES = 32 * 1024 * 1024


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def retry(operation):
    for attempt in range(3):
        try:
            return operation()
        except (urllib.error.URLError, TimeoutError, OSError):
            if attempt == 2:
                raise
            time.sleep(2 ** (attempt + 1))


def trusted_url(url, host):
    parsed = urllib.parse.urlsplit(url)
    require(parsed.scheme == "https" and parsed.hostname == host
            and parsed.port in (None, 443) and not parsed.username and not parsed.password,
            "Codec endpoint did not use the expected official HTTPS host")


def codec_metadata(api_url, codec, build, native_build):
    trusted_url(api_url, "servers.plex.tv")
    with urllib.request.urlopen(api_url, timeout=30) as response:
        trusted_url(response.geturl(), "servers.plex.tv")
        body = response.read(65537)
    require(len(body) <= 65536, "Official codec metadata exceeded size limit")
    root = ET.fromstring(body)
    require(root.get("codec") == codec and root.get("version") == build,
            "Official codec response did not match requested codec/build")
    entries = list(root.iter("Codec"))
    require(len(entries) == 1, "Expected exactly one official codec download")
    entry = entries[0]
    require(entry.get("fileName") == "lib" + codec + ".so"
            and entry.get("build") == native_build, "Unexpected codec file name or architecture")
    digest = entry.get("fileSha256", "")
    require(re.fullmatch(r"[0-9a-fA-F]{64}", digest), "Missing official codec SHA256")
    trusted_url(entry.get("url", ""), "downloads.plex.tv")
    return entry.get("url"), entry.get("fileName"), digest.lower()


def install_codec(url, target, expected_hash):
    temporary = None
    try:
        digest = hashlib.sha256()
        size = 0
        deadline = time.monotonic() + 120
        with urllib.request.urlopen(url, timeout=30) as response:
            trusted_url(response.geturl(), "downloads.plex.tv")
            with tempfile.NamedTemporaryFile(prefix=".codec-download-", dir=target.parent,
                                             delete=False) as output:
                temporary = Path(output.name)
                while True:
                    require(time.monotonic() < deadline, "Codec download exceeded 120 seconds")
                    chunk = response.read(1024 * 1024)
                    if not chunk:
                        break
                    size += len(chunk)
                    require(size <= MAX_CODEC_BYTES, "Official codec exceeded 32 MiB size limit")
                    output.write(chunk)
                    digest.update(chunk)
        require(size > 0 and digest.hexdigest() == expected_hash,
                "Official codec download SHA256 mismatch")
        with temporary.open("rb") as downloaded:
            require(downloaded.read(4) == b"\x7fELF", "Official codec was not an ELF library")
        temporary.chmod(0o644)
        owner = target.parent.stat()
        if os.geteuid() == 0:
            os.chown(temporary, owner.st_uid, owner.st_gid)
        os.replace(temporary, target)
        return {"path": str(target), "bytes": size, "sha256": digest.hexdigest(),
                "matches_official_sha256": True}
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def bootstrap(result):
    machine = platform.machine()
    architecture = {"aarch64": "aarch64", "arm64": "aarch64", "x86_64": "x86_64"}.get(machine)
    require(architecture is not None, "Unsupported native codec architecture: " + machine)
    environment = {key: value for key, value in os.environ.items()
                   if not key.startswith(("PLEX_PG_", "PG"))
                   and key not in ("LD_PRELOAD", "DYLD_INSERT_LIBRARIES", "FFMPEG_EXTERNAL_LIBS")}
    environment["LD_LIBRARY_PATH"] = "/usr/lib/plexmediaserver/lib"
    version = subprocess.run([TRANSCODER, "-version"], env=environment, capture_output=True,
                             text=True, timeout=15, check=True)
    match = re.search(r"^ffmpeg version (\S+)", version.stdout + version.stderr, re.M)
    require(match is not None, "Cannot determine bundled Plex decoder version")
    build = match.group(1)
    require(re.fullmatch(r"[a-zA-Z0-9.-]+", build), "Invalid bundled Plex codec build")
    device_path = CODECS_ROOT / ".device-id"
    deadline = time.monotonic() + 60
    while not device_path.is_file() and time.monotonic() < deadline:
        time.sleep(2)
    require(device_path.is_file(), "Plex did not initialize its codec device ID")
    device = device_path.read_text().strip()
    require(bool(device) and len(device) <= 512, "Invalid Plex codec device ID")
    native_build = "linux-" + architecture + "-standard"
    directory = CODECS_ROOT / (build + "-linux-" + architecture)
    if not directory.exists():
        directory.mkdir(parents=True)
        if os.geteuid() == 0:
            owner = device_path.stat()
            os.chown(directory, owner.st_uid, owner.st_gid)
    result.update({"decoder_build": build, "native_build": native_build,
                   "directory": str(directory), "codecs": []})
    for codec in ("h264_decoder", "aac_decoder"):
        api_url = "https://servers.plex.tv/api/codecs/" + codec + "?" + urllib.parse.urlencode({
            "build": native_build, "deviceId": device, "version": build})
        url, name, digest = retry(lambda: codec_metadata(api_url, codec, build, native_build))
        installed = retry(lambda: install_codec(url, directory / name, digest))
        installed.update({"codec": codec, "metadata_url": api_url, "download_url": url,
                          "official_sha256": digest})
        result["codecs"].append(installed)


def main():
    if len(sys.argv) != 2:
        raise SystemExit("Usage: bootstrap-plex-codecs.py OUTPUT_EVIDENCE_JSON")
    evidence = Path(sys.argv[1])
    evidence.parent.mkdir(parents=True, exist_ok=True)
    result = {"passed": False, "source": "official Plex codec API"}
    status = 0
    try:
        bootstrap(result)
        result["passed"] = True
    except (RuntimeError, OSError, ValueError, ET.ParseError, subprocess.SubprocessError) as error:
        result["error"] = str(error)
        print("FAIL official Plex codec bootstrap: " + str(error), file=sys.stderr)
        status = 1
    evidence.write_text(json.dumps(result, indent=2) + "\n")
    if not status:
        print("PASS official build-matched Plex H264/AAC codec bootstrap")
    return status


if __name__ == "__main__":
    sys.exit(main())
