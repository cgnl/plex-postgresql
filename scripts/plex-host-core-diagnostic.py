#!/usr/bin/env python3
"""Offline, bounded host cores for PMS PIDs observed in one owned fixture."""
import json
from datetime import datetime, timezone
import os
from pathlib import Path
import selectors
import shutil
import subprocess
import sys
import time

CORE_LIMIT = 512 * 1024 * 1024
TEXT_LIMIT = 256 * 1024
PMS_EXE = "/usr/lib/plexmediaserver/Plex Media Server"


def bounded(command, limit=TEXT_LIMIT, timeout=10, destination=None):
    """Stream stdout with a hard limit; never buffer an unbounded dump."""
    output = bytearray()
    errors = bytearray()
    record = {"status": "unavailable", "bytes": 0}
    process = None
    selector = selectors.DefaultSelector()
    deadline = time.monotonic() + timeout
    try:
        process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   stdin=subprocess.DEVNULL, start_new_session=True)
        selector.register(process.stdout, selectors.EVENT_READ)
        selector.register(process.stderr, selectors.EVENT_READ)
        while selector.get_map():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                record["status"] = "timeout"
                break
            for key, _ in selector.select(min(remaining, 0.2)):
                chunk = os.read(key.fileobj.fileno(), 65536)
                if not chunk:
                    selector.unregister(key.fileobj)
                    continue
                if key.fileobj is process.stderr:
                    errors.extend(chunk[:max(0, 8192 - len(errors))])
                    continue
                if record["bytes"] + len(chunk) > limit:
                    record["status"] = "size_limit"
                    return record, bytes(output)
                record["bytes"] += len(chunk)
                if destination is None:
                    output.extend(chunk)
                else:
                    destination.write(chunk)
        if record["status"] != "timeout":
            record["exit_code"] = process.wait(timeout=max(0.01, deadline - time.monotonic()))
            denied = any(message in errors.lower() for message in
                         (b"permission denied", b"operation not permitted", b"access denied", b"password is required"))
            record["status"] = ("captured" if process.returncode == 0 else
                                "permission_denied" if denied else "unavailable")
    except subprocess.TimeoutExpired:
        record["status"] = "timeout"
    except OSError as error:
        record["error_type"] = type(error).__name__
    finally:
        selector.close()
        if process is not None:
            if process.poll() is None:
                # Stop only our diagnostic command, never the owned PMS process.
                import signal
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=1)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            process.stdout.close()
            process.stderr.close()
    return record, bytes(output)


def observed_pids(root):
    result = []
    for path in sorted(root.glob("*-pms-lifecycle.json"), key=lambda item: item.stat().st_mtime_ns, reverse=True)[:32]:
        if path.is_symlink() or path.stat().st_size > 65536:
            continue
        try:
            snapshot = json.loads(path.read_text())
            for line in snapshot.get("host_processes", "").splitlines():
                parts = line.split(None, 4)
                if len(parts) == 5 and parts[0].isdigit() and parts[4] in ("Plex Media Serv", "Plex Media Server"):
                    pid = int(parts[0])
                    if pid > 1 and pid not in result:
                        result.append(pid)
        except (OSError, ValueError, TypeError):
            continue
    return result[:4]


def safe_info(output, pid):
    """Exclude command line, environment and systemd's automatic stack trace."""
    fields = {}
    allowed = {"PID", "Executable", "Signal", "Timestamp", "Storage", "Size on Disk"}
    for line in output.decode(errors="replace").splitlines():
        key, separator, value = line.strip().partition(":")
        if separator and key in allowed:
            fields[key] = value.strip()[:1024]
    matched = (fields.get("PID", "").split(" ", 1)[0] == str(pid)
               and fields.get("Executable") == PMS_EXE)
    return fields, matched


def stack_commands(sysroot, executable, core):
    # No argument/local/environment dumps or automatic scripts from core files.
    quoted = lambda path: '"' + str(path).replace("\\", "\\\\").replace('"', '\\"') + '"'
    return ["set auto-load off", "set debuginfod enabled off", "set pagination off",
            "set print frame-arguments none", "set print entry-values no",
            "set backtrace limit 12", "set sysroot " + str(sysroot),
            "file " + quoted(executable), "core-file " + quoted(core),
            "info registers pc sp", "x/8i $pc", "info proc mappings",
            "info sharedlibrary", "thread apply all bt 12"]


