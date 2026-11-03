#!/bin/bash
# Install Plex wrapper scripts for PostgreSQL shim (macOS)
#
# Server:  bash wrapper (env + init + exec .original)
# Scanner: binary patched with insert_dylib (LC_LOAD_DYLIB)
#
# For Linux, use install_wrappers_linux.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SHIM_DIR="$(dirname "$SCRIPT_DIR")"
PLEX_APP="/Applications/Plex Media Server.app/Contents/MacOS"
SHIM_SRC="$SHIM_DIR/db_interpose_pg.dylib"
SHIM_DST="$PLEX_APP/db_interpose_pg.dylib"
SHADOW_SEED_HELPER_SRC="$SCRIPT_DIR/seed_shadow_table_from_pg.py"
SHADOW_SEED_HELPER_DST="$PLEX_APP/seed_shadow_table_from_pg.py"
SQLITE_DB="$HOME/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo "=== Plex PostgreSQL Wrapper Installer ==="
echo ""

# Check if shim exists in source directory
if [[ ! -f "$SHIM_SRC" ]]; then
    echo -e "${RED}ERROR: Shim not found at $SHIM_SRC${NC}"
    echo "Run 'make' first to build the shim."
    exit 1
fi

# Check if Plex is running
if pgrep -f "Plex Media Server" >/dev/null 2>&1; then
    echo -e "${YELLOW}WARNING: Plex is running. Stop it first:${NC}"
    echo "  pkill -9 -f 'Plex Media Server'"
    exit 1
fi

# Source shared migration library (if it exists)
if [[ -f "$SCRIPT_DIR/migrate_lib.sh" ]]; then
    source "$SCRIPT_DIR/migrate_lib.sh"
    plex_pg_load_config
    # Same defaults as the server wrapper below, so migration and runtime use
    # one database; migrate_lib requires these to be set.
    PG_HOST="${PLEX_PG_HOST:-/tmp}"
    PG_PORT="${PLEX_PG_PORT:-5432}"
    PG_DATABASE="${PLEX_PG_DATABASE:-plex}"
    PG_USER="${PLEX_PG_USER:-plex}"
    PG_SCHEMA="${PLEX_PG_SCHEMA:-plex}"
    export PLEX_PG_PASSWORD="${PLEX_PG_PASSWORD:-plex}"
    migration_preflight
    init_pg_schema "$(resolve_schema_dir)"
    if [[ ! -f "$SQLITE_DB" ]]; then
        seed_fresh_pg_defaults
    fi
    if [[ -f "$SQLITE_DB" ]] && is_shadow_database "$SQLITE_DB"; then
        echo "The Plex database is already a PostgreSQL shadow; skipping migration."
    else
        check_and_migrate
    fi
fi

# ============================================================================
# Build insert_dylib if needed (for Scanner patching)
# ============================================================================

INSERT_DYLIB="$SHIM_DIR/tools/insert_dylib"
if [[ ! -f "$INSERT_DYLIB" ]]; then
    echo "Building insert_dylib tool..."
    mkdir -p "$SHIM_DIR/tools"

    # Clone and build
    TMPDIR=$(mktemp -d)
    if git clone --depth 1 https://github.com/tyilo/insert_dylib.git "$TMPDIR/insert_dylib" 2>/dev/null; then
        clang -o "$INSERT_DYLIB" "$TMPDIR/insert_dylib/insert_dylib/main.c" -O2 -framework Foundation 2>/dev/null
        rm -rf "$TMPDIR"
        if [[ -f "$INSERT_DYLIB" ]]; then
            echo -e "${GREEN}  insert_dylib built${NC}"
        else
            echo -e "${RED}  ERROR: Failed to build insert_dylib${NC}"
            exit 1
        fi
    else
        echo -e "${RED}  ERROR: Failed to clone insert_dylib repo${NC}"
        exit 1
    fi
fi

# ============================================================================
# Copy shim dylib into Plex.app
# ============================================================================
# The dylib lives next to the Plex binaries so both the Server wrapper
# (DYLD_INSERT_LIBRARIES) and the Scanner (LC_LOAD_DYLIB @loader_path)
# can find it without relying on external paths.

