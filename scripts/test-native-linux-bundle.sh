#!/bin/bash
# Verify a staged release ZIP against an unmodified Plex image on a native host.
set -euo pipefail

BUNDLE_ZIP=${1:?Usage: test-native-linux-bundle.sh bundle.zip}
: "${BASE_PLEX_IMAGE:?Set BASE_PLEX_IMAGE to the pinned, unmodified Plex image}"
: "${POSTGRES_IMAGE:?Set POSTGRES_IMAGE to the pinned PostgreSQL fixture image}"
EVIDENCE_DIR=${EVIDENCE_DIR:-evidence/native-bundle}
mkdir -p "$EVIDENCE_DIR"
EVIDENCE_DIR=$(cd "$EVIDENCE_DIR" && pwd)
case "$(uname -m)" in
    x86_64) ARCH=x86_64; DOCKER_ARCH=amd64 ;;
    aarch64|arm64) ARCH=aarch64; DOCKER_ARCH=arm64 ;;
    *) echo "Unsupported native host architecture" >&2; exit 1 ;;
esac
[[ ${EXPECTED_ARCH:-$DOCKER_ARCH} == "$DOCKER_ARCH" ]] || {
    echo "Native host architecture differs from expected runner" >&2; exit 1;
}
docker image inspect "$BASE_PLEX_IMAGE" >/dev/null 2>&1 || docker pull "$BASE_PLEX_IMAGE"
[[ $(docker image inspect --format '{{.Architecture}}' "$BASE_PLEX_IMAGE") == "$DOCKER_ARCH" ]] || {
    echo "Plex image architecture differs from native host" >&2; exit 1;
}
[[ $(docker info --format '{{.Architecture}}') == "$ARCH" ]] || {
    echo "Docker daemon architecture differs from native host" >&2; exit 1;
}
STAGE=$(mktemp -d)
CONTAINER=""
POSTGRES=""
NETWORK=""
cleanup() {
    local status=$?
    if [[ -n "$CONTAINER" ]]; then docker rm -fv "$CONTAINER" >/dev/null || status=1; fi
    if [[ -n "$POSTGRES" ]]; then docker rm -fv "$POSTGRES" >/dev/null || status=1; fi
    if [[ -n "$NETWORK" ]]; then docker network rm "$NETWORK" >/dev/null || status=1; fi
    rm -rf "$STAGE" || status=1
    return "$status"
}
trap cleanup EXIT
python3 - "$BUNDLE_ZIP" "$STAGE" <<'PY'
import sys, zipfile
with zipfile.ZipFile(sys.argv[1]) as archive:
    archive.extractall(sys.argv[2])
PY
NETWORK=$(docker network create --label plex-postgresql.test=native-bundle "plex-bundle-$(basename "$STAGE")")
POSTGRES=$(docker run -d --network "$NETWORK" --network-alias postgres \
    --label plex-postgresql.test=native-bundle \
    -e POSTGRES_USER=plex -e POSTGRES_PASSWORD=plex -e POSTGRES_DB=plex "$POSTGRES_IMAGE")
[[ $(docker image inspect --format '{{.Architecture}}' "$POSTGRES_IMAGE") == "$DOCKER_ARCH" ]] || {
    echo 'PostgreSQL fixture image architecture differs from native host' >&2; exit 1;
}
ready=0
for attempt in $(seq 1 60); do
    if docker exec "$POSTGRES" pg_isready -h 127.0.0.1 -p 5432 -U plex -d plex >/dev/null 2>&1; then
        ready=1; break
    fi
    sleep 1
done
[[ "$ready" == 1 ]] || { echo 'PostgreSQL fixture did not become ready' >&2; exit 1; }
CONTAINER=$(docker create --label plex-postgresql.test=native-bundle \
    --network "$NETWORK" \
    --entrypoint /bin/bash -e HOME=/tmp/plex-bundle-home \
    -e TMPDIR=/tmp/plex-bundle-temp "$BASE_PLEX_IMAGE" -c 'sleep 3600')
