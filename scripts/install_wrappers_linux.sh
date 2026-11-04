#!/bin/bash
# Install Plex wrapper scripts for PostgreSQL shim (Linux)
# This replaces the Plex binaries with wrapper scripts that inject the shim

set -e

PLEX_DIR="${PLEX_DIR:-/usr/lib/plexmediaserver}"
SHIM_PATH_EXPLICIT="${SHIM_PATH:-}"
SHIM_PATH="${SHIM_PATH:-/usr/local/lib/plex-postgresql/db_interpose_pg.so}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SHIM_DIR="$(dirname "$SCRIPT_DIR")"

# Plex data location
if [[ -d "/var/lib/plexmediaserver" ]]; then
    PLEX_SUPPORT_DIR="/var/lib/plexmediaserver/Library/Application Support/Plex Media Server"
else
    PLEX_SUPPORT_DIR="$HOME/Library/Application Support/Plex Media Server"
fi
SQLITE_DB="$PLEX_SUPPORT_DIR/Plug-in Support/Databases/com.plexapp.plugins.library.db"

# Shared migration library and config loader. sudo drops exported variables,
# so settings come from /etc/plex-postgresql.conf (or PLEX_PG_* lines in
# /etc/default/plexmediaserver) unless passed on the sudo command line.
source "$SCRIPT_DIR/migrate_lib.sh"
plex_pg_load_config

# PostgreSQL defaults
PG_HOST="${PLEX_PG_HOST:-localhost}"
PG_PORT="${PLEX_PG_PORT:-5432}"
PG_DATABASE="${PLEX_PG_DATABASE:-plex}"
PG_USER="${PLEX_PG_USER:-plex}"
PG_SCHEMA="${PLEX_PG_SCHEMA:-plex}"

# Colors for this script (migrate_lib.sh also defines these)
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo "=== Plex PostgreSQL Wrapper Installer (Linux) ==="
echo ""

# Check if running as root
if [[ $EUID -ne 0 ]]; then
    echo -e "${RED}ERROR: This script must be run as root${NC}"
    echo "  sudo $0"
    exit 1
fi

# Validate the selected bundle before migration or replacing Plex binaries.
case "$(uname -m)" in
    x86_64) ARCH=x86_64; ELF_MACHINE=62 ;;
    aarch64|arm64) ARCH=aarch64; ELF_MACHINE=183 ;;
    *) echo "ERROR: Unsupported Linux architecture" >&2; exit 1 ;;
esac
# The dynamic loader treats whitespace and colons as preload separators.
if [[ "$SHIM_PATH" == *[[:space:]:]* ]]; then
    echo "ERROR: Shim installation path cannot contain whitespace or colons" >&2
    exit 1
fi
SHIM_INSTALL_DIR="$(dirname "$SHIM_PATH")"
SHIM_SOURCE="$SHIM_PATH"
RUNTIME_DIR=""
if [[ ! -f "$SHIM_PATH_EXPLICIT" ]] &&
    { [[ -f "$SHIM_DIR/db_interpose_pg-linux-x86_64.so" ]] || [[ -f "$SHIM_DIR/db_interpose_pg-linux-aarch64.so" ]]; }; then
    SHIM_SOURCE="$SHIM_DIR/db_interpose_pg-linux-$ARCH.so"
    RUNTIME_DIR="$SHIM_DIR/libs/$ARCH"
    for required in libpq.so.5 libgcc_s.so.1; do
        [[ -f "$RUNTIME_DIR/$required" ]] || {
            echo "ERROR: Missing $ARCH runtime library: $RUNTIME_DIR/$required" >&2
            exit 1
        }
    done
fi

