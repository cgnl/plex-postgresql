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
real_media_verified=0
real_media_full_decode_verified=0
native_roundtrip_verified=0
concurrent_real_media_verified=0
real_media_sample_seconds=0
if ((soak_seconds > 60)); then real_media_sample_seconds=20; fi
script_dir=$(cd "$(dirname "$0")" && pwd)
media_cache=${PLEX_E2E_MEDIA_CACHE_DIR:-${TMPDIR:-/tmp}/plex-e2e-media-cache}
mkdir -p "$EVIDENCE_DIR"
for tool in docker python3; do command -v "$tool" >/dev/null; done
[[ "$CANDIDATE_IMAGE" =~ (^|@)sha256:[0-9a-f]{64}$ ]] || { echo "Candidate must be immutable" >&2; exit 1; }
[[ "$POSTGRES_IMAGE" =~ @sha256:[0-9a-f]{64}$ ]] || { echo "PostgreSQL must be digest-pinned" >&2; exit 1; }
[[ "$EXPECTED_ARCH" == amd64 || "$EXPECTED_ARCH" == arm64 ]]
[[ "$VARIANT" == linuxserver || "$VARIANT" == plexinc ]]
fixture="plex-canary-$(python3 -c 'import secrets; print(secrets.token_hex(8))')"
network="$fixture"
download_network="$fixture-downloads"
postgres="$fixture-postgres"
plex="$fixture-plex"
negative="$fixture-negative"
config="$fixture-config"
negative_config="$fixture-negative-config"
password=$(python3 -c 'import secrets; print(secrets.token_hex(16))')
phase="setup"

# Reuse the same bounded, environment-free collector in shell checkpoints and
# the concurrent coordinator. This file is evidence, not a product component.
cat > "$EVIDENCE_DIR/pms-lifecycle-probe.py" <<'PY'
import json
import os
from pathlib import Path
import stat
import sys
import time


def process_info(directory):
    raw = (directory / "stat").read_text()
    fields = raw.rsplit(")", 1)[1].split()
    result = {"pid": int(directory.name), "state": fields[0],
              "parent_pid": int(fields[1]), "start_ticks": int(fields[19])}
    allowed = {"State", "PPid", "VmRSS", "VmHWM", "VmPeak", "Threads", "CoreDumping"}
    result["status"] = {key: value.strip() for line in (directory / "status").read_text().splitlines()
                        if ":" in line for key, value in [line.split(":", 1)] if key in allowed}
    try:
        result["core_limits"] = [line for line in (directory / "limits").read_text().splitlines()
                                 if line.startswith(("Max core file size", "Max file size"))]
        result["coredump_filter"] = (directory / "coredump_filter").read_text()[:64].strip()
    except OSError:
        result["core_limits_available"] = False
    return result


def memory_info():
    roots = [Path("/sys/fs/cgroup"), Path("/sys/fs/cgroup/memory")]
    names = ("memory.events", "memory.events.local", "memory.current", "memory.peak", "memory.max",
             "memory.oom_control", "memory.failcnt", "memory.usage_in_bytes",
             "memory.max_usage_in_bytes", "memory.limit_in_bytes")
    files = {}
    for root in roots:
        for name in names:
            path = root / name
            try:
                if path.is_file():
                    with path.open() as source:
                        files[name] = source.read(4096).strip()
            except OSError:
                pass
    counters = {}
    for name in ("memory.events", "memory.oom_control"):
        for line in files.get(name, "").splitlines():
            parts = line.split()
            if len(parts) == 2 and parts[1].isdigit():
                counters[name + "." + parts[0]] = int(parts[1])
    if files.get("memory.failcnt", "").isdigit():
        counters["memory.failcnt"] = int(files["memory.failcnt"])
    return {"files": files, "counters": counters}


def core_info():
    result = {}
    for key, filename in (("core_pattern", "/proc/sys/kernel/core_pattern"),
                          ("core_uses_pid", "/proc/sys/kernel/core_uses_pid"),
                          ("suid_dumpable", "/proc/sys/fs/suid_dumpable")):
        try:
            with Path(filename).open() as source:
                result[key] = source.read(4096).strip()
        except OSError:
            result[key + "_available"] = False
    result["core_pattern_piped"] = result.get("core_pattern", "").startswith("|")
    return result


