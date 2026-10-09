#!/usr/bin/env python3
"""Certify an isolated native Plex -> PostgreSQL -> native Plex media roundtrip.

Requires the parent's labeled PostgreSQL fixture and cached Big Buck Bunny.
Evidence is deliberately specific to the supplied Plex version and base digest.
"""

import argparse
import hashlib
import json
from pathlib import Path
import re
import signal
import subprocess
import sys
import time


TOOLS = "/usr/local/lib/plex-postgresql"
ROOT = "/config/Library/Application Support/Plex Media Server"
DATABASES = ROOT + "/Plug-in Support/Databases"
LIBRARY = "com.plexapp.plugins.library.db"
BLOBS = "com.plexapp.plugins.library.blobs.db"
MEDIA = "/config/runtime-fixture-media/Big Buck Bunny (2008).m4v"
API_CODE = '''
import json, sys, urllib.request, urllib.parse, urllib.error, xml.etree.ElementTree as ET
path, method, params = sys.argv[1:4]
params = json.loads(params)
url = "http://127.0.0.1:32400" + path
if params: url += "?" + urllib.parse.urlencode(params)
try:
    with urllib.request.urlopen(urllib.request.Request(url, method=method), timeout=60) as response:
        body = response.read(8*1024*1024+1)
except urllib.error.HTTPError as error:
    raise RuntimeError("Plex API " + method + " " + path + " returned HTTP " + str(error.code)) from None
if len(body) > 8*1024*1024: raise RuntimeError("Oversized Plex API response")
root = ET.fromstring(body) if body.strip() else None
print(json.dumps({"root": dict(root.attrib) if root is not None else {},
                  "nodes": [{"tag": node.tag, **node.attrib} for node in root.iter()] if root is not None else []}))
'''


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