docker start "$CONTAINER" >/dev/null
docker cp "$STAGE/." "$CONTAINER:/tmp/plex-bundle"
docker image inspect "$BASE_PLEX_IMAGE" > "$EVIDENCE_DIR/base-image.json"
printf '%s\n' "${SOURCE_SHA:-$(git rev-parse HEAD)}" > "$EVIDENCE_DIR/source-sha.txt"
python3 - "$BUNDLE_ZIP" > "$EVIDENCE_DIR/bundle-sha256.txt" <<'PY'
import hashlib, pathlib, sys
p = pathlib.Path(sys.argv[1])
print(hashlib.sha256(p.read_bytes()).hexdigest(), p.name)
PY
# All paths and binaries below belong to this disposable, unmodified base image.
docker exec -i -e ARCH="$ARCH" "$CONTAINER" /bin/bash -s \
    > "$EVIDENCE_DIR/install-and-loader.log" 2>&1 <<'INNER'
set -euo pipefail
PLEX_DIR=/usr/lib/plexmediaserver
BUNDLE=/tmp/plex-bundle
INSTALLER="$BUNDLE/scripts/install_wrappers_linux.sh"
export HOME=/tmp/plex-bundle-home
mkdir -p "$HOME" "$TMPDIR"
cd "$BUNDLE"
[[ -f scripts/migrate_table.py && -f scripts/seed_shadow_table_from_pg.py && -f schema/plex_schema.sql ]]
# Install the documented migration prerequisites in this disposable base image.
if ! command -v python3 >/dev/null || ! command -v sqlite3 >/dev/null || ! command -v psql >/dev/null; then
    apt-get update -qq
    DEBIAN_FRONTEND=noninteractive apt-get install --no-install-recommends -y python3 sqlite3 postgresql-client
fi
sha256sum "$PLEX_DIR/Plex Media Server" "$PLEX_DIR/Plex Media Scanner" > /tmp/plex-original-sha256
assert_unchanged() {
    sha256sum -c /tmp/plex-original-sha256
    [[ ! -e "$PLEX_DIR/Plex Media Server.original" && ! -e "$PLEX_DIR/Plex Media Scanner.original" ]]
}
expect_rejection() {
    if bash "$INSTALLER" > /tmp/installer-rejection.log 2>&1; then
        echo "Installer accepted invalid runtime" >&2; exit 1
    fi
    grep -E 'ERROR: (Missing|Binary does not match|Plex cannot load)' /tmp/installer-rejection.log
    assert_unchanged
}
for required in libpq.so.5 libgcc_s.so.1; do
    mv "libs/$ARCH/$required" "libs/$ARCH/$required.saved"
    expect_rejection
    mv "libs/$ARCH/$required.saved" "libs/$ARCH/$required"
done
# Simulate the original opposite-architecture library without executing it.
cp "libs/$ARCH/libpq.so.5" /tmp/libpq-original
if [[ "$ARCH" == x86_64 ]]; then
    printf '\267\000' | dd of="libs/$ARCH/libpq.so.5" bs=1 seek=18 conv=notrunc status=none
else
    printf '\076\000' | dd of="libs/$ARCH/libpq.so.5" bs=1 seek=18 conv=notrunc status=none
fi
expect_rejection
cp /tmp/libpq-original "libs/$ARCH/libpq.so.5"
mv "db_interpose_pg-linux-$ARCH.so" /tmp/selected-shim
expect_rejection
mv /tmp/selected-shim "db_interpose_pg-linux-$ARCH.so"
mv "$PLEX_DIR/Plex Media Scanner" /tmp/original-scanner
if bash "$INSTALLER" > /tmp/installer-rejection.log 2>&1; then
    echo "Installer accepted a missing Scanner" >&2; exit 1
fi
grep 'ERROR: Missing binary' /tmp/installer-rejection.log
mv /tmp/original-scanner "$PLEX_DIR/Plex Media Scanner"
assert_unchanged
mv "$PLEX_DIR/lib/libc.so" /tmp/original-plex-libc
expect_rejection
mv /tmp/original-plex-libc "$PLEX_DIR/lib/libc.so"
if SHIM_PATH="/tmp/invalid preload/shim.so" bash "$INSTALLER" > /tmp/installer-rejection.log 2>&1; then
    echo "Installer accepted a path unsupported by LD_PRELOAD" >&2; exit 1
fi
grep 'ERROR: Shim installation path' /tmp/installer-rejection.log
assert_unchanged
# Exercise the ZIP's own installer and migration helper with real SQLite files
# and an isolated PostgreSQL destination, rather than the fresh-install shortcut.
export PGHOST=postgres PGPORT=5432 PGDATABASE=plex PGUSER=plex PGPASSWORD=plex
export PLEX_PG_HOST=postgres PLEX_PG_PASSWORD=plex MIGRATION_INTERACTIVE=0
PG_SCHEMA=plex
source "$BUNDLE/scripts/migrate_lib.sh"
load_pg_schema_file "$BUNDLE/schema/plex_schema.sql" "$BUNDLE/schema/sqlite_column_types.sql"
if [[ -d /var/lib/plexmediaserver ]]; then
    source_dir='/var/lib/plexmediaserver/Library/Application Support/Plex Media Server/Plug-in Support/Databases'