def dump_index(root=Path("/run/plex-temp")):
    result = {"root": str(root), "selected": [], "skipped": [], "scan_truncated": False,
              "max_files": 8, "max_file_bytes": 64 * 1024 * 1024,
              "max_total_bytes": 128 * 1024 * 1024, "selected_bytes": 0}
    if not root.is_dir() or root.is_symlink():
        result["available"] = False
        return result
    result["available"] = True
    deadline = time.monotonic() + 2
    for index, path in enumerate(root.rglob("*")):
        if index >= 256 or time.monotonic() > deadline:
            result["scan_truncated"] = True
            break
        if not (path.name.lower().endswith(".dmp") or path.name == "core" or path.name.startswith("core.")):
            continue
        reason = None
        try:
            if path.is_symlink() or not path.is_file():
                reason = "not_regular_file"
                size = 0
            else:
                size = path.stat().st_size
                if size > result["max_file_bytes"]:
                    reason = "file_size_limit"
                elif len(result["selected"]) >= result["max_files"]:
                    reason = "file_count_limit"
                elif result["selected_bytes"] + size > result["max_total_bytes"]:
                    reason = "total_size_limit"
        except OSError:
            reason, size = "unreadable", 0
        entry = {"path": str(path), "bytes": size}
        if reason:
            entry["reason"] = reason
            if len(result["skipped"]) < 16:
                result["skipped"].append(entry)
            else:
                result["skipped_records_truncated"] = True
        else:
            result["selected"].append(entry)
            result["selected_bytes"] += size
    return result


def read_dump(path, expected_size):
    path = Path(path)
    relative = path.relative_to("/run/plex-temp")
    if ".." in relative.parts or not (path.name.lower().endswith(".dmp") or path.name == "core" or path.name.startswith("core.")):
        raise ValueError("Unexpected transient dump path")
    limit = 64 * 1024 * 1024
    if not 0 <= expected_size <= limit:
        raise ValueError("Invalid transient dump size")
    directory = descriptor = None
    try:
        directory = os.open("/run/plex-temp", os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        for part in relative.parts[:-1]:
            following = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=directory)
            os.close(directory)
            directory = following
        descriptor = os.open(relative.name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode) or before.st_size != expected_size or before.st_size > limit:
            raise ValueError("Transient dump changed or exceeded size limit")
        with os.fdopen(descriptor, "rb") as source:
            descriptor = None
            data = source.read(limit + 1)
            after = os.fstat(source.fileno())
        if len(data) != expected_size or len(data) > limit or (before.st_size, before.st_mtime_ns) != (after.st_size, after.st_mtime_ns):
            raise ValueError("Transient dump changed during bounded read")
        return data
    finally:
        if descriptor is not None:
            os.close(descriptor)
        if directory is not None:
            os.close(directory)


def snapshot():
    result = {"time": time.monotonic(), "pms": [], "parents": [],
              "scan_complete": True, "memory": memory_info(), "core": core_info()}
    deadline = time.monotonic() + 2
    try:
        entries = Path("/proc").iterdir()
        for directory in entries:
            if time.monotonic() > deadline:
                result["scan_complete"] = False
                break
            if not directory.name.isdigit():
                continue
            try:
                try:
                    executable = os.readlink(directory / "exe")
                except OSError:
                    # Inspect only argv[0] to identify PMS, never return args.
                    with (directory / "cmdline").open("rb") as source:
                        executable = source.read(4096).split(b"\0", 1)[0].decode(errors="replace")
                if executable.endswith("/Plex Media Server"):
                    process = process_info(directory)
                    result["pms"].append(process)
                    try:
                        parent = process_info(Path("/proc") / str(process["parent_pid"]))
                        if parent not in result["parents"]:
                            result["parents"].append(parent)
                    except (OSError, ValueError, IndexError):
                        pass
            except (OSError, ValueError, IndexError):
                continue
    except OSError:
        result["scan_complete"] = False
    return result


def compare(before, after):
    old = {(process["pid"], process["start_ticks"]) for process in before["pms"]}
    new = {(process["pid"], process["start_ticks"]) for process in after["pms"]}
    counters = after["memory"]["counters"]
    previous = before["memory"]["counters"]
    delta = {key: max(0, value - previous.get(key, value)) for key, value in counters.items()}
    complete = before["scan_complete"] and after["scan_complete"]
    non_live = [(process["pid"], process["start_ticks"], process["state"])
                for process in after["pms"] if process.get("state") in ("Z", "X", "x")]
    return {"observation_complete": complete, "same_pms_processes": complete and bool(old) and old == new and not non_live,
            "lost_pms": sorted(old - new) if complete else [],
            "new_pms": sorted(new - old) if complete else [],
            "pms_absent": complete and bool(old) and not new,
            "non_live_pms": non_live,
            "oom_counter_deltas": delta}


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] == "--read-dump":
        sys.stdout.buffer.write(read_dump(sys.argv[2], int(sys.argv[3])))
    else:
        print(json.dumps(dump_index() if sys.argv[1:] == ["--dump-index"] else snapshot()))
PY