echo ""
echo "Installing shim dylib..."
cp -f "$SHIM_SRC" "$SHIM_DST"
echo -e "${GREEN}  Copied to $SHIM_DST${NC}"

if [[ -f "$SHADOW_SEED_HELPER_SRC" ]]; then
    cp -f "$SHADOW_SEED_HELPER_SRC" "$SHADOW_SEED_HELPER_DST"
    chmod +x "$SHADOW_SEED_HELPER_DST"
    echo -e "${GREEN}  Copied to $SHADOW_SEED_HELPER_DST${NC}"
else
    echo -e "${YELLOW}  WARNING: Shadow seed helper not found at $SHADOW_SEED_HELPER_SRC${NC}"
fi

# ============================================================================
# Server: bash wrapper + .original
# ============================================================================

echo ""
echo "Installing Plex Media Server wrapper..."

if [[ -f "$PLEX_APP/Plex Media Server" && ! -f "$PLEX_APP/Plex Media Server.original" ]]; then
    if file "$PLEX_APP/Plex Media Server" | grep -q "Mach-O"; then
        echo "  Backing up original binary → .original"
        mv "$PLEX_APP/Plex Media Server" "$PLEX_APP/Plex Media Server.original"
    else
        echo -e "${YELLOW}  Wrapper already installed (not a Mach-O binary)${NC}"
    fi
fi

if [[ -f "$PLEX_APP/Plex Media Server.original" ]]; then
    # Ensure .original has ad-hoc signature (no hardened runtime)
    local_flags=$(codesign -dvvv "$PLEX_APP/Plex Media Server.original" 2>&1 | grep "flags=" | head -1)
    if echo "$local_flags" | grep -q "runtime"; then
        echo "  Removing hardened runtime from .original..."
        codesign --remove-signature "$PLEX_APP/Plex Media Server.original"
        codesign -s - "$PLEX_APP/Plex Media Server.original"
    fi

    cat > "$PLEX_APP/Plex Media Server" << 'WRAPPER'
#!/bin/bash
# Plex Media Server wrapper for PostgreSQL shim

SCRIPT_DIR="$(dirname "$0")"
SERVER_BINARY="$SCRIPT_DIR/Plex Media Server.original"
SHIM_FILE="$SCRIPT_DIR/db_interpose_pg.dylib"
SHADOW_SEED_HELPER="$SCRIPT_DIR/seed_shadow_table_from_pg.py"

# Add PostgreSQL binaries to PATH
export PATH="/opt/homebrew/opt/postgresql@15/bin:$PATH"

# PostgreSQL configuration
export PLEX_PG_HOST="${PLEX_PG_HOST:-/tmp}"
export PLEX_PG_PORT="${PLEX_PG_PORT:-5432}"
export PLEX_PG_DATABASE="${PLEX_PG_DATABASE:-plex}"
export PLEX_PG_USER="${PLEX_PG_USER:-plex}"
export PLEX_PG_PASSWORD="${PLEX_PG_PASSWORD:-plex}"
export PLEX_PG_SCHEMA="${PLEX_PG_SCHEMA:-plex}"
export PLEX_PG_SHADOW_SYNC_TABLES="${PLEX_PG_SHADOW_SYNC_TABLES:-preferences}"
export PLEX_PG_LOG_LEVEL="${PLEX_PG_LOG_LEVEL:-ERROR}"
export PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR="${PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR:-$HOME/Library/Application Support}"

# FFmpeg external codecs (DTS, AC3, AAC, H264, HEVC, etc.)
CODEC_DIR="$PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR/Plex Media Server/Codecs"
CODEC_VERSION=$(ls -1 "$CODEC_DIR" 2>/dev/null | grep -E '^[a-f0-9]+-[a-f0-9]+-darwin-aarch64$' | head -1)
if [ -n "$CODEC_VERSION" ]; then
    export FFMPEG_EXTERNAL_LIBS="$CODEC_DIR/$CODEC_VERSION/"
    echo "[plex-pg] External codecs: $FFMPEG_EXTERNAL_LIBS"
