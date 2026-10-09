#!/usr/bin/env bash
set -euo pipefail

: "${CANDIDATE_IMAGE:?immutable candidate image required}"
: "${POSTGRES_IMAGE:?digest-pinned PostgreSQL image required}"
: "${EXPECTED_PLEX_VERSION:?official Linux Plex version required}"
: "${EXPECTED_ARCH:?native architecture required}"
: "${VARIANT:?linuxserver or plexinc required}"
EVIDENCE_DIR=${EVIDENCE_DIR:-container-evidence}
restart_cycles=${PLEX_E2E_RESTART_CYCLES:-1}
[[ "$restart_cycles" =~ ^[0-9]{1,5}$ ]] || { echo "Invalid restart cycle count" >&2; exit 2; }
restart_cycles=$((10#$restart_cycles))
((restart_cycles >= 1 && restart_cycles <= 10000)) || { echo "Restart cycles must be between 1 and 10000" >&2; exit 2; }
restart_cycles_completed=0
soak_seconds=${PLEX_E2E_SOAK_SECONDS:-0}
[[ "$soak_seconds" =~ ^[0-9]{1,5}$ ]] || { echo "Invalid soak duration" >&2; exit 2; }
soak_seconds=$((10#$soak_seconds))
((soak_seconds <= 21000)) || { echo "Soak cannot exceed 5h50m" >&2; exit 2; }
soak_seconds_completed=0
soak_iterations=0
live_recovery_verified=0
decoded_fixture_verified=0
mkdir -p "$EVIDENCE_DIR"
for tool in docker python3; do command -v "$tool" >/dev/null; done
[[ "$CANDIDATE_IMAGE" =~ (^|@)sha256:[0-9a-f]{64}$ ]] || { echo "Candidate must be immutable" >&2; exit 1; }
[[ "$POSTGRES_IMAGE" =~ @sha256:[0-9a-f]{64}$ ]] || { echo "PostgreSQL must be digest-pinned" >&2; exit 1; }
[[ "$EXPECTED_ARCH" == amd64 || "$EXPECTED_ARCH" == arm64 ]]
[[ "$VARIANT" == linuxserver || "$VARIANT" == plexinc ]]
fixture="plex-canary-$(python3 -c 'import secrets; print(secrets.token_hex(8))')"
network="$fixture"
postgres="$fixture-postgres"
plex="$fixture-plex"
negative="$fixture-negative"
config="$fixture-config"
negative_config="$fixture-negative-config"
password=$(python3 -c 'import secrets; print(secrets.token_hex(16))')
phase="setup"

cleanup() {
    status=$?
    trap - EXIT
    for container in "$plex" "$negative" "$postgres"; do
        if [[ $(docker inspect --format '{{index .Config.Labels "plex-pg-canary"}}' "$container" 2>/dev/null) == "$fixture" ]]; then
            docker logs "$container" > "$EVIDENCE_DIR/$container.log" 2>&1 || true
            docker inspect --format '{{json .State}}' "$container" > "$EVIDENCE_DIR/$container-state.json" || true
            mkdir -p "$EVIDENCE_DIR/$container-diagnostics"
            docker cp "$container:/config/Library/Application Support/Plex Media Server/Crash Reports" "$EVIDENCE_DIR/$container-diagnostics/" 2>/dev/null || true
            docker cp "$container:/config/Library/Application Support/Plex Media Server/Logs" "$EVIDENCE_DIR/$container-diagnostics/" 2>/dev/null || true
            docker rm -fv "$container" >/dev/null || true
        fi
    done
    for volume in "$config" "$negative_config"; do
        if [[ $(docker volume inspect --format '{{index .Labels "plex-pg-canary"}}' "$volume" 2>/dev/null) == "$fixture" ]]; then
            docker volume rm "$volume" >/dev/null || true
        fi
    done
    if [[ $(docker network inspect --format '{{index .Labels "plex-pg-canary"}}' "$network" 2>/dev/null) == "$fixture" ]]; then
        docker network rm "$network" >/dev/null || true
    fi
    python3 - "$EVIDENCE_DIR/result.json" "$phase" "$status" "$CANDIDATE_IMAGE" "$EXPECTED_PLEX_VERSION" "$VARIANT" "$EXPECTED_ARCH" "$restart_cycles" "$restart_cycles_completed" "$soak_seconds" "$soak_seconds_completed" "$soak_iterations" "$live_recovery_verified" "$decoded_fixture_verified" <<'PY'
import json
import sys
from pathlib import Path
path, phase, status, image, version, variant, arch, cycles, completed, soak, elapsed, iterations, live_recovery, decoded = sys.argv[1:]
Path(path).write_text(json.dumps({
    "phase": phase, "exit_code": int(status), "candidate": image,
    "plex_version": version, "variant": variant, "arch": arch,
    "restart_cycles_requested": int(cycles), "restart_cycles_completed": int(completed),
    "soak_seconds_requested": int(soak), "soak_seconds_completed": int(elapsed),
    "soak_iterations": int(iterations),
    "live_postgres_recovery_verified": live_recovery == '1',
    "decoded_fixture_verified": decoded == '1',
    "promotion_allowed": False,
    "missing_gate": "Full native matrix, scan/playback/watch-state/artwork and sustained outage workload",
}, indent=2) + "\n")
PY
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if [[ "$CANDIDATE_IMAGE" != sha256:* ]]; then
    docker pull --platform "linux/$EXPECTED_ARCH" "$CANDIDATE_IMAGE"
fi
docker pull --platform "linux/$EXPECTED_ARCH" "$POSTGRES_IMAGE"
candidate_id=$(docker image inspect --format '{{.Id}}' "$CANDIDATE_IMAGE")
[[ $(docker image inspect --format '{{.Architecture}}' "$candidate_id") == "$EXPECTED_ARCH" ]]
server_arch=$(docker info --format '{{.Architecture}}')
case "$server_arch" in
    x86_64|amd64) server_arch=amd64 ;;
    aarch64|arm64) server_arch=arm64 ;;
esac
[[ "$server_arch" == "$EXPECTED_ARCH" ]] || { echo "Native execution required; no emulated certification" >&2; exit 1; }
printf '%s\n' "$candidate_id" > "$EVIDENCE_DIR/candidate-image-id.txt"
docker image inspect "$candidate_id" > "$EVIDENCE_DIR/candidate-image.json"
docker run --rm --network none --entrypoint bash "$candidate_id" -ec '
    test "$VERSION" = docker
    dpkg-query -W -f="\${Version}" plexmediaserver
' > "$EVIDENCE_DIR/installed-version.txt"
[[ $(cat "$EVIDENCE_DIR/installed-version.txt") == "$EXPECTED_PLEX_VERSION" ]] || { echo "Base image is not the latest official Plex version" >&2; exit 1; }
docker run --rm --network none --entrypoint cat "$candidate_id" \
    /usr/local/lib/plex-postgresql/default-exports.txt > "$EVIDENCE_DIR/default-exports.txt"
[[ -s "$EVIDENCE_DIR/default-exports.txt" ]]
awk '$3 ~ /^(vfork|__cxa_throw)(@.*)?$/ || $3 ~ /create_simple_converter/ { rejected=1 } END { exit rejected }' "$EVIDENCE_DIR/default-exports.txt"

docker network create --internal --label "plex-pg-canary=$fixture" "$network" >/dev/null
docker volume create --label "plex-pg-canary=$fixture" "$config" >/dev/null
docker volume create --label "plex-pg-canary=$fixture" "$negative_config" >/dev/null
docker run -d --name "$postgres" --label "plex-pg-canary=$fixture" --network "$network" --network-alias postgres \
    -e POSTGRES_USER=plex -e "POSTGRES_PASSWORD=$password" -e POSTGRES_DB=plex \
    "$POSTGRES_IMAGE" -c log_statement=all >/dev/null
postgres_ready=false
for ((attempt=1; attempt<=60; attempt++)); do
    if docker exec "$postgres" pg_isready -U plex -d plex >/dev/null 2>&1; then postgres_ready=true; break; fi
    sleep 2
done
[[ "$postgres_ready" == true ]] || { echo "Fixture PostgreSQL never became ready" >&2; exit 1; }

start_plex() {
    docker run -d --name "$1" --label "plex-pg-canary=$fixture" --network "$network" \
        -v "$2:/config" \
        -e VERSION=docker -e PLEX_PG_HOST=postgres -e PLEX_PG_PORT=5432 \
        -e PLEX_PG_DATABASE=plex -e PLEX_PG_USER=plex -e "PLEX_PG_PASSWORD=$password" \
        -e PLEX_PG_SCHEMA=plex -e PLEX_PG_LOG_LEVEL=DEBUG -e MIGRATION_INTERACTIVE=0 \
        "$candidate_id" >/dev/null
}

read_identity() {
    docker exec "$1" python3 -c 'import urllib.request; print(urllib.request.urlopen("http://127.0.0.1:32400/identity", timeout=3).read().decode())' 2>/dev/null
}

assert_no_crash_reports() {
    docker exec "$1" python3 -c '
from pathlib import Path
root = Path("/config/Library/Application Support/Plex Media Server/Crash Reports")
reports = sorted(str(path) for path in root.rglob("*.dmp"))
if reports:
    raise SystemExit("Native Plex crash reports detected: " + ", ".join(reports))
'
}

assert_ready() {
    local container="$1" stage="$2" ready=false
    for ((attempt=1; attempt<=90; attempt++)); do
        assert_no_crash_reports "$container" || return 1
        if read_identity "$container" > "$EVIDENCE_DIR/$stage-identity.xml"; then ready=true; break; fi
        [[ $(docker inspect --format '{{.State.Running}}' "$container") == true ]] || break
        sleep 2
    done
    [[ "$ready" == true ]] || { echo "$stage: actual Plex HTTP readiness failed" >&2; return 1; }
    assert_no_crash_reports "$container" || return 1
    python3 - "$EVIDENCE_DIR/$stage-identity.xml" "$EXPECTED_PLEX_VERSION" <<'PY'
import sys
import xml.etree.ElementTree as ET
root = ET.parse(sys.argv[1]).getroot()
if root.tag != "MediaContainer" or root.get("version") != sys.argv[2]:
    raise SystemExit("Actual Plex version does not match official Linux version")
PY
    local process_user
    process_user=$(docker exec "$container" python3 -c '
from pathlib import Path
for process in Path("/proc").iterdir():
    if not process.name.isdigit():
        continue
    try:
        executable = (process / "cmdline").read_bytes().split(b"\0", 1)[0]
        if executable.endswith(b"/Plex Media Server"):
            for line in (process / "status").read_text().splitlines():
                if line.startswith("Uid:"):
                    print(line.split()[1])
                    raise SystemExit(0)
    except (FileNotFoundError, PermissionError, ProcessLookupError):
        continue
raise SystemExit("No actual PMS command line")
')
    [[ "$process_user" =~ ^[0-9]+$ ]]
    docker exec -u "$process_user" "$container" python3 -c '
import os
from pathlib import Path
processes = []
for process in Path("/proc").iterdir():
    if not process.name.isdigit():
        continue
    try:
        executable = os.readlink(process / "exe")
        if executable.endswith("/Plex Media Server"):
            processes.append(process)
    except (FileNotFoundError, PermissionError, ProcessLookupError):
        continue
if not processes:
    raise SystemExit("No actual PMS process")
for process in processes:
    if "db_interpose_pg.so" not in (process / "maps").read_text():
        raise SystemExit("PMS process lacks the interpose shim")
print("Actual PMS processes load db_interpose_pg.so:", [p.name for p in processes])
' > "$EVIDENCE_DIR/$stage-shim.txt"
    docker exec "$postgres" psql -X -U plex -d plex -v ON_ERROR_STOP=1 -Atc \
        "SELECT count(*) FROM pg_stat_activity WHERE datname='plex' AND client_addr IS NOT NULL;" \
        > "$EVIDENCE_DIR/$stage-pg-sessions.txt"
    [[ $(cat "$EVIDENCE_DIR/$stage-pg-sessions.txt") =~ ^[1-9][0-9]*$ ]] || { echo "No candidate PostgreSQL sessions" >&2; return 1; }
}

phase="first-start"
start_plex "$plex" "$config"
assert_ready "$plex" first-start
phase="api-write-routing"
docker exec "$plex" mkdir -p /config/runtime-fixture-media
docker exec -i "$plex" python3 - "$fixture" <<'PY' > "$EVIDENCE_DIR/api-created-section.xml"
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
for attempt in range(90):
    try:
        with urllib.request.urlopen("http://127.0.0.1:32400/system/agents?mediaType=1", timeout=3) as response:
            agents = ET.fromstring(response.read())
        if any(agent.get("identifier") == "tv.plex.agents.none" for agent in agents.iter("Agent")):
            break
    except (urllib.error.URLError, TimeoutError):
        time.sleep(2)
else:
    raise SystemExit("Plex personal-media agent never became ready")
params = urllib.parse.urlencode({
    "name": sys.argv[1], "type": "movie", "agent": "tv.plex.agents.none",
    "scanner": "Plex Movie", "language": "en-US",
    "location": "/config/runtime-fixture-media",
})
request = urllib.request.Request("http://127.0.0.1:32400/library/sections?" + params, method="POST")
for attempt in range(90):
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            print(response.read().decode())
        break
    except urllib.error.HTTPError as error:
        body = error.read().decode(errors="replace")
        starting_up = error.code == 400 and body.strip() == "the server is still starting up. Please retry later"
        if error.code != 503 and not starting_up:
            print(body, file=sys.stderr)
            raise
        time.sleep(2)
else:
    raise SystemExit("Plex library mutation endpoint never became ready")
PY
assert_section_routing() {
    local stage="$1"
    docker exec "$postgres" psql -X -U plex -d plex -v ON_ERROR_STOP=1 -Atc \
        "SELECT count(*) FROM plex.library_sections WHERE name='$fixture';" \
        > "$EVIDENCE_DIR/$stage-persisted-section.txt"
    [[ $(cat "$EVIDENCE_DIR/$stage-persisted-section.txt") == 1 ]] || { echo "API mutation missing or duplicated in PostgreSQL" >&2; return 1; }
    docker exec "$plex" sqlite3 \
        'file:/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db?mode=ro' \
        "SELECT count(*) FROM library_sections WHERE name='$fixture';" \
        > "$EVIDENCE_DIR/$stage-shadow-section.txt"
    [[ $(cat "$EVIDENCE_DIR/$stage-shadow-section.txt") == 0 ]] || { echo "API mutation leaked to SQLite-only storage" >&2; return 1; }
    docker exec -i "$plex" python3 - "$fixture" <<'PY' > "$EVIDENCE_DIR/$stage-api-sections.xml"
import sys
import urllib.request
import xml.etree.ElementTree as ET
with urllib.request.urlopen("http://127.0.0.1:32400/library/sections", timeout=10) as response:
    body = response.read()
sections = [section for section in ET.fromstring(body).iter("Directory") if section.get("title") == sys.argv[1]]
if len(sections) != 1:
    raise SystemExit("Actual Plex API cannot read the PostgreSQL-persisted library section")
print(body.decode())
PY
}
assert_section_routing first-start
phase="native-media-scan"
section_id=$(docker exec "$postgres" psql -X -U plex -d plex -v ON_ERROR_STOP=1 -Atc \
    "SELECT id FROM plex.library_sections WHERE name='$fixture';")
[[ "$section_id" =~ ^[1-9][0-9]*$ ]]
media_file='/config/runtime-fixture-media/Plex Fixture (2000).avi'
docker exec "$plex" python3 -c 'from pathlib import Path; Path("/tmp/runtime-fixture.rgb").write_bytes(bytes([0, 0, 255]) * 320 * 240 * 20)'
docker exec "$plex" env -u LD_PRELOAD LD_LIBRARY_PATH=/usr/lib/plexmediaserver/lib \
    '/usr/lib/plexmediaserver/Plex Transcoder' -hide_banner -loglevel error \
    -f rawvideo -pixel_format rgb24 -video_size 320x240 -framerate 10 \
    -i /tmp/runtime-fixture.rgb -c:v rawvideo -pix_fmt bgr24 -threads 1 "$media_file" \
    > "$EVIDENCE_DIR/fixture-media-generation.log" 2>&1
docker exec "$plex" test -s "$media_file"
docker exec "$plex" python3 -c 'import sys, urllib.request; urllib.request.urlopen("http://127.0.0.1:32400/library/sections/" + sys.argv[1] + "/refresh", timeout=10).close()' "$section_id"
assert_scanned_media() {
    local stage="$1" media_identity
    assert_no_crash_reports "$plex"
    docker exec -i "$plex" python3 - "$section_id" "$media_file" <<'PY' > "$EVIDENCE_DIR/$stage-scanned-media.xml"
import hashlib
import os
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET
from pathlib import Path
url = "http://127.0.0.1:32400/library/sections/" + sys.argv[1] + "/all"
for attempt in range(90):
    try:
        with urllib.request.urlopen(url, timeout=3) as response:
            body = response.read()
        videos = [video for video in ET.fromstring(body).iter("Video")
                  if any(part.get("file") == sys.argv[2] for part in video.iter("Part"))]
        if len(videos) > 1:
            raise SystemExit("Scanner duplicated fixture metadata")
        if len(videos) == 1 and any(int(media.get("width", "0")) == 320
                                  and int(media.get("height", "0")) == 240
                                  and int(media.get("duration", "0")) == 2000
                                  and any(part.get("file") == sys.argv[2] for part in media.iter("Part"))
                                  for media in videos[0].iter("Media")):
            video = videos[0]
            parts = [part for part in video.iter("Part") if part.get("file") == sys.argv[2]]
            if len(parts) != 1 or not parts[0].get("key", "").startswith("/library/parts/"):
                raise SystemExit("Scanned media lacks a unique native file route")
            expected = Path(sys.argv[2]).read_bytes()
            with urllib.request.urlopen("http://127.0.0.1:32400" + parts[0].get("key"), timeout=10) as response:
                actual = response.read(len(expected) + 1)
            if hashlib.sha256(actual).digest() != hashlib.sha256(expected).digest():
                raise SystemExit("Plex media-file route returned different or truncated bytes")
            with tempfile.TemporaryDirectory(prefix="plex-decode-fixture-") as directory:
                source = Path(directory) / "downloaded.avi"
                source.write_bytes(actual)
                environment = {key: value for key, value in os.environ.items()
                               if not key.startswith(("PLEX_PG_", "PG"))
                               and key not in ("LD_PRELOAD", "DYLD_INSERT_LIBRARIES")}
                environment["LD_LIBRARY_PATH"] = "/usr/lib/plexmediaserver/lib"
                decoded = subprocess.run([
                    "/usr/lib/plexmediaserver/Plex Transcoder", "-hide_banner", "-loglevel", "error",
                    "-i", str(source), "-map", "0:v:0", "-pix_fmt", "rgb24",
                    "-c:v", "rawvideo", "-threads", "1", "-f", "rawvideo", "-",
                ], env=environment, capture_output=True, timeout=30)
                expected_frames = bytes([0, 0, 255]) * 320 * 240 * 20
                if decoded.returncode != 0 or decoded.stdout != expected_frames:
                    print(decoded.stderr.decode(errors="replace"), file=sys.stderr)
                    raise SystemExit("Native decoder did not reproduce all 20 fixture frames")
            for attribute in ("thumb", "art"):
                image_path = video.get(attribute, "")
                if not image_path.startswith("/library/metadata/"):
                    raise SystemExit("Scanned media lacks native " + attribute + " route")
                with urllib.request.urlopen("http://127.0.0.1:32400" + image_path, timeout=10) as response:
                    image_type = response.headers.get_content_type()
                    payload = response.read(1024 * 1024 + 1)
                image_signature = payload.startswith((b"\x89PNG\r\n\x1a\n", b"\xff\xd8")) or (payload.startswith(b"RIFF") and payload[8:12] == b"WEBP")
                if not image_type.startswith("image/") or not image_signature or len(payload) > 1024 * 1024:
                    raise SystemExit("Plex returned invalid or oversized " + attribute + " image")
            print("PASS native media-file bytes, decoded frames and thumbnail/art routes", file=sys.stderr)
            print(body.decode())
            break
    except (urllib.error.URLError, TimeoutError):
        pass
    time.sleep(2)
else:
    raise SystemExit("Actual scanner never exposed analyzed fixture media through Plex API")
PY
    assert_no_crash_reports "$plex"
    docker exec "$postgres" psql -X -U plex -d plex -v ON_ERROR_STOP=1 -Atc \
        "SELECT count(*) FROM plex.metadata_items metadata JOIN plex.media_items media ON media.metadata_item_id=metadata.id JOIN plex.media_parts part ON part.media_item_id=media.id WHERE metadata.library_section_id=$section_id AND part.file='$media_file';" \
        > "$EVIDENCE_DIR/$stage-persisted-media.txt"
    [[ $(cat "$EVIDENCE_DIR/$stage-persisted-media.txt") == 1 ]] || { echo "Scanned media missing or duplicated in PostgreSQL" >&2; return 1; }
    media_identity=$(docker exec "$postgres" psql -X -U plex -d plex -v ON_ERROR_STOP=1 -Atc \
        "SELECT metadata.id::text || ':' || media.id::text || ':' || part.id::text FROM plex.metadata_items metadata JOIN plex.media_items media ON media.metadata_item_id=metadata.id JOIN plex.media_parts part ON part.media_item_id=media.id WHERE metadata.library_section_id=$section_id AND part.file='$media_file';")
    [[ "$media_identity" =~ ^[1-9][0-9]*:[1-9][0-9]*:[1-9][0-9]*$ ]]
    printf '%s\n' "$media_identity" > "$EVIDENCE_DIR/$stage-media-identity.txt"
    if [[ "$stage" != first-start ]]; then
        [[ "$media_identity" == $(cat "$EVIDENCE_DIR/first-start-media-identity.txt") ]] || { echo "Restart recreated media rows instead of preserving their identities" >&2; return 1; }
    fi
    docker exec "$plex" sqlite3 \
        'file:/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db?mode=ro' \
        "SELECT count(*) FROM media_parts WHERE file='$media_file';" \
        > "$EVIDENCE_DIR/$stage-shadow-media.txt"
    [[ $(cat "$EVIDENCE_DIR/$stage-shadow-media.txt") == 0 ]] || { echo "Scanned media leaked to SQLite-only storage" >&2; return 1; }
    decoded_fixture_verified=1
}
assert_scanned_media first-start
phase="watch-state"
metadata_id=$(docker exec "$postgres" psql -X -U plex -d plex -v ON_ERROR_STOP=1 -Atc \
    "SELECT metadata.id FROM plex.metadata_items metadata JOIN plex.media_items media ON media.metadata_item_id=metadata.id JOIN plex.media_parts part ON part.media_item_id=media.id WHERE metadata.library_section_id=$section_id AND part.file='$media_file';")
[[ "$metadata_id" =~ ^[1-9][0-9]*$ ]]
media_guid=$(docker exec "$postgres" psql -X -U plex -d plex -v ON_ERROR_STOP=1 -Atc \
    "SELECT guid FROM plex.metadata_items WHERE id=$metadata_id;")
[[ "$media_guid" =~ ^tv\.plex\.agents\.none://[0-9]+$ ]]
set_watch_state() {
    docker exec "$plex" python3 -c 'import sys, urllib.parse, urllib.request; params=urllib.parse.urlencode({"key": sys.argv[1], "identifier": "com.plexapp.plugins.library"}); urllib.request.urlopen("http://127.0.0.1:32400/:/" + sys.argv[2] + "?" + params, timeout=10).close()' "$metadata_id" "$1"
}
assert_watch_state() {
    local stage="$1" expected="$2" persisted
    docker exec -i "$plex" python3 - "$metadata_id" "$expected" <<'PY' > "$EVIDENCE_DIR/$stage-watch-state.xml"
import sys
import urllib.request
import xml.etree.ElementTree as ET
with urllib.request.urlopen("http://127.0.0.1:32400/library/metadata/" + sys.argv[1], timeout=10) as response:
    body = response.read()
videos = list(ET.fromstring(body).iter("Video"))
if len(videos) != 1 or videos[0].get("ratingKey") != sys.argv[1]:
    raise SystemExit("Watch-state response did not identify the fixture media")
if (int(videos[0].get("viewCount", "0")) > 0) != (sys.argv[2] == "1"):
    raise SystemExit("Actual Plex API returned unexpected watch state")
print(body.decode())
PY
    docker exec "$postgres" psql -X -U plex -d plex -v ON_ERROR_STOP=1 -Atc \
        "SELECT count(*) FROM plex.metadata_item_settings WHERE guid='$media_guid' AND view_count>0;" \
        > "$EVIDENCE_DIR/$stage-persisted-watch-state.txt"
    persisted=$(cat "$EVIDENCE_DIR/$stage-persisted-watch-state.txt")
    [[ "$persisted" == "$expected" ]] || { echo "Watch state missing or duplicated in PostgreSQL" >&2; return 1; }
    docker exec "$plex" sqlite3 \
        'file:/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db?mode=ro' \
        "SELECT count(*) FROM metadata_item_settings WHERE guid='$media_guid' AND view_count>0;" \
        > "$EVIDENCE_DIR/$stage-shadow-watch-state.txt"
    [[ $(cat "$EVIDENCE_DIR/$stage-shadow-watch-state.txt") == 0 ]] || { echo "Watch state leaked to SQLite-only storage" >&2; return 1; }
    assert_no_crash_reports "$plex"
}
set_watch_state scrobble
assert_watch_state watched 1
phase="restart"
for ((cycle=1; cycle<=restart_cycles; cycle++)); do
    stage=restart
    if ((cycle > 1)); then stage="restart-$cycle"; fi
    phase="$stage"
    docker restart "$plex" >/dev/null
    assert_ready "$plex" "$stage"
    assert_section_routing "$stage"
    assert_scanned_media "$stage"
    assert_watch_state "$stage" 1
    restart_cycles_completed=$cycle
    echo "PASS native restart $cycle/$restart_cycles"
done
if ((soak_seconds > 0)); then
    phase="native-workload-soak"
    soak_pms_identity=$(cat "$EVIDENCE_DIR/$stage-shim.txt")
    soak_started=$(python3 -c 'import time; print(int(time.monotonic()))')
    while ((soak_seconds_completed < soak_seconds)); do
        iteration=$((soak_iterations + 1))
        stage="soak-$iteration"
        docker exec "$plex" python3 -c 'import sys, urllib.request; urllib.request.urlopen("http://127.0.0.1:32400/library/sections/" + sys.argv[1] + "/refresh", timeout=10).close()' "$section_id"
        readers=()
        for ((client=1; client<=4; client++)); do
            (trap - EXIT INT TERM; assert_scanned_media "$stage-client-$client") &
            readers+=("$!")
        done
        reader_failure=0
        for reader in "${readers[@]}"; do
            if ! wait "$reader"; then reader_failure=1; fi
        done
        ((reader_failure == 0)) || { echo "Concurrent soak reader failed" >&2; exit 1; }
        set_watch_state unscrobble
        assert_watch_state "$stage-unwatched" 0
        set_watch_state scrobble
        assert_watch_state "$stage-watched" 1
        assert_ready "$plex" "$stage"
        [[ $(cat "$EVIDENCE_DIR/$stage-shim.txt") == "$soak_pms_identity" ]] || { echo "PMS process restarted during continuous soak" >&2; exit 1; }
        docker stats --no-stream --format '{{json .}}' "$plex" "$postgres" >> "$EVIDENCE_DIR/soak-resource-samples.jsonl"
        soak_iterations=$iteration
        soak_now=$(python3 -c 'import time; print(int(time.monotonic()))')
        soak_seconds_completed=$((soak_now - soak_started))
        remaining=$((soak_seconds - soak_seconds_completed))
        if ((remaining > 0)); then
            pause=$remaining
            if ((pause > 15)); then pause=15; fi
            sleep "$pause"
            soak_now=$(python3 -c 'import time; print(int(time.monotonic()))')
            soak_seconds_completed=$((soak_now - soak_started))
        fi
        echo "PASS native soak iteration $soak_iterations ($soak_seconds_completed/$soak_seconds seconds)"
    done
fi
phase="watch-state-reset"
set_watch_state unscrobble
assert_watch_state unwatched 0
phase="live-postgres-interruption"
live_pms_identity=$(cat "$EVIDENCE_DIR/$stage-shim.txt")
docker stop "$postgres" >/dev/null
sleep 5
docker exec "$plex" python3 -c '
import urllib.error
import urllib.request
try:
    with urllib.request.urlopen("http://127.0.0.1:32400/library/sections", timeout=3) as response:
        print("Read during outage returned HTTP", response.status)
except (urllib.error.URLError, TimeoutError) as error:
    print("Read during outage failed:", error)
' > "$EVIDENCE_DIR/live-outage-read.txt"
assert_no_crash_reports "$plex"
docker start "$postgres" >/dev/null
postgres_ready=false
for ((attempt=1; attempt<=60; attempt++)); do
    if docker exec "$postgres" pg_isready -U plex -d plex >/dev/null 2>&1; then postgres_ready=true; break; fi
    sleep 2
done
[[ "$postgres_ready" == true ]]
phase="live-postgres-recovery"
assert_ready "$plex" live-recovery
assert_section_routing live-recovery
assert_scanned_media live-recovery
set_watch_state scrobble
assert_watch_state live-recovery 1
[[ $(cat "$EVIDENCE_DIR/live-recovery-shim.txt") == "$live_pms_identity" ]] || { echo "PMS restarted instead of recovering PostgreSQL sessions" >&2; exit 1; }
set_watch_state unscrobble
assert_watch_state live-recovery-reset 0
live_recovery_verified=1
echo "PASS native PostgreSQL recovery without PMS restart"
phase="postgres-unavailable"
docker stop "$plex" "$postgres" >/dev/null
start_plex "$negative" "$negative_config"
for ((attempt=1; attempt<=40; attempt++)); do
    if read_identity "$negative" >/dev/null; then
        echo "FAIL: fresh Plex served requests while PostgreSQL was unavailable" >&2
        exit 1
    fi
    sleep 2
done
phase="postgres-recovery"
docker start "$postgres" >/dev/null
postgres_ready=false
for ((attempt=1; attempt<=60; attempt++)); do
    if docker exec "$postgres" pg_isready -U plex -d plex >/dev/null 2>&1; then postgres_ready=true; break; fi
    sleep 2
done
[[ "$postgres_ready" == true ]]
docker restart "$negative" >/dev/null
assert_ready "$negative" recovery
assert_no_crash_reports "$negative"
phase="workload-smoke-complete-certification-incomplete"
echo "API routing/persistence and container smoke completed. PROMOTION BLOCKED: full native workload certification is incomplete." >&2
exit 1