capture_pms_lifecycle() {
    local stage="$1"
    [[ $(docker inspect --format '{{index .Config.Labels "plex-pg-canary"}}' "$plex" 2>/dev/null) == "$fixture" ]] || return 0
    python3 - "$plex" "$EVIDENCE_DIR/$stage-pms-lifecycle.json" <<'PY'
import json
from pathlib import Path
import subprocess
import sys
try:
    probe = subprocess.run(["docker", "exec", sys.argv[1], "python3", "/tmp/plex-pms-lifecycle.py"],
                           capture_output=True, text=True, timeout=10)
    result = json.loads(probe.stdout) if probe.returncode == 0 else {"available": False, "exit_code": probe.returncode}
    # Host PIDs correlate container namespace snapshots with kernel OOM logs.
    # comm contains executable names only; arguments and environment are absent.
    top = subprocess.run(["docker", "top", sys.argv[1], "-eo", "pid,ppid,stat,rss,comm"],
                         capture_output=True, text=True, timeout=5)
    result["host_processes"] = top.stdout[-8192:] if top.returncode == 0 else "unavailable"
except (OSError, ValueError, subprocess.TimeoutExpired) as error:
    result = {"available": False, "error_type": type(error).__name__}
Path(sys.argv[2]).write_text(json.dumps(result, indent=2) + "\n")
PY
}

capture_host_failure() {
    python3 - "$EVIDENCE_DIR" <<'PY'
import json
from pathlib import Path
import re
import runpy
import shutil
import subprocess
import sys
root = Path(sys.argv[1])
probe = runpy.run_path(str(root / "pms-lifecycle-probe.py"))
result = {"memory": probe["memory_info"](), "core": probe["core_info"](), "kernel": {"available": False}}
try:
    allowed = {"MemTotal", "MemAvailable", "SwapTotal", "SwapFree"}
    result["host_memory"] = {key: value.strip() for line in Path("/proc/meminfo").read_text().splitlines()
                             if ":" in line for key, value in [line.split(":", 1)] if key in allowed}
except OSError:
    pass
if shutil.which("dmesg"):
    try:
        output = subprocess.run(["dmesg"], capture_output=True, text=True, timeout=5)
        privileged_read = False
        denied = re.search(r"permission denied|not permitted|access denied", output.stderr, re.I)
        if output.returncode != 0 and denied and shutil.which("sudo"):
            output = subprocess.run(["sudo", "-n", "dmesg"], capture_output=True, text=True, timeout=5)
            privileged_read = True
        lines = [line for line in output.stdout.splitlines()
                 if re.search(r"out of memory|oom.kill|killed process|memory cgroup|segfault|general protection fault|plex media", line, re.I)]
        result["kernel"] = {"available": output.returncode == 0, "exit_code": output.returncode,
                            "noninteractive_privileged_read": privileged_read,
                            "filtered_tail": "\n".join(lines[-100:])[-32768:]}
    except (OSError, subprocess.TimeoutExpired) as error:
        result["kernel"]["error_type"] = type(error).__name__
(root / "host-failure-memory.json").write_text(json.dumps(result, indent=2) + "\n")
PY
}

capture_transient_dumps() {
    python3 - "$1" "$fixture" "$EVIDENCE_DIR/$1-diagnostics" <<'PY'
import json
from pathlib import Path, PurePosixPath
import subprocess
import sys
container, fixture, destination = sys.argv[1:]
root = Path(destination)
root.mkdir(parents=True, exist_ok=True)
record = {"available": False, "copied": [], "copy_failures": []}
try:
    owned = subprocess.run(["docker", "inspect", "--format", '{{index .Config.Labels "plex-pg-canary"}}', container],
                           capture_output=True, text=True, timeout=5)
    if owned.returncode != 0 or owned.stdout.strip() != fixture:
        raise RuntimeError("Container ownership was not verified")
    probe = subprocess.run(["docker", "exec", container, "python3", "/tmp/plex-pms-lifecycle.py", "--dump-index"],
                           capture_output=True, text=True, timeout=5)
    if probe.returncode == 0:
        record = json.loads(probe.stdout)
        record.update({"copied": [], "copy_failures": []})
        dump_root = root / "transient-dumps"
        transferred = 0
        for index, entry in enumerate(record.get("selected", [])[:8]):
            path = PurePosixPath(entry["path"])
            relative = path.relative_to("/run/plex-temp")
            if ".." in relative.parts or not (path.name.lower().endswith(".dmp") or path.name == "core" or path.name.startswith("core.")):
                raise RuntimeError("Unexpected transient dump path")
            dump_root.mkdir(exist_ok=True)
            copied = dump_root / (str(index + 1) + "-" + path.name)
            partial = copied.with_name(copied.name + ".partial")
            promoted = False
            try:
                if not isinstance(entry["bytes"], int) or not 0 <= entry["bytes"] <= 64 * 1024 * 1024:
                    raise ValueError("Invalid indexed dump size")
                if transferred + entry["bytes"] > 128 * 1024 * 1024:
                    raise ValueError("Transient dump total size limit")
                transfer = subprocess.run(["docker", "exec", container, "python3", "/tmp/plex-pms-lifecycle.py",
                                           "--read-dump", str(path), str(entry["bytes"])],
                                          capture_output=True, timeout=10)
                if transfer.returncode != 0 or len(transfer.stdout) != entry["bytes"] or len(transfer.stdout) > 64 * 1024 * 1024:
                    raise ValueError("Transient dump source changed or bounded transfer failed")
                if partial.is_symlink() or copied.is_symlink():
                    raise ValueError("Unexpected transient dump evidence symlink")
                partial.write_bytes(transfer.stdout)
                partial.replace(copied)
                promoted = True
                transferred += len(transfer.stdout)
                record["copied"].append({"path": str(path), "evidence": str(copied.relative_to(root)), "bytes": len(transfer.stdout)})
            except (OSError, ValueError, subprocess.TimeoutExpired) as error:
                record["copy_failures"].append({"path": str(path), "reason": type(error).__name__})
            finally:
                partial.unlink(missing_ok=True)
                if not promoted:
                    copied.unlink(missing_ok=True)
    else:
        record["probe_exit_code"] = probe.returncode
except (OSError, ValueError, RuntimeError, subprocess.TimeoutExpired) as error:
    record["collection_error_type"] = type(error).__name__
(root / "transient-dump-index.json").write_text(json.dumps(record, indent=2) + "\n")
PY
}