def collect(root, container, fixture):
    record = {"status": "unavailable", "max_file_bytes": CORE_LIMIT,
              "max_total_bytes": CORE_LIMIT, "cores": []}
    inspected, data = bounded(["docker", "inspect", "--format",
                               '{{json .State}} {{index .Config.Labels "plex-pg-canary"}}', container])
    if inspected["status"] != "captured":
        return record
    state_data, _, label = data.decode().strip().rpartition(" ")
    if label != fixture:
        record["status"] = "ownership_rejected"
        return record
    state = json.loads(state_data)
    init_pid = state.get("Pid")
    if not isinstance(init_pid, int) or init_pid <= 1 or not state.get("Running"):
        record["status"] = "owned_container_root_unavailable"
        return record
    if not shutil.which("coredumpctl"):
        record["status"] = "coredumpctl_unavailable"
        return record
    # GitHub's native hosts provide noninteractive sudo. Do not prompt or install.
    prefix = [] if os.geteuid() == 0 else ["sudo", "-n"]
    if prefix and not shutil.which("sudo"):
        record["status"] = "privileged_read_unavailable"
        return record
    if not shutil.which("timeout"):
        record["status"] = "privileged_timeout_unavailable"
        return record
    since = datetime.fromisoformat(state["StartedAt"].replace("Z", "+00:00")).astimezone(timezone.utc).strftime("%Y-%m-%d %H:%M:%S UTC")
    sysroot = Path("/proc") / str(init_pid) / "root"
    pids = observed_pids(root)
    record.update({"status": "inspected", "observed_host_pids": pids,
                   "container_init_pid": init_pid, "sysroot": str(sysroot)})
    remaining = CORE_LIMIT
    deadline = time.monotonic() + 90
    for pid in pids:
        if time.monotonic() >= deadline or remaining <= 0:
            record["collection_limit_reached"] = True
            break
        entry = {"host_pid": pid}
        record["cores"].append(entry)
        match = ["COREDUMP_PID=" + str(pid), "COREDUMP_EXE=" + PMS_EXE]
        command = ["coredumpctl", "--no-pager", "--since=" + since]
        # Run the timeout inside sudo so root commands cannot outlive an
        # interrupted/unprivileged collector. It does not target Plex.
        privileged = lambda args: prefix + ["timeout", "--signal=KILL", "30"] + args
        info, output = bounded(privileged(command + ["-1", "info"] + match),
                               timeout=min(10, deadline - time.monotonic()))
        entry["info"] = info
        entry["metadata"], matched = safe_info(output, pid)
        if info["status"] != "captured" or not matched:
            entry["status"] = "matching_core_unavailable"
            continue
        core = root / ("host-pms-" + str(pid) + ".core")
        partial = core.with_suffix(".partial")
        try:
            with partial.open("xb") as destination:
                dumped, _ = bounded(privileged(command + ["dump"] + match), limit=remaining,
                                    timeout=min(30, deadline - time.monotonic()), destination=destination)
            entry["dump"] = dumped
            if dumped["status"] != "captured" or dumped["bytes"] == 0:
                entry["status"] = "core_not_captured"
                continue
            partial.replace(core)
            remaining -= dumped["bytes"]
            entry.update({"status": "core_captured", "evidence": core.name})
            if not shutil.which("gdb"):
                entry["stack"] = {"status": "gdb_unavailable"}
                continue
            gdb = ["gdb", "--batch", "--nx", "--nh", "--quiet"]
            for operation in stack_commands(sysroot, sysroot / PMS_EXE.lstrip("/"), core.absolute()):
                gdb.extend(["-ex", operation])
            stack, output = bounded(privileged(gdb), timeout=min(30, max(0.01, deadline - time.monotonic())))
            # GDB announces the core's command line even with argument printing off.
            filtered = "\n".join(line for line in output.decode(errors="replace").splitlines()
                                 if not line.startswith("Core was generated by"))
            (root / ("host-pms-" + str(pid) + "-stack.txt")).write_text(filtered + "\n")
            entry["stack"] = stack
        except OSError as error:
            entry["status"] = "evidence_write_failed"
            entry["error_type"] = type(error).__name__
        finally:
            partial.unlink(missing_ok=True)
    return record


if __name__ == "__main__":
    evidence, container, fixture = sys.argv[1:]
    root = Path(evidence)
    try:
        result = collect(root, container, fixture)
    except (OSError, ValueError, KeyError, TypeError) as error:
        result = {"status": "collector_error", "error_type": type(error).__name__}
    (root / "host-core-diagnostics.json").write_text(json.dumps(result, indent=2) + "\n")