fi

# Check shim exists
if [ ! -f "$SHIM_FILE" ]; then
    echo "[plex-pg] ERROR: Shim not found at $SHIM_FILE"
    echo "[plex-pg] Re-run install_wrappers.sh to fix."
    exit 1
fi

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

    psql -c "CREATE SCHEMA IF NOT EXISTS $schema;" 2>/dev/null || true

    local table_count=$(psql -t -c "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '$schema';" 2>/dev/null | tr -d ' ')

    if [ "$table_count" -gt "0" ] 2>/dev/null; then
        echo "[plex-pg] PostgreSQL schema '$schema' ready with $table_count tables"
    else
        echo "[plex-pg] PostgreSQL schema '$schema' is empty (Plex will create tables on first run)"
    fi

    # Ensure PostgreSQL compatibility helper functions exist.
    psql <<'SQL' >/dev/null 2>&1 || true
CREATE OR REPLACE FUNCTION public.jsonb_mergepatch(target jsonb, patch jsonb)
RETURNS jsonb
LANGUAGE plpgsql
IMMUTABLE
STRICT
PARALLEL SAFE
AS $fn$
DECLARE
    result jsonb;
    k text;
    v jsonb;
BEGIN
    IF jsonb_typeof(patch) <> 'object' THEN
        RETURN patch;
    END IF;

    IF jsonb_typeof(target) <> 'object' THEN
        result := '{}'::jsonb;
    ELSE
        result := target;
    END IF;

    FOR k, v IN SELECT e.key, e.value FROM jsonb_each(patch) AS e(key, value) LOOP
        IF v = 'null'::jsonb THEN
            result := result - k;
        ELSIF (result ? k)
              AND jsonb_typeof(result -> k) = 'object'
              AND jsonb_typeof(v) = 'object' THEN
            result := jsonb_set(result, ARRAY[k], public.jsonb_mergepatch(result -> k, v), true);
        ELSE
            result := jsonb_set(result, ARRAY[k], v, true);
        END IF;
    END LOOP;

    RETURN result;
END;
$fn$;
SQL
}

