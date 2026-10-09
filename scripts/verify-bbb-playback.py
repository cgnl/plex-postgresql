#!/usr/bin/env python3
"""Verify scanned Big Buck Bunny playback through a running local Plex server.

Run inside the native Plex container: section_id media_file output_evidence_dir.
Only Python's standard library and the bundled Plex Transcoder are required.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET


BASE_URL = "http://127.0.0.1:32400"
TRANSCODER = "/usr/lib/plexmediaserver/Plex Transcoder"
CHUNK_SIZE = 1024 * 1024
CODECS_ROOT = Path("/config/Library/Application Support/Plex Media Server/Codecs")


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def fetch_xml(path):
    with urllib.request.urlopen(BASE_URL + path, timeout=10) as response:
        body = response.read(8 * CHUNK_SIZE + 1)
    require(len(body) <= 8 * CHUNK_SIZE, "Plex metadata exceeded size limit")
    return body, ET.fromstring(body)


def hash_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(CHUNK_SIZE), b""):
            digest.update(chunk)
    return digest.hexdigest()


def decoder_environment():
    environment = {key: value for key, value in os.environ.items()
                   if not key.startswith(("PLEX_PG_", "PG"))
                   and key not in ("LD_PRELOAD", "DYLD_INSERT_LIBRARIES", "FFMPEG_EXTERNAL_LIBS")}
    environment["LD_LIBRARY_PATH"] = "/usr/lib/plexmediaserver/lib"
    version = subprocess.run([TRANSCODER, "-version"], env=environment,
                             capture_output=True, text=True, timeout=15, check=True)
    match = re.search(r"^ffmpeg version (\S+)", version.stdout + version.stderr, re.M)
    require(match is not None, "Cannot determine bundled Plex decoder version")
    build = match.group(1)
    libraries = [library for library in CODECS_ROOT.glob("*/libh264_decoder.so")
                 if library.parent.name.startswith(build + "-linux-") and library.is_file()]
    require(len(libraries) == 1,
            "Expected exactly one installed H264 codec for Plex decoder build " + build
            + "; let Plex install its official codec before offline playback verification")
    library = libraries[0]
    environment["FFMPEG_EXTERNAL_LIBS"] = str(library.parent) + "/"
    return environment, {"build": build, "directory": str(library.parent),
                         "h264_decoder_sha256": hash_file(library),
                         "decoder_libraries": {
                             item.name: hash_file(item)
                             for item in sorted(library.parent.glob("lib*_decoder.so"))
                             if item.is_file()}}


def scanned_media(section_id, source, evidence_dir, kind, season, episode):
    deadline = time.monotonic() + 180
    last_error = "Movie not yet scanned/analyzed"
    while time.monotonic() < deadline:
        try:
            discovery_path = "/library/sections/" + section_id + "/all"
            if kind == "episode":
                discovery_path += "?type=4"
            _, root = fetch_xml(discovery_path)
            videos = [video for video in root.iter("Video")
                      if any(part.get("file") == str(source) for part in video.iter("Part"))]
            require(len(videos) <= 1, "Scanner duplicated Big Buck Bunny metadata")
            if videos:
                rating_key = videos[0].get("ratingKey", "")
                require(re.fullmatch(r"[1-9][0-9]*", rating_key), "Missing movie ratingKey")
                body, metadata = fetch_xml("/library/metadata/" + rating_key)
                matches = [(video, media, part) for video in metadata.iter("Video")
                           for media in video.findall("Media") for part in media.findall("Part")
                           if part.get("file") == str(source)]
                require(len(matches) <= 1, "Movie does not have a unique media part")
                if matches:
                    video, media, part = matches[0]
                    require(video.get("type") == kind, "Unexpected scanned media type")
                    if kind == "episode":
                        require(int(video.get("parentIndex", "0")) == season
                                and int(video.get("index", "0")) == episode,
                                "Scanned episode has incorrect season/episode numbers")
                    duration = int(media.get("duration", video.get("duration", "0")))
                    audio = media.get("audioCodec") or next(
                        (stream.get("codec") for stream in part.findall("Stream")
                         if stream.get("streamType") == "2"), None)
                    if media.get("videoCodec") == "h264" and audio and duration > 500000:
                        require(video.get("ratingKey") == rating_key, "Movie identity changed")
                        require(part.get("key", "").startswith("/library/parts/"),
                                "Missing Plex original-file route")
                        require(int(media.get("width", "0")) > 0
                                and int(media.get("height", "0")) > 0, "Missing video dimensions")
                        require(int(part.get("size", "0")) == source.stat().st_size,
                                "Plex analyzed size differs from source file")
                        (evidence_dir / "bbb-scanned-media.xml").write_bytes(body)
                        return rating_key, media, part, duration, audio
                    last_error = "Movie lacks analyzed H264/audio or duration >500 seconds"
        except (urllib.error.URLError, TimeoutError) as error:
            last_error = str(error)
        time.sleep(2)
    raise RuntimeError("Plex scan timed out: " + last_error)


def verify_ranges(part_key, source, size):
    results = []
    for start in (0, size // 2, max(0, size - 65536)):
        end = min(size - 1, start + 65535)
        request = urllib.request.Request(BASE_URL + part_key,
                                         headers={"Range": f"bytes={start}-{end}"})
        with urllib.request.urlopen(request, timeout=30) as response:
            require(response.status == 206, "Plex did not honor byte-range playback request")
            require(response.headers.get("Content-Range") == f"bytes {start}-{end}/{size}",
                    "Plex returned an incorrect Content-Range")
            served = response.read(end - start + 2)
        with source.open("rb") as original:
            original.seek(start)
            expected = original.read(end - start + 1)
        require(served == expected, "Plex byte-range differs from source")
        results.append({"start": start, "end": end, "bytes": len(served),
                        "sha256": hashlib.sha256(served).hexdigest(), "matches_source": True})
    return results


def verify_playback(section_id, source, evidence_dir, result, kind, season, episode, sample_seconds):
    require(re.fullmatch(r"[1-9][0-9]*", section_id), "Invalid section ID")
    require(source.is_file() and source.stat().st_size > 0, "Missing movie fixture")
    source_hash = hash_file(source)
    expected_hash = os.environ.get("BBB_SHA256")
    if expected_hash:
        require(source_hash == expected_hash, "Movie fixture differs from pinned BBB_SHA256")
    result["source"] = {"path": str(source), "bytes": source.stat().st_size,
                        "sha256": source_hash,
                        "url": os.environ.get("BBB_SOURCE_URL"),
                        "expected_sha256": expected_hash}
    rating_key, media, part, duration, audio = scanned_media(
        section_id, source, evidence_dir, kind, season, episode)
    result["media"] = {"section_id": section_id, "rating_key": rating_key,
                       "part_key": part.get("key"), "duration_ms": duration,
                       "video_codec": media.get("videoCodec"), "audio_codec": audio,
                       "width": int(media.get("width")), "height": int(media.get("height")),
                       "container": media.get("container"), "kind": kind,
                       "season": season, "episode": episode}
    result["ranges"] = verify_ranges(part.get("key"), source, source.stat().st_size)
    with tempfile.TemporaryDirectory(prefix="plex-bbb-playback-") as directory:
        if sample_seconds:
            decoder_input = BASE_URL + part.get("key")
            result["delivery"] = {"mode": "direct_http_sample", "sample_seconds": sample_seconds,
                                  "full_file_hash_verified": False}
        else:
            downloaded = Path(directory) / "served-movie.mp4"
            digest = hashlib.sha256()
            received = 0
            deadline = time.monotonic() + 240
            with urllib.request.urlopen(BASE_URL + part.get("key"), timeout=30) as response:
                require(response.status == 200, "Plex full-file response was not HTTP 200")
                with downloaded.open("wb") as target:
                    while True:
                        require(time.monotonic() < deadline, "Full movie delivery exceeded 240 seconds")
                        chunk = response.read(CHUNK_SIZE)
                        if not chunk:
                            break
                        received += len(chunk)
                        require(received <= source.stat().st_size, "Plex delivered oversized movie")
                        target.write(chunk)
                        digest.update(chunk)
            require(received == source.stat().st_size and digest.hexdigest() == source_hash,
                    "Plex original-file playback delivered truncated or different bytes")
            result["delivery"] = {"mode": "full_file", "bytes": received, "sha256": digest.hexdigest(),
                                  "matches_source": True, "http_status": 200,
                                  "full_file_hash_verified": True}
            decoder_input = str(downloaded)
        environment, result["codecs"] = decoder_environment()
        command = [TRANSCODER, "-hide_banner", "-loglevel", "error", "-xerror",
                   "-err_detect", "explode", "-threads", "2", "-i", decoder_input,
                   "-map", "0:v:0", "-map", "0:a:0", "-threads", "2",
                   "-progress", "pipe:1", "-nostats", "-f", "null", "-"]
        if sample_seconds:
            command[-1:-1] = ["-t", str(sample_seconds)]
        started = time.monotonic()
        with (evidence_dir / "bbb-decode.stdout.log").open("wb") as stdout, \
                (evidence_dir / "bbb-decode.stderr.log").open("wb") as stderr:
            decoded = subprocess.run(command, env=environment, stdout=stdout, stderr=stderr,
                                     timeout=360 if kind == "episode" else 240, check=False)
        progress = (evidence_dir / "bbb-decode.stdout.log").read_text(errors="replace")
        timestamps = [int(value) for value in re.findall(r"^out_time_us=(\d+)$", progress, re.M)]
        frames = [int(value) for value in re.findall(r"^frame=(\d+)$", progress, re.M)]
        result["decode"] = {"returncode": decoded.returncode,
                            "elapsed_seconds": round(time.monotonic() - started, 3),
                            "decoded_time_us": max(timestamps, default=0),
                            "decoded_video_frames": max(frames, default=0),
                            "mapped_streams": ["0:v:0", "0:a:0"],
                            "input": "plex_http" if sample_seconds else "plex_delivered_file",
                            "full_movie": not bool(sample_seconds),
                            "decoder": TRANSCODER, "strict_errors": True}
        require(decoded.returncode == 0, "Bundled Plex decoder rejected served movie; see stderr log")
        expected_time_us = sample_seconds * 1000000 if sample_seconds else duration * 1000
        # FFmpeg progress reports the final frame timestamp, which can precede
        # the requested sample boundary by one frame interval.
        minimum_time_us = expected_time_us - max(expected_time_us * 0.01, 100000)
        require("progress=end" in progress and max(frames, default=0) > 0
                and max(timestamps, default=0) >= minimum_time_us,
                "Decoder did not complete the requested audio/video playback duration")
        result["decode"]["completed"] = True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("section_id")
    parser.add_argument("media_file")
    parser.add_argument("output_evidence_dir")
    parser.add_argument("--kind", choices=("movie", "episode"), default="movie")
    parser.add_argument("--season", type=int)
    parser.add_argument("--episode", type=int)
    parser.add_argument("--sample-seconds", type=int, default=0,
                        help="Decode this many seconds directly over HTTP instead of full-file delivery")
    args = parser.parse_args()
    if args.kind == "episode" and (args.season is None or args.episode is None
                                   or args.season < 0 or args.episode < 1):
        parser.error("--kind episode requires --season >=0 and --episode >=1")
    if args.kind == "movie" and (args.season is not None or args.episode is not None):
        parser.error("--season and --episode require --kind episode")
    if not 0 <= args.sample_seconds <= 500:
        parser.error("--sample-seconds must be between 0 (full playback) and 500")
    evidence_dir = Path(args.output_evidence_dir)
    evidence_dir.mkdir(parents=True, exist_ok=True)
    result = {"passed": False, "playback_kind": (
        "Plex HTTP original-file sample streaming and native decode" if args.sample_seconds else
        "Plex HTTP original-file delivery and full native decode"),
              "browser_playback_verified": False, "server_transcoding_verified": False}
    status = 0
    try:
        verify_playback(args.section_id, Path(args.media_file), evidence_dir, result,
                        args.kind, args.season, args.episode, args.sample_seconds)
        result["passed"] = True
    except (RuntimeError, OSError, ValueError, ET.ParseError, subprocess.SubprocessError) as error:
        result["error"] = str(error)
        print("FAIL real media playback: " + str(error), file=sys.stderr)
        status = 1
    (evidence_dir / "bbb-playback.json").write_text(json.dumps(result, indent=2) + "\n")
    if not status:
        print("PASS real media: scanned H264/audio, exact byte ranges and "
              + ("HTTP sample decode" if args.sample_seconds else "full served bytes and native decode"))
    return status


if __name__ == "__main__":
    sys.exit(main())
