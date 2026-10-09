#!/usr/bin/env python3
"""Resolve candidate inputs; never change a release, tag, or compatibility claim."""

import argparse
import datetime
import hashlib
import json
import os
import re
import sys
import urllib.parse
import urllib.request
from pathlib import Path


PLEX_API = "https://plex.tv/api/downloads/5.json"
IMAGES = {
    "linuxserver": ("linuxserver/plex", "latest"),
    "plexinc": ("plexinc/pms-docker", "latest"),
    "builder": ("library/alpine", "3.15"),
    "postgres": ("library/postgres", "16-bookworm"),
    "postgres15": ("library/postgres", "15-bookworm"),
    "postgres18": ("library/postgres", "18-bookworm"),
}
ACCEPT = ", ".join([
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
])


def fetch(url, headers=None):
    request = urllib.request.Request(url, headers=headers or {})
    with urllib.request.urlopen(request, timeout=60) as response:
        return response.read(), response.headers


def resolve_image(repo, tag):
    query = urllib.parse.urlencode({
        "service": "registry.docker.io", "scope": f"repository:{repo}:pull",
    })
    token_body, _ = fetch(f"https://auth.docker.io/token?{query}")
    token = json.loads(token_body)["token"]
    body, headers = fetch(
        f"https://registry-1.docker.io/v2/{repo}/manifests/{tag}",
        {"Authorization": f"Bearer {token}", "Accept": ACCEPT},
    )
    digest = headers.get("Docker-Content-Digest", "")
    if digest != f"sha256:{hashlib.sha256(body).hexdigest()}":
        raise ValueError(f"Missing or invalid registry digest for {repo}:{tag}")
    manifest = json.loads(body)
    platforms = {}
    for entry in manifest.get("manifests", []):
        platform = entry.get("platform", {})
        arch = platform.get("architecture")
        if platform.get("os") == "linux" and arch in ("amd64", "arm64"):
            child_digest = entry.get("digest", "")
            if not re.fullmatch(r"sha256:[0-9a-f]{64}", child_digest):
                raise ValueError(f"Invalid {arch} manifest digest for {repo}:{tag}")
            if arch in platforms:
                raise ValueError(f"Ambiguous {arch} platform for {repo}:{tag}")
            platforms[arch] = child_digest
    if set(platforms) != {"amd64", "arm64"}:
        raise ValueError(f"Both native architectures are required for {repo}:{tag}")
    return {"source": f"{repo}:{tag}", "ref": f"{repo}@{digest}",
            "digest": digest, "platforms": platforms}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=Path("upstream-candidate.json"))
    args = parser.parse_args()
    api_body, _ = fetch(PLEX_API)
    linux = json.loads(api_body)["computer"]["Linux"]
    plex_version = linux["version"]
    if not re.fullmatch(r"[0-9]+(?:\.[0-9]+){3}-[0-9a-f]+", plex_version):
        raise ValueError("Official API returned an invalid Linux version")
    releases = {}
    for arch, build in (("amd64", "linux-x86_64"), ("arm64", "linux-aarch64")):
        matches = [release for release in linux["releases"]
                   if release.get("build") == build and release.get("distro") == "debian"
                   and release.get("version") == plex_version]
        if len(matches) != 1:
            raise ValueError(f"Official API lacks an unambiguous {arch} release")
        release = matches[0]
        if urllib.parse.urlparse(release["url"]).hostname != "downloads.plex.tv":
            raise ValueError("Unexpected official release download host")
        releases[arch] = {key: release[key] for key in ("url", "checksum", "build")}
    images = {name: resolve_image(repo, tag) for name, (repo, tag) in IMAGES.items()}
    baseline_path = Path(".github/upstream-digests.json")
    baseline = json.loads(baseline_path.read_text()) if baseline_path.exists() else {}
    updated = any(baseline.get(images[name]["source"]) != images[name]["digest"]
                  for name in ("linuxserver", "plexinc"))
    snapshot = {
        "schema_version": 1,
        "observed_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "plex_api": PLEX_API, "plex_version": plex_version,
        "official_releases": releases, "images": images,
        "upstream_digest_changed": updated,
        "promotion_allowed": False,
        "promotion_blocker": "Full native matrix and sustained workload certification are incomplete",
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    temporary = args.output.with_suffix(args.output.suffix + ".tmp")
    temporary.write_text(json.dumps(snapshot, indent=2) + "\n")
    temporary.replace(args.output)
    print(json.dumps(snapshot, indent=2))
    if "GITHUB_OUTPUT" in os.environ:
        with open(os.environ["GITHUB_OUTPUT"], "a") as output:
            output.write(f"updated={str(updated).lower()}\nplex_version={plex_version}\n")
            for name, image in images.items():
                output.write(f"{name}_image={image['ref']}\n")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"Candidate resolution failed: {error}", file=sys.stderr)
        sys.exit(1)
