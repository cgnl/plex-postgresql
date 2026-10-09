#!/bin/bash
# Verify a staged release ZIP against an unmodified Plex image on a native host.
set -euo pipefail

BUNDLE_ZIP=${1:?Usage: test-native-linux-bundle.sh bundle.zip}
: "${BASE_PLEX_IMAGE:?Set BASE_PLEX_IMAGE to the pinned, unmodified Plex image}"
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
cleanup() {
    [[ -z "$CONTAINER" ]] || docker rm -f "$CONTAINER" >/dev/null
    rm -rf "$STAGE"
}
trap cleanup EXIT
python3 - "$BUNDLE_ZIP" "$STAGE" <<'PY'
import sys, zipfile
with zipfile.ZipFile(sys.argv[1]) as archive:
    archive.extractall(sys.argv[2])
PY
CONTAINER=$(docker create --label plex-postgresql.test=native-bundle \
    --entrypoint /bin/bash -e HOME=/tmp/plex-bundle-home "$BASE_PLEX_IMAGE" -c 'sleep 300')
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
mkdir -p "$HOME"
cd "$BUNDLE"
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
bash "$INSTALLER"
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
LD_PRELOAD="$INSTALL_DIR/db_interpose_pg.so" \
    LD_LIBRARY_PATH="$INSTALL_DIR:$PLEX_DIR/lib" \
    "$PLEX_DIR/lib/libc.so" --list "$PLEX_DIR/Plex Media Scanner.original" > /tmp/bundle-loader.log 2>&1
cat /tmp/bundle-loader.log
grep "$INSTALL_DIR/libpq.so.5" /tmp/bundle-loader.log
grep "$INSTALL_DIR/db_interpose_pg.so" /tmp/bundle-loader.log
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
    'postgresql_functionality_verified': False,
}, indent=2) + '\n')
PY
tail -1 "$EVIDENCE_DIR/install-and-loader.log"