init_sqlite_schema() {
    local db_dir="$PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR/Plex Media Server/Plug-in Support/Databases"

    sync_schema_migrations_to_sqlite() {
        local db_file="$1"
        local pg_count sqlite_count new_count version

        if ! command -v psql >/dev/null 2>&1; then
            return 0
        fi

        pg_count=$(psql -t -c "SELECT COUNT(*) FROM ${PLEX_PG_SCHEMA}.schema_migrations;" 2>/dev/null | tr -d ' ')
        sqlite_count=$(sqlite3 "$db_file" "SELECT COUNT(*) FROM schema_migrations;" 2>/dev/null || echo "0")

        if [ "$pg_count" -gt "$sqlite_count" ] 2>/dev/null; then
            echo "[plex-pg] Syncing schema_migrations to SQLite ($sqlite_count -> $pg_count)..."
            psql -t -A -c "SELECT version FROM ${PLEX_PG_SCHEMA}.schema_migrations ORDER BY version;" 2>/dev/null | while IFS= read -r version; do
                [ -z "$version" ] && continue
                sqlite3 "$db_file" "INSERT OR IGNORE INTO schema_migrations (version) VALUES ('$version');" 2>/dev/null || true
            done
            new_count=$(sqlite3 "$db_file" "SELECT COUNT(*) FROM schema_migrations;" 2>/dev/null || echo "0")
            echo "[plex-pg] SQLite schema_migrations now has $new_count entries"
        fi
    }

    seed_shadow_tables_from_pg() {
        local db_file="$1"
        local db_name helper table_list normalized_list raw_table table
        db_name=$(basename "$db_file")

        if [[ "$db_name" != "com.plexapp.plugins.library.db" ]]; then
            return 0
        fi

        if ! command -v psql >/dev/null 2>&1 || ! command -v python3 >/dev/null 2>&1; then
            return 0
        fi

        helper="$SHADOW_SEED_HELPER"
        if [[ ! -f "$helper" ]]; then
            echo "[plex-pg] WARNING: Shadow seed helper missing: $helper"
            return 0
        fi

        table_list="${PLEX_PG_SHADOW_SYNC_TABLES:-preferences}"
        normalized_list=$(printf '%s' "$table_list" | tr '[:upper:]' '[:lower:]' | tr -d '[:space:]')
        case "$normalized_list" in
            ""|0|none|false|off)
                echo "[plex-pg] Shadow table seeding disabled"
                return 0
                ;;
        esac

        IFS=',' read -r -a shadow_tables <<< "$table_list"
        for raw_table in "${shadow_tables[@]}"; do
            table=$(printf '%s' "$raw_table" | tr -d '[:space:]')
            [[ -z "$table" ]] && continue
            echo "[plex-pg] Seeding shadow SQLite $db_name table '$table' from PostgreSQL..."
            if ! python3 "$helper" "$db_file" "$table" "$PLEX_PG_SCHEMA"; then
                echo "[plex-pg] WARNING: Failed to seed shadow table '$table' from PostgreSQL"
            fi
        done
    }

    local db_files=(
        "$db_dir/com.plexapp.plugins.library.db"
        "$db_dir/com.plexapp.plugins.library.blobs.db"
    )

    mkdir -p "$db_dir"

    for db_file in "${db_files[@]}"; do
        local db_name=$(basename "$db_file")

        if [ ! -f "$db_file" ]; then
            echo "[plex-pg] SQLite database $db_name not found (Plex will create it)"
        else
            if ! sqlite3 "$db_file" "SELECT min_version FROM schema_migrations LIMIT 1" >/dev/null 2>&1; then
                echo "[plex-pg] Adding min_version column to $db_name..."
                sqlite3 "$db_file" "ALTER TABLE schema_migrations ADD COLUMN min_version TEXT;" 2>/dev/null || true
            fi
        fi

        sync_schema_migrations_to_sqlite "$db_file"
        seed_shadow_tables_from_pg "$db_file"
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

# Set DYLD_INSERT_LIBRARIES right before exec (not earlier, to avoid affecting init tools)
export DYLD_INSERT_LIBRARIES="$SHIM_FILE"

echo "[plex-pg] Starting Plex Media Server (DYLD_INSERT_LIBRARIES=$SHIM_FILE)..."
exec "$SERVER_BINARY" "$@"
WRAPPER

    chmod +x "$PLEX_APP/Plex Media Server"
    echo -e "${GREEN}  Server wrapper installed${NC}"
else
    echo -e "${RED}  ERROR: Original binary not found${NC}"
    exit 1
fi

# ============================================================================
# Scanner: patch with insert_dylib (LC_LOAD_DYLIB injection)
# ============================================================================
#
# The Scanner is spawned by the Server via posix_spawn. macOS strips
# DYLD_INSERT_LIBRARIES during posix_spawn, so a bash wrapper doesn't work.
# Instead we inject the shim as a direct dylib dependency into the binary.

echo ""
echo "Installing Plex Media Scanner shim..."

SCANNER="$PLEX_APP/Plex Media Scanner"

if [[ ! -f "$SCANNER" ]]; then
    echo -e "${RED}  ERROR: Scanner binary not found${NC}"
    exit 1
fi

if [[ ! -f "$PLEX_APP/Plex Media Scanner.original" ]] && file "$SCANNER" | grep -q "Mach-O"; then
    echo "  Backing up original Scanner binary..."
    cp -p "$SCANNER" "$PLEX_APP/Plex Media Scanner.original"
fi

# Check if already patched (shim already linked)
if otool -L "$SCANNER" 2>/dev/null | grep -q "db_interpose_pg.dylib"; then
    echo -e "${YELLOW}  Scanner already patched (shim dylib linked)${NC}"
    if [[ ! -f "$PLEX_APP/Plex Media Scanner.original" ]]; then
        echo -e "${YELLOW}  WARNING: Plex Media Scanner.original missing (uninstall cannot fully restore Scanner)${NC}"
    fi