compare_pms_lifecycle() {
    python3 - "$EVIDENCE_DIR" "$1" "$2" <<'PY'
import json
from pathlib import Path
import runpy
import sys
root = Path(sys.argv[1])
before = json.loads((root / (sys.argv[2] + "-pms-lifecycle.json")).read_text())
after = json.loads((root / (sys.argv[3] + "-pms-lifecycle.json")).read_text())
if "pms" in before and "pms" in after:
    probe = runpy.run_path(str(root / "pms-lifecycle-probe.py"))
    result = probe["compare"](before, after)
else:
    result = {"observation_complete": False}
(root / (sys.argv[3] + "-pms-transition.json")).write_text(json.dumps(result, indent=2) + "\n")
PY
}

cleanup() {
    status=$?
    trap - EXIT
    capture_pms_lifecycle cleanup || true
    if ((status != 0)); then capture_host_failure || true; fi
    for container in "$plex" "$negative" "$fixture-source" "$fixture-imported" "$fixture-restored" "$postgres"; do
        if [[ $(docker inspect --format '{{index .Config.Labels "plex-pg-canary"}}' "$container" 2>/dev/null) == "$fixture" ]]; then
            docker logs "$container" > "$EVIDENCE_DIR/$container.log" 2>&1 || true
            docker inspect --format '{{json .State}}' "$container" > "$EVIDENCE_DIR/$container-state.json" || true
            mkdir -p "$EVIDENCE_DIR/$container-diagnostics"
            if ((status != 0)); then capture_transient_dumps "$container" || true; fi
            docker cp "$container:/config/Library/Application Support/Plex Media Server/Crash Reports" "$EVIDENCE_DIR/$container-diagnostics/" 2>/dev/null || true
            docker cp "$container:/config/Library/Application Support/Plex Media Server/Logs" "$EVIDENCE_DIR/$container-diagnostics/" 2>/dev/null || true
            docker rm -fv "$container" >/dev/null || true
        fi
    done
    for volume in "$config" "$negative_config" "$fixture-source-config" "$fixture-imported-config" "$fixture-restored-config"; do
        if [[ $(docker volume inspect --format '{{index .Labels "plex-pg-canary"}}' "$volume" 2>/dev/null) == "$fixture" ]]; then
            docker volume rm "$volume" >/dev/null || true
        fi
    done
    if [[ $(docker network inspect --format '{{index .Labels "plex-pg-canary"}}' "$network" 2>/dev/null) == "$fixture" ]]; then
        docker network rm "$network" >/dev/null || true
    fi
    if [[ $(docker network inspect --format '{{index .Labels "plex-pg-canary"}}' "$download_network" 2>/dev/null) == "$fixture" ]]; then
        docker network rm "$download_network" >/dev/null || true
    fi
    python3 - "$EVIDENCE_DIR/result.json" "$phase" "$status" "$CANDIDATE_IMAGE" "$EXPECTED_PLEX_VERSION" "$VARIANT" "$EXPECTED_ARCH" "$restart_cycles" "$restart_cycles_completed" "$soak_seconds" "$soak_seconds_completed" "$soak_iterations" "$live_recovery_verified" "$decoded_fixture_verified" "$real_media_verified" "$real_media_full_decode_verified" "$native_roundtrip_verified" "$concurrent_real_media_verified" <<'PY'
import json
import sys
from pathlib import Path
path, phase, status, image, version, variant, arch, cycles, completed, soak, elapsed, iterations, live_recovery, decoded, real_media, full_decode, roundtrip, concurrent = sys.argv[1:]
Path(path).write_text(json.dumps({
    "phase": phase, "exit_code": int(status), "candidate": image,
    "plex_version": version, "variant": variant, "arch": arch,
    "restart_cycles_requested": int(cycles), "restart_cycles_completed": int(completed),
    "soak_seconds_requested": int(soak), "soak_seconds_completed": int(elapsed),
    "soak_iterations": int(iterations),
    "live_postgres_recovery_verified": live_recovery == '1',
    "decoded_fixture_verified": decoded == '1',
    "real_movie_and_tv_playback_verified": real_media == '1',
    "real_media_full_decode_verified": full_decode == '1',
    "native_import_and_rollback_verified": roundtrip == '1',
    "four_concurrent_real_media_clients_verified": concurrent == '1',
    "workload_passed": status == '0' and phase == 'native-workload-complete',
    "promotion_allowed": False,
    "missing_gate": "Full native matrix, scan/playback/watch-state/artwork and sustained outage workload",
}, indent=2) + "\n")
PY
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

python3 "$script_dir/prepare-media-fixtures.py" "$media_cache"
cp "$media_cache/media-fixtures.json" "$EVIDENCE_DIR/media-fixtures.json"

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
        -e PLEX_PG_REAPER_DIAGNOSTICS=1 \
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
docker cp "$EVIDENCE_DIR/pms-lifecycle-probe.py" "$plex:/tmp/plex-pms-lifecycle.py"
capture_pms_lifecycle first-start
phase="media-bootstrap"
docker exec "$plex" mkdir -p /config/runtime-fixture-media
# Plex installs its own H.264/AAC codecs on first analysis. Permit outbound
# bootstrap on an owned network, then remove it before restarts and soak.
docker network create --label "plex-pg-canary=$fixture" "$download_network" >/dev/null
docker network connect "$download_network" "$plex"
docker cp "$script_dir/bootstrap-plex-codecs.py" "$plex:/tmp/bootstrap-plex-codecs.py"
docker exec "$plex" python3 /tmp/bootstrap-plex-codecs.py /tmp/codec-bootstrap.json
docker cp "$plex:/tmp/codec-bootstrap.json" "$EVIDENCE_DIR/codec-bootstrap.json"
bbb_file='/config/runtime-fixture-media/Big Buck Bunny (2008).m4v'
tv_root='/config/runtime-tv-media'
tv_season="$tv_root/The Beverly Hillbillies (1962)/Season 01"
docker exec "$plex" mkdir -p "$tv_season"
docker cp "$media_cache/BigBuckBunny_640x360.m4v" "$plex:$bbb_file"
docker cp "$media_cache/beverly-s01e01.mp4" "$plex:$tv_season/The Beverly Hillbillies - S01E01 - The Clampetts Strike Oil.mp4"
docker cp "$media_cache/beverly-s01e02.mp4" "$plex:$tv_season/The Beverly Hillbillies - S01E02 - Getting Settled.mp4"
docker cp "$script_dir/verify-bbb-playback.py" "$plex:/tmp/verify-bbb-playback.py"
media_file='/config/runtime-fixture-media/Plex Fixture (2000).avi'
docker exec "$plex" python3 -c 'from pathlib import Path; Path("/tmp/runtime-fixture.rgb").write_bytes(bytes([0, 0, 255]) * 320 * 240 * 20)'
docker exec "$plex" env -u LD_PRELOAD LD_LIBRARY_PATH=/usr/lib/plexmediaserver/lib \
    '/usr/lib/plexmediaserver/Plex Transcoder' -hide_banner -loglevel error \
    -f rawvideo -pixel_format rgb24 -video_size 320x240 -framerate 10 \
    -i /tmp/runtime-fixture.rgb -c:v rawvideo -pix_fmt bgr24 -threads 1 "$media_file" \
    > "$EVIDENCE_DIR/fixture-media-generation.log" 2>&1
docker exec "$plex" test -s "$media_file"
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
last_body = b""
last_error = "Analyzed fixture metadata not yet available"
for attempt in range(90):
    try:
        with urllib.request.urlopen(url, timeout=3) as response:
            body = response.read()
        last_body = body
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
            # The scanner commits dimensions before generated thumbnail/art
            # URLs. Poll publication as part of readiness, then certify both
            # native routes and their actual image payloads below.
            missing_images = [attribute for attribute in ("thumb", "art") if not video.get(attribute)]
            if missing_images:
                last_error = "Native " + "/".join(missing_images) + " route not yet published"
                time.sleep(2)
                continue
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
    except (urllib.error.URLError, TimeoutError) as error:
        last_error = str(error)
    time.sleep(2)
else:
    print(last_body.decode(errors="replace"))
    raise SystemExit("Actual scanner never exposed analyzed fixture media and native artwork through Plex API: " + last_error)
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
phase="real-movie-and-tv-library"
docker exec -i "$plex" python3 - "$fixture-tv" "$tv_root" <<'PY' > "$EVIDENCE_DIR/tv-created-section.xml"
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
params = urllib.parse.urlencode({"name": sys.argv[1], "type": "show", "agent": "com.plexapp.agents.none",
    "scanner": "Plex Series Scanner", "language": "xn", "location": sys.argv[2]})
request = urllib.request.Request("http://127.0.0.1:32400/library/sections?" + params, method="POST")
for attempt in range(60):
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
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
    raise SystemExit("TV library creation never became ready")
PY
tv_section_id=$(docker exec "$postgres" psql -X -U plex -d plex -v ON_ERROR_STOP=1 -Atc \
    "SELECT id FROM plex.library_sections WHERE name='$fixture-tv';")
[[ "$tv_section_id" =~ ^[1-9][0-9]*$ ]]
docker exec "$plex" python3 -c 'import sys, urllib.request; [urllib.request.urlopen("http://127.0.0.1:32400/library/sections/" + section + "/refresh?force=1", timeout=10).close() for section in sys.argv[1:]]' "$section_id" "$tv_section_id"

assert_real_media() {
    local stage="$1" sample="${2:-0}" output="/tmp/real-media-$1" status=0
    local sample_args=()
    if ((sample > 0)); then sample_args=(--sample-seconds "$sample"); fi
    docker exec "$plex" python3 /tmp/verify-bbb-playback.py "$section_id" "$bbb_file" "$output/movie" "${sample_args[@]}" || status=1
    docker exec "$plex" python3 /tmp/verify-bbb-playback.py "$tv_section_id" "$tv_season/The Beverly Hillbillies - S01E01 - The Clampetts Strike Oil.mp4" "$output/s01e01" --kind episode --season 1 --episode 1 "${sample_args[@]}" || status=1
    docker exec "$plex" python3 /tmp/verify-bbb-playback.py "$tv_section_id" "$tv_season/The Beverly Hillbillies - S01E02 - Getting Settled.mp4" "$output/s01e02" --kind episode --season 1 --episode 2 "${sample_args[@]}" || status=1
    mkdir -p "$EVIDENCE_DIR/$stage-real-media"
    docker cp "$plex:$output/." "$EVIDENCE_DIR/$stage-real-media/" || status=1
    ((status == 0)) || return 1
    docker exec "$postgres" psql -X -U plex -d plex -v ON_ERROR_STOP=1 -Atc \
        "SELECT count(*) FROM plex.metadata_items episode JOIN plex.metadata_items season ON season.id=episode.parent_id JOIN plex.metadata_items show ON show.id=season.parent_id WHERE episode.library_section_id=$tv_section_id AND episode.metadata_type=4 AND season.metadata_type=3 AND season.index=1 AND show.metadata_type=2 AND episode.index IN (1,2);" \
        > "$EVIDENCE_DIR/$stage-tv-hierarchy.txt"
    [[ $(cat "$EVIDENCE_DIR/$stage-tv-hierarchy.txt") == 2 ]] || { echo "TV season/episode hierarchy missing or duplicated in PostgreSQL" >&2; return 1; }
    docker exec "$plex" sqlite3 \
        'file:/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db?mode=ro' \
        "SELECT count(*) FROM media_parts WHERE file LIKE '/config/runtime-tv-media/%';" \
        > "$EVIDENCE_DIR/$stage-tv-shadow.txt"
    [[ $(cat "$EVIDENCE_DIR/$stage-tv-shadow.txt") == 0 ]] || { echo "TV metadata leaked to SQLite" >&2; return 1; }
    assert_no_crash_reports "$plex"
}
assert_concurrent_real_media() {
    local stage="$1" output="/tmp/concurrent-real-media-$1" status=0
    docker exec -i "$plex" python3 - "$section_id" "$tv_section_id" "$bbb_file" \
        "$tv_season/The Beverly Hillbillies - S01E01 - The Clampetts Strike Oil.mp4" \
        "$tv_season/The Beverly Hillbillies - S01E02 - Getting Settled.mp4" "$output" <<'PY' || status=1
import json
import os
from pathlib import Path
import signal
import runpy
import subprocess
import sys
import threading
import time
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET

movie_section, tv_section, movie, episode1, episode2, destination = sys.argv[1:]
output = Path(destination)
output.mkdir(parents=True, exist_ok=False)
gate = output / "start"
lifecycle = runpy.run_path("/tmp/plex-pms-lifecycle.py")
baseline = lifecycle["snapshot"]()
result = {"passed": False, "clients_requested": 4, "sample_seconds": 20, "decoder_input_rate": "realtime",
          "browser_playback_verified": False, "server_transcoding_verified": False,
          "workload": [], "postgres_sessions": [],
          "pms_lifecycle": {"before": baseline, "during": []}}
clients = []
logs = []
workload_errors = []
done = threading.Event()
base = "http://127.0.0.1:32400"

def request(path, params=None):
    url = base + path
    if params:
        url += "?" + urllib.parse.urlencode(params)
    with urllib.request.urlopen(url, timeout=10) as response:
        body = response.read(8 * 1024 * 1024 + 1)
    if len(body) > 8 * 1024 * 1024:
        raise RuntimeError("Oversized workload metadata response")
    return body

def mixed_workload(key):
    try:
        started = time.monotonic()
        request("/library/sections/" + tv_section + "/refresh", {"force": 1})
        result["workload"].append({"action": "tv_rescan", "start": started,
                                   "end": time.monotonic(), "passed": True})
        for iteration in range(60):
            if done.is_set():
                break
            if iteration in (5, 20, 40):
                started = time.monotonic()
                request("/library/sections/" + tv_section + "/refresh", {"force": 1})
                result["workload"].append({"action": "tv_rescan", "start": started,
                                           "end": time.monotonic(), "passed": True})
            started = time.monotonic()
            root = ET.fromstring(request("/library/metadata/" + key))
            if not any(video.get("ratingKey") == key for video in root.iter("Video")):
                raise RuntimeError("Concurrent metadata read lost real movie identity")
            action = "scrobble" if iteration % 2 == 0 else "unscrobble"
            request("/:/" + action, {"key": key, "identifier": "com.plexapp.plugins.library"})
            watched = list(ET.fromstring(request("/library/metadata/" + key)).iter("Video"))
            if len(watched) != 1 or (int(watched[0].get("viewCount", "0")) > 0) != (action == "scrobble"):
                raise RuntimeError("Concurrent real-media watch write was not visible through Plex API")
            result["workload"].append({"action": "metadata_read_and_" + action,
                                       "start": started, "end": time.monotonic(), "passed": True})
            done.wait(0.1)
    except Exception as error:
        workload_errors.append(str(error))

# Record the real native decoder process interval, excluding metadata setup
# and codec version checks. Its input is the Plex HTTP original-file route.
client_code = r'''
import json, os, runpy, subprocess, sys, time
from pathlib import Path
gate, output, *arguments = sys.argv[1:]
deadline = time.monotonic() + 15
while not Path(gate).exists():
    if time.monotonic() > deadline: raise RuntimeError("Concurrent client start gate timed out")
    time.sleep(0.01)
original_run = subprocess.run
def timed_run(command, *args, **kwargs):
    if "-progress" not in command:
        return original_run(command, *args, **kwargs)
    # Read HTTP media at playback speed to sustain a meaningful bounded load
    # rather than racing four decoders through the sample as fast as possible.
    command = list(command)
    command.insert(command.index("-i"), "-re")
    started = time.monotonic()
    try:
        return original_run(command, *args, **kwargs)
    finally:
        Path(output, "decoder-interval.json").write_text(json.dumps({"start": started, "end": time.monotonic()}))
subprocess.run = timed_run
sys.argv = ["/tmp/verify-bbb-playback.py", *arguments]
runpy.run_path(sys.argv[0], run_name="__main__")
'''
pg_environment = os.environ.copy()
for name in ("HOST", "PORT", "DATABASE", "USER", "PASSWORD"):
    pg_environment["PG" + name] = os.environ["PLEX_PG_" + name]
workload = None
try:
    listing = ET.fromstring(request("/library/sections/" + movie_section + "/all"))
    movies = [video for video in listing.iter("Video")
              if any(part.get("file") == movie for part in video.iter("Part"))]
    if len(movies) != 1:
        raise RuntimeError("Concurrent burst lacks unique genuine BBB metadata")
    key = movies[0].get("ratingKey")
    fixtures = [(movie_section, movie, []),
                (tv_section, episode1, ["--kind", "episode", "--season", "1", "--episode", "1"]),
                (tv_section, episode2, ["--kind", "episode", "--season", "1", "--episode", "2"]),
                (movie_section, movie, [])]
    for index, (section, media, options) in enumerate(fixtures, 1):
        directory = output / ("client-" + str(index))
        directory.mkdir()
        log = (directory / "client.log").open("wb")
        logs.append(log)
        clients.append(subprocess.Popen([sys.executable, "-c", client_code, str(gate), str(directory),
                                         section, media, str(directory), *options, "--sample-seconds", "20"],
                                        stdout=log, stderr=subprocess.STDOUT, start_new_session=True))
    gate.touch()
    workload = threading.Thread(target=mixed_workload, args=(key,), daemon=True)
    workload.start()
    deadline = time.monotonic() + 180
    next_lifecycle = time.monotonic()
    while any(client.poll() is None for client in clients):
        if time.monotonic() > deadline:
            raise RuntimeError("Concurrent real-media burst exceeded 180 seconds")
        count = subprocess.run(["psql", "-X", "-At", "-v", "ON_ERROR_STOP=1", "-c",
                                "SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() "
                                "AND client_addr IS NOT NULL AND pid<>pg_backend_pid()"],
                               env=pg_environment, capture_output=True, text=True, timeout=10, check=True)
        result["postgres_sessions"].append({"time": time.monotonic(), "count": int(count.stdout.strip())})
        if time.monotonic() >= next_lifecycle and len(result["pms_lifecycle"]["during"]) < 32:
            current = lifecycle["snapshot"]()
            result["pms_lifecycle"]["during"].append({"snapshot": current,
                                                     "transition": lifecycle["compare"](baseline, current)})
            next_lifecycle = time.monotonic() + 1
        time.sleep(0.1)
    done.set()
    workload.join(timeout=25)
    if workload.is_alive() or workload_errors:
        raise RuntimeError("Concurrent metadata/watch workload failed: " + "; ".join(workload_errors))
    if any(client.returncode != 0 for client in clients):
        raise RuntimeError("Concurrent real-media client failed; see client logs")
    results = []
    for index in range(1, 5):
        directory = output / ("client-" + str(index))
        playback = json.loads((directory / "bbb-playback.json").read_text())
        interval = json.loads((directory / "decoder-interval.json").read_text())
        if not (playback.get("passed") and playback["decode"]["completed"]
                and playback["decode"]["input"] == "plex_http"
                and playback["delivery"]["sample_seconds"] == 20):
            raise RuntimeError("Concurrent client did not complete real HTTP audio/video decoding")
        results.append({"client": index, "interval": interval, "media": playback["media"],
                        "passed": True, "playback_evidence": "client-" + str(index) + "/bbb-playback.json"})
    overlap_start = max(item["interval"]["start"] for item in results)
    overlap_end = min(item["interval"]["end"] for item in results)
    result["clients"] = results
    result["four_client_overlap_seconds"] = max(0, overlap_end - overlap_start)
    if overlap_end <= overlap_start:
        raise RuntimeError("Four real HTTP decoder processes did not overlap")
    mixed_overlap = [event for event in result["workload"]
                     if event["start"] < overlap_end and event["end"] > overlap_start]
    if not any(event["action"].startswith("metadata_read_and_") for event in mixed_overlap):
        raise RuntimeError("Metadata/watch workload did not overlap all four HTTP clients")
    if not any(event["action"] == "tv_rescan" for event in mixed_overlap):
        raise RuntimeError("Real TV rescan did not overlap all four HTTP clients")
    if not any(sample["count"] > 0 and overlap_start <= sample["time"] <= overlap_end
               for sample in result["postgres_sessions"]):
        raise RuntimeError("No PostgreSQL session observation during four-client overlap")
    result["mixed_workload_overlap_events"] = len(mixed_overlap)
    result["passed"] = True
except Exception as error:
    result["error"] = str(error)
    print("FAIL concurrent real-media burst: " + str(error), file=sys.stderr)
finally:
    done.set()
    final_snapshot = lifecycle["snapshot"]()
    result["pms_lifecycle"]["after"] = final_snapshot
    result["pms_lifecycle"]["transition"] = lifecycle["compare"](baseline, final_snapshot)
    transitions = [result["pms_lifecycle"]["transition"],
                   *(item["transition"] for item in result["pms_lifecycle"]["during"])]
    result["pms_process_survived_burst"] = all(item["same_pms_processes"] for item in transitions)
    if result["passed"] and not result["pms_process_survived_burst"]:
        result["passed"] = False
        result["error"] = "Actual PMS PID/start-time identity changed during concurrent real-media burst"
    for client in clients:
        if client.poll() is None:
            os.killpg(client.pid, signal.SIGTERM)
            try:
                client.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(client.pid, signal.SIGKILL)
                client.wait(timeout=5)
    for log in logs:
        log.close()
    (output / "concurrent-playback.json").write_text(json.dumps(result, indent=2) + "\n")
if result["passed"]:
    print("PASS four overlapping real H264/audio HTTP clients with TV scan and metadata/watch workload")
sys.exit(0 if result["passed"] else 1)
PY
    mkdir -p "$EVIDENCE_DIR/$stage-concurrent-real-media"
    docker cp "$plex:$output/." "$EVIDENCE_DIR/$stage-concurrent-real-media/" || status=1
    ((status == 0)) || return 1
    assert_no_crash_reports "$plex"
}

assert_real_media first-start "$real_media_sample_seconds"
real_media_verified=1
if ((real_media_sample_seconds == 0)); then real_media_full_decode_verified=1; fi
if ((real_media_sample_seconds == 0)); then
    phase="native-import-and-rollback"
    : "${BASE_PLEX_IMAGE:?digest-pinned original Plex base required for native source/rollback proof}"
    python3 "$script_dir/plex-native-roundtrip.py" --candidate "$candidate_id" \
        --base-image "$BASE_PLEX_IMAGE" --postgres "$postgres" --network "$network" \
        --fixture "$fixture" --evidence-dir "$EVIDENCE_DIR/native-roundtrip" \
        --media-cache "$media_cache" --expected-version "$EXPECTED_PLEX_VERSION"
    native_roundtrip_verified=1
fi
capture_pms_lifecycle before-download-network-disconnect
docker network disconnect "$download_network" "$plex"
capture_pms_lifecycle after-download-network-disconnect
compare_pms_lifecycle before-download-network-disconnect after-download-network-disconnect
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
phase="concurrent-real-media"
assert_concurrent_real_media smoke
concurrent_real_media_verified=1
set_watch_state scrobble
assert_watch_state concurrent-smoke-watched 1
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
        assert_real_media "$stage" 20
        assert_concurrent_real_media "$stage"
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
assert_real_media live-recovery "$real_media_sample_seconds"
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
phase="native-workload-complete"
echo "PASS native workload; complete matrix certification is evaluated separately."
exit 0
