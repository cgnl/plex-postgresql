#!/bin/bash
# Install Plex wrapper scripts for PostgreSQL shim (macOS)
# This replaces the Plex binaries with wrapper scripts that inject the shim
# For Linux, use install_wrappers_linux.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SHIM_DIR="$(dirname "$SCRIPT_DIR")"
PLEX_APP="/Applications/Plex Media Server.app/Contents/MacOS"
SHIM_PATH="$SHIM_DIR/db_interpose_pg.dylib"
SQLITE_DB="$HOME/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db"

# PostgreSQL defaults
PG_HOST="${PLEX_PG_HOST:-localhost}"
PG_PORT="${PLEX_PG_PORT:-5432}"
PG_DATABASE="${PLEX_PG_DATABASE:-plex}"
PG_USER="${PLEX_PG_USER:-plex}"
PG_SCHEMA="${PLEX_PG_SCHEMA:-plex}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

echo "=== Plex PostgreSQL Wrapper Installer ==="
echo ""

# Check if shim exists
if [[ ! -f "$SHIM_PATH" ]]; then
    echo -e "${RED}ERROR: Shim not found at $SHIM_PATH${NC}"
    echo "Run 'make' first to build the shim."
    exit 1
fi

# Check if Plex is running
if pgrep -x "Plex Media Server" >/dev/null 2>&1 || pgrep -x "Plex Media Server.original" >/dev/null 2>&1; then
    echo -e "${YELLOW}WARNING: Plex is running. Stop it first:${NC}"
    echo "  pkill -x 'Plex Media Server' 'Plex Media Server.original'"
    exit 1
fi


# Source shared migration library
source "$SCRIPT_DIR/migrate_lib.sh"

# Run migration check before installing wrappers
check_and_migrate

echo "Installing Plex Media Server wrapper..."
if [[ -f "$PLEX_APP/Plex Media Server" && ! -f "$PLEX_APP/Plex Media Server.original" ]]; then
    # First time - backup original binary
    if file "$PLEX_APP/Plex Media Server" | grep -q "Mach-O"; then
        echo "  Backing up original binary..."
        mv "$PLEX_APP/Plex Media Server" "$PLEX_APP/Plex Media Server.original"
    else
        echo -e "${YELLOW}  Wrapper already installed (not a Mach-O binary)${NC}"
    fi
fi

if [[ -f "$PLEX_APP/Plex Media Server.original" ]]; then
    cat > "$PLEX_APP/Plex Media Server" << 'WRAPPER'
#!/bin/bash
# Plex Media Server wrapper for PostgreSQL shim

SCRIPT_DIR="$(dirname "$0")"
SERVER_BINARY="$SCRIPT_DIR/Plex Media Server.original"
SHIM_DIR="/Users/sander/plex-postgresql"

# PostgreSQL configuration
export PLEX_PG_HOST="${PLEX_PG_HOST:-/tmp}"
export PLEX_PG_PORT="${PLEX_PG_PORT:-5432}"
export PLEX_PG_DATABASE="${PLEX_PG_DATABASE:-plex}"
export PLEX_PG_USER="${PLEX_PG_USER:-plex}"
export PLEX_PG_PASSWORD="${PLEX_PG_PASSWORD:-plex}"
export PLEX_PG_SCHEMA="${PLEX_PG_SCHEMA:-plex}"
export PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR="${PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR:-/Users/sander/Library/Application Support}"

# PostgreSQL shim - validate existence before loading
SHIM_FILE="$SHIM_DIR/db_interpose_pg.dylib"
if [ ! -f "$SHIM_FILE" ]; then
    echo "[plex-pg] ERROR: Shim not found: $SHIM_FILE"
    echo "[plex-pg] Run 'make' in $SHIM_DIR to build it"
    echo "[plex-pg] Aborting to prevent transcoder crashes"
    exit 1
fi
export DYLD_INSERT_LIBRARIES="$SHIM_FILE"

# === Initialization Functions ===

wait_for_postgres() {
    echo "[plex-pg] Waiting for PostgreSQL at $PLEX_PG_HOST:$PLEX_PG_PORT..."
    local max_attempts=30
    local attempt=1

    export PGHOST="$PLEX_PG_HOST"
    export PGPORT="$PLEX_PG_PORT"
    export PGDATABASE="$PLEX_PG_DATABASE"
    export PGUSER="$PLEX_PG_USER"
    export PGPASSWORD="$PLEX_PG_PASSWORD"

    while [ $attempt -le $max_attempts ]; do
        if psql -c "SELECT 1" >/dev/null 2>&1; then
            echo "[plex-pg] PostgreSQL is ready!"
            return 0
        fi
        echo "[plex-pg] Attempt $attempt/$max_attempts - PostgreSQL not ready, waiting..."
        sleep 2
        attempt=$((attempt + 1))
    done

    echo "[plex-pg] WARNING: PostgreSQL did not become ready, continuing anyway..."
    return 1
}