else
    source_dir="$HOME/Library/Application Support/Plex Media Server/Plug-in Support/Databases"
fi
mkdir -p "$source_dir"
export SOURCE_DATABASE="$source_dir/com.plexapp.plugins.library.db"
python3 - <<'PY_SOURCE'
import os, sqlite3
from contextlib import closing
path = os.environ['SOURCE_DATABASE']
with closing(sqlite3.connect(path)) as connection, connection:
    connection.execute('CREATE TABLE metadata_items(id INTEGER PRIMARY KEY, metadata_type INTEGER, title TEXT, extra_data TEXT)')
    connection.execute('INSERT INTO metadata_items VALUES (1, 1, ?, ?)', ('Bundle migration Ω', '{"bundle":"verified"}'))
    for table in ('media_parts', 'media_items', 'metadata_item_settings', 'tags'):
        connection.execute(f'CREATE TABLE {table}(id INTEGER PRIMARY KEY, extra_data TEXT)')
    connection.execute('CREATE TABLE schema_migrations(version TEXT NOT NULL, rollback_sql TEXT, optimize_on_rollback INTEGER, min_version TEXT)')
    connection.execute("INSERT INTO schema_migrations VALUES ('bundle-proof', NULL, 0, NULL)")
with closing(sqlite3.connect(path[:-3] + '.blobs.db')) as connection, connection:
    connection.execute('CREATE TABLE blobs(id INTEGER PRIMARY KEY, linked_type TEXT, linked_id INTEGER, linked_guid TEXT, created_at INTEGER, blob_type INTEGER, blob BLOB)')
    connection.execute('INSERT INTO blobs VALUES (1, ?, 1, ?, 1, 1, ?)', ('metadata', 'bundle-guid', b'\x00\xffbundle'))