validate_elf() {
    local path="$1" header machine
    [[ -f "$path" ]] || { echo "ERROR: Missing binary: $path" >&2; return 1; }
    header=$(od -An -tx1 -N6 "$path" | tr -d ' \n')
    machine=$(od -An -tu2 -j18 -N2 "$path")
    if [[ "$header" != 7f454c460201 || "$machine" -ne "$ELF_MACHINE" ]]; then
        echo "ERROR: Binary does not match Linux $ARCH: $path" >&2
        return 1
    fi
}
validate_elf "$SHIM_SOURCE"
if [[ -n "$RUNTIME_DIR" ]]; then
    for runtime in "$RUNTIME_DIR"/*.so*; do
        validate_elf "$runtime"
    done
fi
validate_elf "$PLEX_DIR/lib/libc.so"
for binary in "Plex Media Server" "Plex Media Scanner"; do
    original="$PLEX_DIR/$binary"
    [[ ! -f "$original.original" ]] || original="$original.original"
    validate_elf "$original"
done

# Resolve the complete runtime through Plex's own musl loader. This catches
# missing indirect dependencies before touching the installed Plex binaries.
LOADER_CHECK_DIR=$(mktemp -d)
trap 'rm -rf "$LOADER_CHECK_DIR"' EXIT
ln -s "$PLEX_DIR/lib/libc.so" "$LOADER_CHECK_DIR/libc.musl-$ARCH.so.1"
for binary in "Plex Media Server" "Plex Media Scanner"; do
    original="$PLEX_DIR/$binary"
    [[ ! -f "$original.original" ]] || original="$original.original"
    if ! LD_PRELOAD="$SHIM_SOURCE" \
        LD_LIBRARY_PATH="$LOADER_CHECK_DIR:$PLEX_DIR/lib:${RUNTIME_DIR:-$SHIM_INSTALL_DIR}${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
        "$PLEX_DIR/lib/libc.so" --list "$original"; then
        echo "ERROR: Plex cannot load the selected shim and runtime libraries" >&2
        exit 1
    fi
done
rm -rf "$LOADER_CHECK_DIR"
trap - EXIT

# Check if Plex is running
PLEX_RUNNING=0
if pgrep -f "Plex Media Server" >/dev/null 2>&1; then
    PLEX_RUNNING=1
    if [[ "${MIGRATION_LIVE_SOURCE:-0}" != 1 ]]; then
        echo -e "${YELLOW}WARNING: Plex is running. Stop it first:${NC}"
        echo "  sudo systemctl stop plexmediaserver"
        echo "Or migrate a snapshot of the running library and restart afterwards (changes"
        echo "made after the snapshot are lost): sudo MIGRATION_LIVE_SOURCE=1 $0"
        exit 1
    fi
fi

# Prepare the destination before migrating: native installs have no entrypoint
# that loads the schema, so the import would otherwise find no tables.
migration_preflight
init_pg_schema "$(resolve_schema_dir)"
if [[ ! -f "$SQLITE_DB" ]]; then
    seed_fresh_pg_defaults
fi

# Run migration check before installing wrappers. A re-run finds the shim's
# SQLite shadow instead of a native library; there is nothing to migrate then.
if [[ -f "$SQLITE_DB" ]] && is_shadow_database "$SQLITE_DB"; then
    echo "The Plex database is already a PostgreSQL shadow; skipping migration."
else
    check_and_migrate
fi

# Install the validated architecture's runtime after migration succeeds.
install -d "$SHIM_INSTALL_DIR"
if [[ "$SHIM_SOURCE" != "$SHIM_PATH" ]]; then
    install -m 755 "$SHIM_SOURCE" "$SHIM_PATH"
fi
if [[ -n "$RUNTIME_DIR" ]]; then
    for runtime in "$RUNTIME_DIR"/*.so*; do
        install -m 755 "$runtime" "$SHIM_INSTALL_DIR/$(basename "$runtime")"
    done
fi
ln -sf "$PLEX_DIR/lib/libc.so" "$SHIM_INSTALL_DIR/libc.musl-$ARCH.so.1"

# Backup and install Server wrapper
echo "Installing Plex Media Server wrapper..."
if [[ -f "$PLEX_DIR/Plex Media Server" && ! -f "$PLEX_DIR/Plex Media Server.original" ]]; then
    if validate_elf "$PLEX_DIR/Plex Media Server"; then
        echo "  Backing up original binary..."
        mv "$PLEX_DIR/Plex Media Server" "$PLEX_DIR/Plex Media Server.original"
    else
        echo -e "${YELLOW}  Wrapper already installed (not an ELF binary)${NC}"
    fi
fi

if [[ -f "$PLEX_DIR/Plex Media Server.original" ]]; then
    {
        printf '#!/bin/bash\n'
        printf 'SHIM_PATH=%q\n' "$SHIM_PATH"
        printf 'SHIM_INSTALL_DIR=%q\n' "$SHIM_INSTALL_DIR"
        # systemd never reads /etc/default/plexmediaserver for Plex, so the
        # wrapper loads the connection settings itself (same parser as the
        # installer; explicit Environment= entries still take precedence).
        declare -f plex_pg_load_config
        printf 'plex_pg_load_config\n'
    } > "$PLEX_DIR/Plex Media Server"
    cat >> "$PLEX_DIR/Plex Media Server" << 'WRAPPER'
# Plex Media Server wrapper for PostgreSQL shim

SCRIPT_DIR="$(dirname "$0")"
SERVER_BINARY="$SCRIPT_DIR/Plex Media Server.original"

# PostgreSQL shim
export LD_LIBRARY_PATH="$SCRIPT_DIR/lib:$SHIM_INSTALL_DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export LD_PRELOAD="$SHIM_PATH${LD_PRELOAD:+:$LD_PRELOAD}"
export PLEX_PG_HOST="${PLEX_PG_HOST:-localhost}"
export PLEX_PG_PORT="${PLEX_PG_PORT:-5432}"
export PLEX_PG_DATABASE="${PLEX_PG_DATABASE:-plex}"
export PLEX_PG_USER="${PLEX_PG_USER:-plex}"
export PLEX_PG_PASSWORD="${PLEX_PG_PASSWORD:-}"
export PLEX_PG_SCHEMA="${PLEX_PG_SCHEMA:-plex}"
export PLEX_PG_POOL_SIZE="${PLEX_PG_POOL_SIZE:-50}"
export PLEX_PG_IDLE_TIMEOUT="${PLEX_PG_IDLE_TIMEOUT:-300}"

# Execute the original server
exec "$SERVER_BINARY" "$@"
WRAPPER
    chmod +x "$PLEX_DIR/Plex Media Server"
    echo -e "${GREEN}  Server wrapper installed${NC}"
else
    echo -e "${RED}  ERROR: Original binary not found${NC}"
    exit 1
fi

# Backup and install Scanner wrapper
echo "Installing Plex Media Scanner wrapper..."
if [[ -f "$PLEX_DIR/Plex Media Scanner" && ! -f "$PLEX_DIR/Plex Media Scanner.original" ]]; then
    if validate_elf "$PLEX_DIR/Plex Media Scanner"; then
        echo "  Backing up original binary..."
        mv "$PLEX_DIR/Plex Media Scanner" "$PLEX_DIR/Plex Media Scanner.original"
    else
        echo -e "${YELLOW}  Wrapper already installed (not an ELF binary)${NC}"
    fi
fi

if [[ -f "$PLEX_DIR/Plex Media Scanner.original" ]]; then
    {
        printf '#!/bin/bash\n'
        printf 'SHIM_PATH=%q\n' "$SHIM_PATH"
        printf 'SHIM_INSTALL_DIR=%q\n' "$SHIM_INSTALL_DIR"
        # systemd never reads /etc/default/plexmediaserver for Plex, so the
        # wrapper loads the connection settings itself (same parser as the
        # installer; explicit Environment= entries still take precedence).
        declare -f plex_pg_load_config
        printf 'plex_pg_load_config\n'
    } > "$PLEX_DIR/Plex Media Scanner"
    cat >> "$PLEX_DIR/Plex Media Scanner" << 'WRAPPER'
# Plex Media Scanner wrapper for PostgreSQL shim

SCRIPT_DIR="$(dirname "$0")"
SCANNER_ORIGINAL="$SCRIPT_DIR/Plex Media Scanner.original"

# Ensure PostgreSQL shim is loaded
export LD_LIBRARY_PATH="$SCRIPT_DIR/lib:$SHIM_INSTALL_DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export LD_PRELOAD="$SHIM_PATH${LD_PRELOAD:+:$LD_PRELOAD}"

# Execute the original scanner
exec "$SCANNER_ORIGINAL" "$@"
WRAPPER
    chmod +x "$PLEX_DIR/Plex Media Scanner"
    echo -e "${GREEN}  Scanner wrapper installed${NC}"
else
    echo -e "${RED}  ERROR: Original scanner binary not found${NC}"
    exit 1
fi

# Persist the connection settings the migration used, so the wrappers reach
# the same database. An existing file is never overwritten.
PLEX_SERVICE_GROUP=$(stat -c %G "$PLEX_SUPPORT_DIR" 2>/dev/null || echo plex)
write_plex_pg_config "$PLEX_SERVICE_GROUP"

echo ""
echo -e "${GREEN}=== Installation complete ===${NC}"
echo ""
echo "PostgreSQL connection settings: ${PLEX_PG_CONFIG:-/etc/plex-postgresql.conf}"
echo "  (PLEX_PG_HOST, PLEX_PG_PORT, PLEX_PG_DATABASE, PLEX_PG_USER, PLEX_PG_PASSWORD, PLEX_PG_SCHEMA)"
echo ""
if [[ "$PLEX_RUNNING" == 1 ]]; then
    echo -e "${YELLOW}Plex is still running the pre-migration binary. Restart it now:${NC}"
    echo "  sudo systemctl restart plexmediaserver"
else
    echo "Then start Plex:"
    echo "  sudo systemctl start plexmediaserver"
fi
echo ""
echo "To uninstall:"
echo "  sudo ./scripts/uninstall_wrappers_linux.sh"