init_pg_schema() {
    local schema="$PLEX_PG_SCHEMA"
    local schema_file="$SHIM_DIR/schema/plex_schema.sql"

    # Create schema if not exists
    psql -c "CREATE SCHEMA IF NOT EXISTS $schema;" 2>/dev/null || true

    # Check if tables exist
    local table_count=$(psql -t -c "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '$schema';" 2>/dev/null | tr -d ' ')

    if [ "$table_count" -gt "0" ] 2>/dev/null; then
        echo "[plex-pg] PostgreSQL schema '$schema' ready with $table_count tables"
    else
        echo "[plex-pg] PostgreSQL schema '$schema' is empty, loading schema..."
        if [ -f "$schema_file" ]; then
            if psql -f "$schema_file" >/dev/null 2>&1; then
                local new_count=$(psql -t -c "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '$schema';" 2>/dev/null | tr -d ' ')
                echo "[plex-pg] Schema loaded! $new_count tables created."
            else
                echo "[plex-pg] WARNING: Schema load had errors, continuing anyway..."
            fi
        else
            echo "[plex-pg] WARNING: Schema file not found: $schema_file"
        fi
    fi
}

init_sqlite_schema() {
    local db_dir="$PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR/Plex Media Server/Plug-in Support/Databases"
    local schema_file="$SHIM_DIR/schema/sqlite_schema.sql"

    local db_files=(
        "$db_dir/com.plexapp.plugins.library.db"
        "$db_dir/com.plexapp.plugins.library.blobs.db"
    )

    mkdir -p "$db_dir"

    for db_file in "${db_files[@]}"; do
        local db_name=$(basename "$db_file")

        if [ ! -f "$db_file" ]; then
            echo "[plex-pg] Pre-initializing SQLite database $db_name..."
            if [ -f "$schema_file" ]; then
                sqlite3 "$db_file" < "$schema_file" 2>/dev/null || true
                sqlite3 "$db_file" "INSERT OR IGNORE INTO schema_migrations (version) VALUES ('pg_adapter_1.0.0');" 2>/dev/null || true
                echo "[plex-pg] SQLite database $db_name initialized"
            fi
        else
            # Ensure min_version column exists
            if ! sqlite3 "$db_file" "SELECT min_version FROM schema_migrations LIMIT 1" >/dev/null 2>&1; then
                echo "[plex-pg] Adding min_version column to $db_name..."
                sqlite3 "$db_file" "ALTER TABLE schema_migrations ADD COLUMN min_version TEXT;" 2>/dev/null || true
            fi
        fi
    done
}

# === Run Initialization ===
echo "[plex-pg] === Plex PostgreSQL Shim ==="

if command -v psql >/dev/null 2>&1; then
    wait_for_postgres
    init_pg_schema
else
    echo "[plex-pg] WARNING: psql not found, skipping PostgreSQL initialization"
fi

if command -v sqlite3 >/dev/null 2>&1; then
    init_sqlite_schema
else
    echo "[plex-pg] WARNING: sqlite3 not found, skipping SQLite initialization"
fi

echo "[plex-pg] Starting Plex Media Server..."

# Execute the original server
exec "$SERVER_BINARY" "$@"
WRAPPER
    chmod +x "$PLEX_APP/Plex Media Server"
    echo -e "${GREEN}  Server wrapper installed${NC}"
else
    echo -e "${RED}  ERROR: Original binary not found${NC}"
    exit 1
fi

# Backup and install Scanner wrapper
echo "Installing Plex Media Scanner wrapper..."
if [[ -f "$PLEX_APP/Plex Media Scanner" && ! -f "$PLEX_APP/Plex Media Scanner.original" ]]; then
    if file "$PLEX_APP/Plex Media Scanner" | grep -q "Mach-O"; then
        echo "  Backing up original binary..."
        mv "$PLEX_APP/Plex Media Scanner" "$PLEX_APP/Plex Media Scanner.original"
    else
        echo -e "${YELLOW}  Wrapper already installed (not a Mach-O binary)${NC}"
    fi
fi

if [[ -f "$PLEX_APP/Plex Media Scanner.original" ]]; then
    cat > "$PLEX_APP/Plex Media Scanner" << 'WRAPPER'
#!/bin/bash
# Plex Media Scanner wrapper for PostgreSQL shim

SCRIPT_DIR="$(dirname "$0")"
SCANNER_ORIGINAL="$SCRIPT_DIR/Plex Media Scanner.original"

# PostgreSQL shim - validate existence before loading
SHIM_DIR="/Users/sander/plex-postgresql"
SHIM_FILE="$SHIM_DIR/db_interpose_pg.dylib"
if [ ! -f "$SHIM_FILE" ]; then
    echo "[plex-pg] ERROR: Shim not found: $SHIM_FILE"
    echo "[plex-pg] Run 'make' in $SHIM_DIR to build it"
    exit 1
fi
export DYLD_INSERT_LIBRARIES="${DYLD_INSERT_LIBRARIES:-$SHIM_FILE}"

# Disable shadow database logic
export PLEX_NO_SHADOW_SCAN=1

# Execute the original scanner
exec "$SCANNER_ORIGINAL" "$@"
WRAPPER
    chmod +x "$PLEX_APP/Plex Media Scanner"
    echo -e "${GREEN}  Scanner wrapper installed${NC}"
else
    echo -e "${RED}  ERROR: Original scanner binary not found${NC}"
    exit 1
fi

echo ""
echo -e "${GREEN}=== Installation complete ===${NC}"
echo ""
echo "Wrappers installed. Start Plex normally - the shim will be auto-injected."
echo ""
echo "To uninstall:"
echo "  ./scripts/uninstall_wrappers.sh"