PY_SOURCE
sha256sum "$SOURCE_DATABASE" "${SOURCE_DATABASE%.db}.blobs.db" > /tmp/bundle-source-sha256
bash "$INSTALLER"
sha256sum -c /tmp/bundle-source-sha256
[[ $(psql -X -v ON_ERROR_STOP=1 -Atc "SELECT title FROM plex.metadata_items WHERE id=1") == 'Bundle migration Ω' ]]
[[ $(psql -X -v ON_ERROR_STOP=1 -Atc "SELECT encode(blob, 'hex') FROM plex.blobs WHERE id=1") == '00ff62756e646c65' ]]
[[ $(psql -X -v ON_ERROR_STOP=1 -Atc "SELECT version FROM plex.schema_migrations") == 'bundle-proof' ]]
[[ $(psql -X -v ON_ERROR_STOP=1 -Atc "SELECT count(*) FROM pg_namespace WHERE nspname LIKE 'plex_import_%'") == 0 ]]
echo 'PASS native ZIP SQLite/library/blob migration and immutable source hashes'
# Keep the remaining installer idempotency tests on the no-source path.
mv "$SOURCE_DATABASE" "$SOURCE_DATABASE.tested"
mv "${SOURCE_DATABASE%.db}.blobs.db" "${SOURCE_DATABASE%.db}.blobs.db.tested"
INSTALL_DIR=/usr/local/lib/plex-postgresql
cmp "db_interpose_pg-linux-$ARCH.so" "$INSTALL_DIR/db_interpose_pg.so"
for runtime in "libs/$ARCH"/*.so*; do
    cmp "$runtime" "$INSTALL_DIR/$(basename "$runtime")"
done
[[ $(readlink "$INSTALL_DIR/libc.musl-$ARCH.so.1") == "$PLEX_DIR/lib/libc.so" ]]
# The legacy explicit SHIM_PATH caller keeps its existing installed binary.
SHIM_PATH="$INSTALL_DIR/db_interpose_pg.so" bash "$INSTALLER"
cmp "db_interpose_pg-linux-$ARCH.so" "$INSTALL_DIR/db_interpose_pg.so"
sha256sum "$PLEX_DIR/Plex Media Server.original" "$PLEX_DIR/Plex Media Scanner.original"
# Supply a competing genuine Plex dependency even when this architecture's
# bundle contains only libpq/libgcc, so library priority is always exercised.
for dependency in libssl.so.3 libcrypto.so.3 libc++.so.2; do
    if [[ -f "$PLEX_DIR/lib/$dependency" ]]; then
        if [[ ! -e "$INSTALL_DIR/$dependency" ]]; then
            cp "$PLEX_DIR/lib/$dependency" "$INSTALL_DIR/$dependency"
        fi
        break
    fi
done
LD_PRELOAD="$INSTALL_DIR/db_interpose_pg.so" \
    LD_LIBRARY_PATH="$PLEX_DIR/lib:$INSTALL_DIR" \
    "$PLEX_DIR/lib/libc.so" --list "$PLEX_DIR/Plex Media Scanner.original" > /tmp/bundle-loader.log 2>&1
cat /tmp/bundle-loader.log
grep "$INSTALL_DIR/libpq.so.5" /tmp/bundle-loader.log
grep "$INSTALL_DIR/db_interpose_pg.so" /tmp/bundle-loader.log
python3 - "$PLEX_DIR/lib" "$INSTALL_DIR" /tmp/bundle-loader.log <<'PY_PRECEDENCE'
import pathlib, re, sys
plex, shim, log = map(pathlib.Path, sys.argv[1:])
resolved = dict(re.findall(r'^\s*(\S+) => (\S+)', log.read_text(), re.MULTILINE))
shared = [path.name for path in shim.glob('*.so*') if (plex / path.name).is_file() and path.name in resolved]
if not shared:
    raise RuntimeError('No overlapping dependency found to verify Plex library precedence')
for name in shared:
    if pathlib.Path(resolved[name]) != plex / name:
        raise RuntimeError(f'Plex dependency {name} resolved outside its own library directory')
print('PASS Plex dependency precedence:', ', '.join(sorted(shared)))
PY_PRECEDENCE
# Execute both installed wrappers using the real Plex binaries, not a mock.
timeout 20 "$PLEX_DIR/Plex Media Scanner" --version > /tmp/bundle-scanner.log 2>&1
cat /tmp/bundle-scanner.log
grep -E 'SHIM_INIT|Interpose Shim loaded' /tmp/bundle-scanner.log
timeout 20 "$PLEX_DIR/Plex Media Server" --version > /tmp/bundle-server.log 2>&1
cat /tmp/bundle-server.log
grep -E 'SHIM_INIT|Interpose Shim loaded' /tmp/bundle-server.log
# Generated wrappers must honor explicitly chosen paths.
CUSTOM_PLEX="/tmp/custom plex/bin"
CUSTOM_SHIM="/tmp/custom-runtime/shim.so"
mkdir -p "$CUSTOM_PLEX"
cp "$PLEX_DIR/Plex Media Server.original" "$CUSTOM_PLEX/Plex Media Server"
cp "$PLEX_DIR/Plex Media Scanner.original" "$CUSTOM_PLEX/Plex Media Scanner"
ln -s "$PLEX_DIR/lib" "$CUSTOM_PLEX/lib"
PLEX_DIR="$CUSTOM_PLEX" SHIM_PATH="$CUSTOM_SHIM" bash "$INSTALLER"
timeout 20 "$CUSTOM_PLEX/Plex Media Server" --version > /tmp/bundle-custom-path.log 2>&1
cat /tmp/bundle-custom-path.log
grep 'Constructor complete' /tmp/bundle-custom-path.log
cmp "db_interpose_pg-linux-$ARCH.so" "$CUSTOM_SHIM"
echo "PASS native $ARCH ZIP installation, dependency loading and both Plex wrappers"
INNER
python3 - "$EVIDENCE_DIR" "$DOCKER_ARCH" "$BASE_PLEX_IMAGE" <<'PY'
import json, pathlib, sys
root, arch, base = sys.argv[1:]
pathlib.Path(root, 'result.json').write_text(json.dumps({
    'arch': arch, 'base_image': base, 'native_bundle_install_verified': True,
    'musl_dependency_loading_verified': True, 'server_and_scanner_execution_verified': True,
    'invalid_bundle_preserves_plex': True,
    'sqlite_to_postgresql_migration_verified': True,
    'migration_source_hashes_preserved': True,
    'plex_dependency_precedence_verified': True,
    'postgresql_functionality_verified': True,
}, indent=2) + '\n')
PY
tail -1 "$EVIDENCE_DIR/install-and-loader.log"