else
    if ! file "$SCANNER" | grep -q "Mach-O"; then
        echo -e "${RED}  ERROR: Scanner is not a Mach-O binary (old bash wrapper?)${NC}"
        # Check if .original exists from a previous install
        if [[ -f "$PLEX_APP/Plex Media Scanner.original" ]] && file "$PLEX_APP/Plex Media Scanner.original" | grep -q "Mach-O"; then
            echo "  Restoring from .original..."
            mv "$PLEX_APP/Plex Media Scanner.original" "$SCANNER"
        else
            echo -e "${RED}  No Mach-O binary found. Reinstall Plex first.${NC}"
            exit 1
        fi
    fi

    echo "  Injecting shim dylib into Scanner binary..."
    "$INSERT_DYLIB" --strip-codesig --all-yes \
        "@loader_path/db_interpose_pg.dylib" \
        "$SCANNER" \
        "$SCANNER.patched" >/dev/null 2>&1

    if [[ -f "$SCANNER.patched" ]]; then
        mv "$SCANNER.patched" "$SCANNER"
        # Re-sign ad-hoc (no hardened runtime)
        codesign --remove-signature "$SCANNER" 2>/dev/null || true
        codesign -s - "$SCANNER"
        echo -e "${GREEN}  Scanner patched with shim dylib${NC}"
    else
        echo -e "${RED}  ERROR: insert_dylib failed${NC}"
        exit 1
    fi
fi

# ============================================================================
# Verify
# ============================================================================

echo ""
echo "=== Verification ==="
echo ""

echo "Server:"
if [[ -f "$PLEX_APP/Plex Media Server" ]] && head -1 "$PLEX_APP/Plex Media Server" | grep -q "^#!"; then
    echo -e "  ${GREEN}Wrapper script installed${NC}"
else
    echo -e "  ${RED}FAILED${NC}"
fi

if [[ -f "$PLEX_APP/Plex Media Server.original" ]]; then
    echo -e "  ${GREEN}.original binary present${NC}"
else
    echo -e "  ${RED}.original missing!${NC}"
fi

echo ""
echo "Scanner:"
if otool -L "$PLEX_APP/Plex Media Scanner" 2>/dev/null | grep -q "db_interpose_pg.dylib"; then
    echo -e "  ${GREEN}Shim dylib injected (LC_LOAD_DYLIB)${NC}"
else
    echo -e "  ${RED}Shim NOT linked!${NC}"
fi

if [[ -f "$PLEX_APP/Plex Media Scanner.original" ]]; then
    echo -e "  ${GREEN}.original scanner backup present${NC}"
else
    echo -e "  ${YELLOW}.original scanner backup missing${NC}"
fi

local_flags=$(codesign -dvvv "$PLEX_APP/Plex Media Scanner" 2>&1 | grep "flags=" | head -1)
if echo "$local_flags" | grep -q "adhoc"; then
    echo -e "  ${GREEN}Ad-hoc signed (no hardened runtime)${NC}"
else
    echo -e "  ${YELLOW}Signing: $local_flags${NC}"
fi

echo ""
echo "Shim dylib:"
if [[ -f "$SHIM_DST" ]]; then
    echo -e "  ${GREEN}db_interpose_pg.dylib present in Plex.app${NC}"
else
    echo -e "  ${RED}db_interpose_pg.dylib missing from Plex.app!${NC}"
fi

echo ""
echo -e "${GREEN}=== Installation complete ===${NC}"
echo ""
echo "Layout inside $PLEX_APP/:"
echo "  Plex Media Server          → bash wrapper (env + init + exec .original)"
echo "  Plex Media Server.original → real server binary (shim via DYLD_INSERT_LIBRARIES)"
echo "  Plex Media Scanner         → patched binary (shim via LC_LOAD_DYLIB @loader_path)"
echo "  db_interpose_pg.dylib      → PostgreSQL shim library"
echo ""
echo "Start Plex normally - the shim will be auto-injected."
echo ""
echo "NOTE: After a Plex update, re-run this script to re-install."
echo ""
echo "To uninstall:"
echo "  ./scripts/uninstall_wrappers.sh"