class Roundtrip:
    def __init__(self, args):
        self.args = args
        self.label = "plex-pg-canary=" + args.fixture
        self.containers = []
        self.volumes = []
        self.networks = []
        self.sequence = 0
        self.database = "roundtrip_" + hashlib.sha256(args.fixture.encode()).hexdigest()[:20]
        self.database_created = False
        self.evidence = {"passed": False, "version": args.expected_version,
                         "candidate": args.candidate, "base_image": args.base_image, "stages": {}}
        args.evidence_dir.mkdir(parents=True, exist_ok=True)

    def command(self, *args, input=None, timeout=300):
        result = subprocess.run(["docker", *map(str, args)], input=input,
                                text=True, capture_output=True, timeout=timeout)
        # Keep arguments private, but retain bounded diagnostics from the child
        # so an HTTP failure is distinguishable from Docker or Python errors.
        if result.returncode != 0:
            diagnostic = result.stderr
            secrets = [getattr(self, "pg_env", {}).get("PLEX_PG_PASSWORD", "")]
            for arg in args:
                text = str(arg)
                if text.startswith(("PLEX_PG_PASSWORD=", "PGPASSWORD=")):
                    secrets.append(text.split("=", 1)[1])
            diagnostic = self.sanitize_diagnostic(diagnostic, secrets, limit=4096)
            failure = {"operation": str(args[0]), "returncode": result.returncode,
                       "stderr": diagnostic}
            failures = self.evidence.setdefault("command_failures", [])
            failures.append(failure)
            del failures[:-10]
            raise RuntimeError("Docker operation failed: " + str(args[0]) + " (exit "
                               + str(result.returncode) + "): " + (diagnostic or "no stderr"))
        return result.stdout

    def sanitize_diagnostic(self, text, secrets=(), limit=65536):
        for secret in [getattr(self, "pg_env", {}).get("PLEX_PG_PASSWORD", ""), *secrets]:
            if secret:
                text = text.replace(secret, "[redacted-fixture-password]")
        text = re.sub(r"(?i)((?:x-plex-token|plexonlinetoken|access_token)[=:\s]+[\"']?)[^\s&\"'<>]+",
                      r"\1[redacted-token]", text)
        return text[-limit:].strip()

    def inspect(self, kind, name):
        return json.loads(self.command(kind, "inspect", name))[0]

    def owned(self, kind, name):
        info = self.inspect(kind, name)
        labels = info.get("Config", info).get("Labels") or {}
        require(labels.get("plex-pg-canary") == self.args.fixture,
                "Refusing non-fixture resource: " + name)
        return info

    def volume(self, stage):
        name = self.args.fixture + "-" + stage + "-config"
        # Existing resources must never be reused or deleted by this run.
        probe = subprocess.run(["docker", "volume", "inspect", name], capture_output=True)
        require(probe.returncode != 0, "Roundtrip volume already exists: " + name)
        self.command("volume", "create", "--label", self.label, name)
        self.volumes.append(name)
        return name

    def new_container(self, name):
        probe = subprocess.run(["docker", "container", "inspect", name], capture_output=True)
        require(probe.returncode != 0, "Roundtrip container already exists: " + name)
        self.containers.append(name)

    def helper(self, volume, *args, network=None, extra_mounts=(), env=None, input=None, timeout=300):
        self.sequence += 1
        name = self.args.fixture + "-roundtrip-helper-" + str(self.sequence)
        self.new_container(name)
        command = ["run", "--rm", "-i", "--name", name, "--label", self.label,
                   "--network", network or self.args.network, "--entrypoint", "python3",
                   "-v", volume + ":/config"]
        for mount in extra_mounts:
            command += ["-v", mount]
        for key, value in (env or {}).items():
            command += ["-e", key + "=" + value]
        command += [self.args.candidate, *args]
        # Keep the name registered for cleanup if Docker times out before --rm.
        return self.command(*command, input=input, timeout=timeout)

    def api(self, container, volume, path, method="GET", params=None):
        return json.loads(self.helper(volume, "-c", API_CODE, path, method,
                                      json.dumps(params or {}), network="container:" + container))

    def start(self, stage, volume, native=False, source=None):
        name = self.args.fixture + "-" + stage
        self.new_container(name)
        command = ["run", "-d", "--name", name, "--label", self.label,
                   "--network", self.args.network, "-v", volume + ":/config", "-e", "VERSION=docker"]
        if not native:
            for key, value in self.pg_env.items():
                command += ["-e", key + "=" + value]
            command += ["-e", "MIGRATION_INTERACTIVE=0", "-e", "PLEX_PG_LOG_LEVEL=DEBUG"]
        if source:
            command += ["-v", source + ":/source-config:ro", "-e",
                        "PLEX_SQLITE_SOURCE=/source-config" + DATABASES[len('/config'):] + "/" + LIBRARY]
        command += [self.args.base_image if native else self.args.candidate]
        self.command(*command)
        if native and stage == "source":
            self.command("network", "connect", self.download_network, name)
        return name

    def ready(self, stage, container, volume, native=False):
        last_error = "not started"
        for _ in range(90):
            try:
                identity = self.api(container, volume, "/identity")["root"]
                require(identity.get("version") == self.args.expected_version, "Plex version mismatch")
                self.crashes(volume)
                if not native:
                    uid = self.command("exec", container, "python3", "-c", '''
from pathlib import Path
for p in Path('/proc').iterdir():
    if not p.name.isdigit(): continue
    try:
        if (p/'cmdline').read_bytes().split(b'\\0',1)[0].endswith(b'/Plex Media Server'):
            for line in (p/'status').read_text().splitlines():
                if line.startswith('Uid:'): print(line.split()[1]); raise SystemExit(0)
    except (OSError, PermissionError): pass
raise SystemExit(1)
''').strip()
                    require(uid.isdigit(), "Cannot identify Plex process owner")
                    maps = self.command("exec", "-u", uid, container, "python3", "-c", '''
from pathlib import Path
import os
found = []
for p in Path('/proc').iterdir():
    if not p.name.isdigit(): continue
    try:
        if os.readlink(p/'exe').endswith('/Plex Media Server'):
            found.append('db_interpose_pg.so' in (p/'maps').read_text())
    except (OSError, PermissionError): pass
if not found or not all(found): raise SystemExit(1)
print('loaded')
''')
                    require(maps.strip() == "loaded", "Imported server lacks shim")
                    sessions = self.sql("SELECT count(*) FROM pg_stat_activity WHERE datname='" + self.database
                                        + "' AND client_addr IS NOT NULL")
                    require(int(sessions.strip()) > 0, "Imported Plex lacks PostgreSQL sessions")
                self.evidence["stages"][stage] = {"ready": True, "identity": identity}
                return identity
            except (RuntimeError, ValueError) as error:
                last_error = str(error)
                info = self.inspect("container", container)
                if not info["State"]["Running"]:
                    break
                time.sleep(2)
        raise RuntimeError(stage + " readiness failed: " + last_error)

    def crashes(self, volume):
        self.helper(volume, "-c", '''
from pathlib import Path
assert not list(Path('/config/Library/Application Support/Plex Media Server/Crash Reports').rglob('*.dmp')), 'Plex crash report detected'
''')

    def sql(self, query, database=None):
        self.owned("container", self.args.postgres)
        return self.command("exec", self.args.postgres, "psql", "-X", "-U", self.pg_user,
                            "-d", database or self.database, "-v", "ON_ERROR_STOP=1", "-Atc", query)

    def source_hashes(self, source):
        return json.loads(self.helper(source, "-c", '''
import json,hashlib
from pathlib import Path
root=Path('/config/Library/Application Support/Plex Media Server')
files=[root/'Preferences.xml', *sorted((root/'Plug-in Support/Databases').glob('com.plexapp.plugins.library*.db*'))]
assert (root/'Plug-in Support/Databases/com.plexapp.plugins.library.db').is_file()
assert (root/'Plug-in Support/Databases/com.plexapp.plugins.library.blobs.db').is_file()
print(json.dumps({str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest() for p in files}))
'''))

    def clone_config(self, source, target, export=False):
        self.helper(target, "-c", '''
from pathlib import Path
import shutil,os,sys
src=Path('/source/Library/Application Support/Plex Media Server')
dst=Path('/config/Library/Application Support/Plex Media Server')
dst.mkdir(parents=True,exist_ok=True)
for name in ('Preferences.xml','Codecs'):
    p=src/name
    assert p.exists(), 'Missing native configuration '+name
    shutil.copytree(p,dst/name) if p.is_dir() else shutil.copy2(p,dst/name)
shutil.copytree('/source/runtime-fixture-media','/config/runtime-fixture-media')
if sys.argv[1]=='export':
    db=dst/'Plug-in Support/Databases'; db.mkdir(parents=True)
    for name in ('com.plexapp.plugins.library.db','com.plexapp.plugins.library.blobs.db'):
        shutil.copy2(Path('/source/roundtrip-export')/name,db/name)
for p in [Path('/config'),*Path('/config').rglob('*')]:
    os.chown(p,911,911)
''', "export" if export else "import", extra_mounts=(source + ":/source:ro",))

    def media_identity(self, container, volume, section):
        response = self.api(container, volume, "/library/sections/" + section + "/all")
        videos = [node for node in response["nodes"] if node["tag"] == "Video"]
        require(len(videos) == 1, "Expected one genuine native scanned movie")
        video = videos[0]
        require(video.get("guid") and video.get("ratingKey"), "Missing stable movie identity")
        require(int(video.get("viewCount", "0")) >= 1, "Watched state was not preserved")
        metadata = self.api(container, volume, "/library/metadata/" + video["ratingKey"])
        identity = {key: video.get(key) for key in ("ratingKey", "guid", "title", "viewCount")}
        identity["section"] = section
        identity["media_ids"] = [node.get("id") for node in metadata["nodes"] if node["tag"] == "Media"]
        identity["part_ids"] = [node.get("id") for node in metadata["nodes"] if node["tag"] == "Part"]
        require(identity["media_ids"] and identity["part_ids"], "Missing native media rows")
        return identity

    def migration_markers(self, volume):
        # Inspect private copies of stopped databases, preserving the originals.
        return json.loads(self.helper(volume, "-c", """
import hashlib,json,pathlib,shutil,sqlite3,tempfile,sys
root=pathlib.Path(sys.argv[1])
result={}
with tempfile.TemporaryDirectory() as directory:
    for kind,name in [('library',sys.argv[2]),('blobs',sys.argv[3])]:
        source=root/name
        copy=pathlib.Path(directory)/name
        for suffix in ['', '-wal', '-shm', '-journal']:
            path=pathlib.Path(str(source)+suffix)
            if path.exists(): shutil.copy2(path,str(copy)+suffix)
        connection=sqlite3.connect(copy)
        try:
            versions=sorted(str(row[0]) for row in connection.execute('SELECT version FROM schema_migrations LIMIT 10001'))
        finally:
            connection.close()
        if len(versions)>10000: raise RuntimeError('Oversized native migration history')
        encoded=json.dumps(versions,separators=(',',':')).encode()
        result[kind]={'count':len(versions),'sha256':hashlib.sha256(encoded).hexdigest()}
print(json.dumps(result))
""", DATABASES, LIBRARY, BLOBS))

    def playback(self, stage, container, volume, section, sample=False):
        output = "/config/roundtrip-evidence/" + stage
        args = [TOOLS + "/verify-bbb-playback.py", section, MEDIA, output]
        if sample:
            args += ["--sample-seconds", "20"]
        self.helper(volume, *args, network="container:" + container, timeout=420)
        destination = self.args.evidence_dir / stage
        destination.mkdir(exist_ok=True)
        self.command("cp", container + ":" + output + "/.", destination)
        result = json.loads((destination / "bbb-playback.json").read_text())
        require(result.get("passed"), "Playback failed at " + stage)
        self.evidence["stages"][stage]["playback"] = result

    def plugins(self, stage, container, volume):
        agents = self.api(container, volume, "/system/agents", params={"mediaType": 1})
        require(any(node.get("identifier") == "tv.plex.agents.none" for node in agents["nodes"]),
                "Personal media plugin unavailable at " + stage)
        self.evidence["stages"][stage]["plugin_endpoint_verified"] = True

    def snapshot(self):
        tables = ("library_sections", "section_locations", "metadata_items", "media_items",
                  "media_parts", "metadata_item_settings")
        result = {}
        for table in tables:
            rows = self.sql("SELECT COALESCE(json_agg(r ORDER BY r.id)::text,'[]') FROM "
                            "(SELECT * FROM plex." + table + ") r")
            result[table] = json.loads(rows)
        return result

    def replay_source_rejected(self, container, volume, source_volume, before, hashes):
        """Keep the one-time import mount a negative case, then detach it."""
        self.owned("container", container)
        self.command("restart", "--time", "30", container)
        marker = "Remove the migration source mount to use an existing destination"
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            logs = subprocess.run(["docker", "logs", container], capture_output=True, text=True,
                                  timeout=10)
            require(logs.returncode == 0, "Cannot inspect source-mount replay refusal")
            if marker in logs.stdout + logs.stderr:
                break
            time.sleep(1)
        else:
            raise RuntimeError("Populated destination did not reject the replayed source mount")
        self.command("stop", "--time", "30", container)
        require(self.snapshot() == before, "Rejected source-mount replay changed PostgreSQL contents")
        require(self.source_hashes(source_volume) == hashes, "Rejected replay mutated native source")
        self.evidence["source_mount_replay_rejected"] = True
        self.evidence["stages"]["source-mount-replay"] = {
            "rejected": True, "reason": marker, "postgres_contents_preserved": True,
            "source_hashes_preserved": True}
        self.owned("container", container)
        self.command("container", "rm", container)
        # Migration is a one-time installation step. Normal startup must use
        # the existing PostgreSQL database without the explicit source mount.
        return self.start("imported", volume)

    def search(self, stage, container, volume, key):
        results = {}
        for path in ("/search", "/hubs/search"):
            response = self.api(container, volume, path, params={"query": "Big Buck Bunny"})
            require(any(node.get("ratingKey") == key for node in response["nodes"]),
                    stage + " Plex search failed: " + path)
            results[path] = {"query": "Big Buck Bunny", "rating_key": key, "found": True}
        self.evidence["stages"][stage]["search"] = results

    def run(self):
        require(re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9_.-]{0,90}", self.args.fixture), "Invalid fixture name")
        require(re.search(r"@sha256:[0-9a-f]{64}$", self.args.base_image), "Native base must be digest pinned")
        self.owned("network", self.args.network)
        pg = self.owned("container", self.args.postgres)
        env = dict(entry.split("=", 1) for entry in pg["Config"]["Env"] if "=" in entry)
        self.pg_user = env.get("POSTGRES_USER", "plex")
        self.pg_env = {"PLEX_PG_HOST": "postgres", "PLEX_PG_PORT": "5432",
                       "PLEX_PG_DATABASE": self.database, "PLEX_PG_USER": self.pg_user,
                       "PLEX_PG_PASSWORD": env.get("POSTGRES_PASSWORD", ""), "PLEX_PG_SCHEMA": "plex"}
        self.download_network = self.args.fixture + "-downloads"
        probe = subprocess.run(["docker", "network", "inspect", self.download_network], capture_output=True)
        if probe.returncode == 0:
            self.owned("network", self.download_network)
        else:
            self.command("network", "create", "--label", self.label, self.download_network)
            self.networks.append(self.download_network)
        source_volume = self.volume("source")
        source = self.start("source", source_volume, native=True)
        native_identity = self.ready("source", source, source_volume, native=True)
        self.helper(source_volume, "-c", "from pathlib import Path; Path('/config/runtime-fixture-media').mkdir()")
        fixture = self.args.media_cache / "BigBuckBunny_640x360.m4v"
        require(fixture.is_file(), "Cached real BBB fixture missing")
        self.command("cp", fixture, source + ":" + MEDIA)
        self.helper(source_volume, TOOLS + "/bootstrap-plex-codecs.py", "/config/roundtrip-codecs.json",
                    network="container:" + source, timeout=420)
        self.command("network", "disconnect", self.download_network, source)
        # Plugins initialize asynchronously even after the identity endpoint is ready.
        for attempt in range(90):
            try:
                agents = self.api(source, source_volume, "/system/agents", params={"mediaType": 1})
                require(any(node.get("identifier") == "tv.plex.agents.none" for node in agents["nodes"]),
                        "Personal media agent not ready")
                created = self.api(source, source_volume, "/library/sections", "POST",
                                   {"name": self.args.fixture, "type": "movie", "agent": "tv.plex.agents.none",
                                    "scanner": "Plex Movie", "language": "en-US", "location": "/config/runtime-fixture-media"})
                sections = [node for node in created["nodes"] if node["tag"] == "Directory"]
                require(len(sections) == 1 and sections[0].get("key"), "Missing created native section")
                section = sections[0]["key"]
                break
            except RuntimeError:
                if attempt == 89:
                    raise
                time.sleep(2)
        self.api(source, source_volume, "/library/sections/" + section + "/refresh", params={"force": 1})
        self.playback("source", source, source_volume, section)
        key = self.evidence["stages"]["source"]["playback"]["media"]["rating_key"]
        self.api(source, source_volume, "/:/scrobble", params={"key": key, "identifier": "com.plexapp.plugins.library"})
        original = self.media_identity(source, source_volume, section)
        self.evidence["source_media_identity"] = original
        # Run exactly the same routes and parameters on unmodified native Plex
        # to distinguish an endpoint contract change from a PostgreSQL failure.
        self.search("source", source, source_volume, key)
        self.crashes(source_volume)
        self.command("stop", "--time", "30", source)
        hashes = self.source_hashes(source_volume)
        source_markers = self.migration_markers(source_volume)
        self.evidence["source_migration_markers"] = source_markers
        self.evidence["source_hashes"] = hashes
        self.sql('CREATE DATABASE "' + self.database + '"', env.get("POSTGRES_DB", self.pg_user))
        self.database_created = True
        imported_volume = self.volume("imported")
        self.clone_config(source_volume, imported_volume)
        imported = self.start("imported", imported_volume, source=source_volume)
        identity = self.ready("imported", imported, imported_volume)
        require(identity.get("machineIdentifier") == native_identity.get("machineIdentifier"), "Import changed server identity")
        require(self.media_identity(imported, imported_volume, section) == original, "Import changed media identities/state")
        self.plugins("imported", imported, imported_volume)
        self.playback("imported", imported, imported_volume, section)
        require(self.source_hashes(source_volume) == hashes, "Import mutated source SQLite or preferences")
        before = self.snapshot()
        imported = self.replay_source_rejected(imported, imported_volume, source_volume, before, hashes)
        restart_identity = self.ready("imported-restart", imported, imported_volume)
        require(restart_identity.get("machineIdentifier") == native_identity.get("machineIdentifier"),
                "Post-import restart changed server identity")
        require(self.media_identity(imported, imported_volume, section) == original, "Restart changed imported media")
        require(self.snapshot() == before, "Restart changed imported database contents")
        require(self.source_hashes(source_volume) == hashes, "Restart mutated native source")
        self.evidence["source_preserved"] = True
        self.evidence["restart_preserved_contents"] = True
        self.plugins("imported-restart", imported, imported_volume)
        self.search("imported-restart", imported, imported_volume, key)
        self.command("stop", "--time", "30", imported)
        export_env = {"PGHOST": "postgres", "PGPORT": "5432", "PGUSER": self.pg_user,
                      "PGPASSWORD": self.pg_env["PLEX_PG_PASSWORD"], "PGDATABASE": self.database}
        export_output = self.helper(imported_volume, TOOLS + "/export_pg_to_sqlite.py", "--schema", "plex",
                    "--output-dir", "/config/roundtrip-export", "--sqlite-schema", TOOLS + "/sqlite_schema.sql",
                    "--native-plex-sqlite", "/usr/lib/plexmediaserver/Plex SQLite",
                    "--native-blobs-template", "/source-config" + DATABASES[len('/config'):] + "/" + BLOBS,
                    extra_mounts=(source_volume + ":/source-config:ro",), env=export_env, timeout=600)
        self.evidence["native_export_stdout"] = self.sanitize_diagnostic(export_output, limit=16384)
        restored_volume = self.volume("restored")
        self.clone_config(imported_volume, restored_volume, export=True)
        self.evidence["export_hashes"] = self.source_hashes(restored_volume)
        exported_markers = self.migration_markers(restored_volume)
        self.evidence["exported_migration_markers"] = exported_markers
        require(exported_markers == source_markers, "Native export changed source migration histories")
        restored = self.start("restored", restored_volume, native=True)
        restored_identity = self.ready("restored", restored, restored_volume, native=True)
        require(restored_identity.get("machineIdentifier") == native_identity.get("machineIdentifier"), "Restore changed server identity")
        require(self.media_identity(restored, restored_volume, section) == original, "Native restore changed media or watched state")
        self.plugins("restored", restored, restored_volume)
        self.search("restored", restored, restored_volume, key)
        self.playback("restored", restored, restored_volume, section, sample=True)
        self.crashes(restored_volume)
        require(self.source_hashes(source_volume) == hashes, "Export/restore mutated original native source")
        self.evidence["native_rollback_verified"] = True
        self.evidence["passed"] = True

    def cleanup(self):
        errors = []
        # Preserve the failed initializer and native crash evidence before
        # destroying disposable resources. Commands can contain fixture secrets.
        diagnostics = self.args.evidence_dir / "diagnostics"
        diagnostics.mkdir(exist_ok=True)
        for name in dict.fromkeys(self.containers):
            probe = subprocess.run(["docker", "container", "inspect", name], capture_output=True)
            if probe.returncode != 0:
                continue
            try:
                info = self.owned("container", name)
                (diagnostics / (name + "-state.json")).write_text(json.dumps(info["State"], indent=2))
                logs = subprocess.run(["docker", "logs", "--tail", "500", name],
                                      capture_output=True, text=True, timeout=30)
                (diagnostics / (name + ".log")).write_text(
                    self.sanitize_diagnostic(logs.stdout + logs.stderr))
                volume = name + "-config"
                if not self.evidence["passed"] and volume in self.volumes:
                    self.owned("volume", volume)
                    native_log = self.helper(volume, "-c", '''
from pathlib import Path
log=Path('/config/Library/Application Support/Plex Media Server/Logs/Plex Media Server.log')
if log.is_file():
    with log.open('rb') as source:
        source.seek(max(0,log.stat().st_size-65536))
        print(source.read(65536).decode(errors='replace'))
else:
    print('Native Plex Media Server.log is absent')
''', timeout=30)
                    (diagnostics / (name + "-native-pms.log")).write_text(
                        self.sanitize_diagnostic(native_log))
            except (RuntimeError, OSError, subprocess.SubprocessError):
                errors.append("diagnostics:" + name)
        if not self.evidence["passed"] and self.database_created:
            try:
                self.owned("container", self.args.postgres)
                pg_logs = subprocess.run(["docker", "logs", "--tail", "200", self.args.postgres],
                                         capture_output=True, text=True, timeout=30)
                (diagnostics / "postgres.log").write_text(
                    self.sanitize_diagnostic(pg_logs.stdout + pg_logs.stderr))
            except (RuntimeError, OSError, subprocess.SubprocessError):
                errors.append("diagnostics:postgres")
        for kind, names in (("container", self.containers), ("volume", self.volumes), ("network", self.networks)):
            for name in reversed(names):
                try:
                    probe = subprocess.run(["docker", kind, "inspect", name], capture_output=True)
                    if probe.returncode != 0:
                        continue
                    self.owned(kind, name)
                    self.command(kind, "rm", *( ["-f"] if kind == "container" else []), name)
                except (RuntimeError, subprocess.SubprocessError):
                    errors.append(kind + ":" + name)
        if self.database_created:
            try:
                self.sql("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname='" + self.database
                         + "'", "postgres")
                self.sql('DROP DATABASE "' + self.database + '"', "postgres")
            except (RuntimeError, subprocess.SubprocessError):
                errors.append("database:" + self.database)
        self.evidence["cleanup_errors"] = errors
        if errors:
            self.evidence["passed"] = False


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("candidate", "base-image", "postgres", "network", "fixture", "expected-version"):
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--evidence-dir", required=True, type=Path)
    parser.add_argument("--media-cache", required=True, type=Path)
    args = parser.parse_args()
    runner = Roundtrip(args)
    def interrupted(signum, frame):
        raise RuntimeError("Roundtrip interrupted by signal " + str(signum))
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    try:
        runner.run()
    except (RuntimeError, OSError, ValueError, subprocess.SubprocessError) as error:
        runner.evidence["error"] = str(error)
        print("FAIL native roundtrip: " + str(error), file=sys.stderr)
    finally:
        runner.cleanup()
        (args.evidence_dir / "native-roundtrip.json").write_text(json.dumps(runner.evidence, indent=2) + "\n")
    if runner.evidence["passed"]:
        print("PASS genuine native source import, preserved restart and version-matched native rollback")
        return 0
    return 1


if __name__ == "__main__":
    sys.exit(main())
