#!/usr/bin/with-contenv bash
# cont-init.d script for plex-postgresql with plexinc/pms-docker
#
# Initializes PostgreSQL schema and SQLite databases BEFORE Plex starts.
# Runs as /etc/cont-init.d/39-plex-postgresql (before 40-plex-first-run).
#
# This script runs WITHOUT LD_PRELOAD — the shim is only injected into
# the Plex run script (/etc/services.d/plex/run) at Docker build time.
# This means psql, sqlite3, and other CLI tools work normally here.

set -e

SHIM_DIR="/usr/local/lib/plex-postgresql"

# Migration library location
MIGRATE_LIB="$SHIM_DIR/migrate_lib.sh"

# PostgreSQL settings from environment (set by Docker ENV or -e flags)
PG_HOST="${PLEX_PG_HOST:-postgres}"
PG_PORT="${PLEX_PG_PORT:-5432}"
PG_DATABASE="${PLEX_PG_DATABASE:-plex}"
PG_USER="${PLEX_PG_USER:-plex}"
PG_PASSWORD="${PLEX_PG_PASSWORD:-plex}"
PG_SCHEMA="${PLEX_PG_SCHEMA:-plex}"

export PGHOST="$PG_HOST"
export PGPORT="$PG_PORT"
export PGDATABASE="$PG_DATABASE"
export PGUSER="$PG_USER"
export PGPASSWORD="$PG_PASSWORD"

# Non-interactive mode for Docker (auto-migrate if PG is empty)
MIGRATION_INTERACTIVE="${MIGRATION_INTERACTIVE:-0}"

# Source migration library if available
if [[ -f "$MIGRATE_LIB" ]]; then
    source "$MIGRATE_LIB"
fi

echo "=== plex-postgresql standalone init ==="
echo "PostgreSQL: ${PG_USER}@${PG_HOST}:${PG_PORT}/${PG_DATABASE}"

# Auto-detect source SQLite database for migration
detect_sqlite_db() {
    local locations=(
        "/source-db/com.plexapp.plugins.library.db"
        "/var/lib/plexmediaserver/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db"
        "/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db"
    )
    for db in "${locations[@]}"; do
        if [[ -f "$db" ]]; then
            echo "$db"
            return 0
        fi
    done
    echo "/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db"
}

SQLITE_DB=$(detect_sqlite_db)

# Wait for PostgreSQL
wait_for_postgres() {
    echo "Waiting for PostgreSQL..."
    local max_attempts=30
    local attempt=1
    while [ $attempt -le $max_attempts ]; do
        if psql -c "SELECT 1" >/dev/null 2>&1; then
            echo "PostgreSQL is ready!"
            return 0
        fi
        echo "Attempt $attempt/$max_attempts - PostgreSQL not ready, waiting..."
        sleep 2
        attempt=$((attempt + 1))
    done
    echo "ERROR: PostgreSQL did not become ready in time"
    return 1
}

# Initialize PG schema
init_schema() {
    local schema="$PG_SCHEMA"
    local schema_file="$SHIM_DIR/plex_schema.sql"
    local compat_file="$SHIM_DIR/pg_compat_functions.sql"

    psql -c "CREATE SCHEMA IF NOT EXISTS $schema;" 2>/dev/null || true
    psql -c "CREATE EXTENSION IF NOT EXISTS pg_trgm;" 2>/dev/null || true

    local table_count=$(psql -t -c "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '$schema';" 2>/dev/null | tr -d ' ')

    if [ "$table_count" -gt "0" ] 2>/dev/null; then
        echo "PostgreSQL schema '$schema' ready with $table_count tables"
        # Load sqlite_column_types if missing (upgrade path)
        local types_file="$SHIM_DIR/sqlite_column_types.sql"
        if [ -f "$types_file" ]; then
            local types_exists=$(psql -t -c "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '$schema' AND table_name = 'sqlite_column_types';" 2>/dev/null | tr -d ' ')
            if [ "$types_exists" = "0" ] 2>/dev/null; then
                echo "Loading sqlite_column_types metadata..."
                psql -f "$types_file" 2>/dev/null || true
            fi
        fi
    else
        echo "PostgreSQL schema '$schema' is empty, loading schema..."
        if [ -f "$schema_file" ]; then
            echo "Loading schema from $schema_file..."
            if psql -f "$schema_file" 2>&1; then
                local new_count=$(psql -t -c "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '$schema';" 2>/dev/null | tr -d ' ')
                echo "Schema loaded successfully! $new_count tables created."

                # NOTE: schema_migrations rows from the dump are kept intact.
                # The shim intercepts INSERT INTO schema_migrations and adds
                # ON CONFLICT DO NOTHING, so duplicate versions are silently ignored.
                # This prevents Plex from re-running all 446 migrations from scratch,
                # which causes DDL/schema divergence issues with the SQLite shadow DB.
                local migration_count=$(psql -t -c "SELECT COUNT(*) FROM ${schema}.schema_migrations;" 2>/dev/null | tr -d ' ')
                echo "schema_migrations has $migration_count entries (kept from dump, shim handles duplicates)"

                # Load default seed data
                local seed_file="$SHIM_DIR/seed_data.sql"
                if [ -f "$seed_file" ]; then
                    echo "Loading default seed data..."
                    psql -f "$seed_file" 2>/dev/null || true
                fi
            else
                echo "WARNING: Schema load had errors, continuing anyway..."
            fi
        else
            echo "WARNING: Schema file $schema_file not found!"
        fi
        # Load sqlite_column_types after fresh schema
        local types_file="$SHIM_DIR/sqlite_column_types.sql"
        if [ -f "$types_file" ]; then
            echo "Loading sqlite_column_types metadata..."
            psql -f "$types_file" 2>/dev/null || true
        fi
    fi

    # Ensure PostgreSQL compatibility helper functions exist.
    if [ -f "$compat_file" ]; then
        psql -f "$compat_file" 2>/dev/null || true
    fi
}

# Sync schema_migrations from PostgreSQL to SQLite
# This ensures Plex doesn't try to re-run migrations that are already applied in PG
sync_schema_migrations_to_sqlite() {
    local db_file="$1"
    local db_name
    db_name=$(basename "$db_file")

    local pg_count=$(psql -t -c "SELECT COUNT(*) FROM ${PG_SCHEMA}.schema_migrations;" 2>/dev/null | tr -d ' ')
    local sqlite_count=$(sqlite3 "$db_file" "SELECT COUNT(*) FROM schema_migrations;" 2>/dev/null || echo "0")

    if [ "$pg_count" -gt "$sqlite_count" ] 2>/dev/null; then
        echo "Syncing schema_migrations to SQLite ($sqlite_count → $pg_count rows)..."
        # Export from PG and import into SQLite
        psql -t -A -c "SELECT version FROM ${PG_SCHEMA}.schema_migrations ORDER BY version;" 2>/dev/null | while IFS= read -r version; do
            [ -z "$version" ] && continue
            sqlite3 "$db_file" "INSERT OR IGNORE INTO schema_migrations (version) VALUES ('$version');" 2>/dev/null || true
        done
        local new_count=$(sqlite3 "$db_file" "SELECT COUNT(*) FROM schema_migrations;" 2>/dev/null || echo "0")
        echo "SQLite schema_migrations now has $new_count entries"
    else
        echo "SQLite schema_migrations already in sync ($sqlite_count rows)"
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

    helper="$SHIM_DIR/seed_shadow_table_from_pg.py"
    if [[ ! -f "$helper" ]]; then
        echo "WARNING: Shadow seed helper missing: $helper"
        return 0
    fi

    table_list="${PLEX_PG_SHADOW_SYNC_TABLES:-preferences}"
    normalized_list=$(printf '%s' "$table_list" | tr '[:upper:]' '[:lower:]' | tr -d '[:space:]')
    case "$normalized_list" in
        ""|0|none|false|off)
            echo "Shadow table seeding disabled"
            return 0
            ;;
    esac

    IFS=',' read -r -a shadow_tables <<< "$table_list"
    for raw_table in "${shadow_tables[@]}"; do
        table=$(printf '%s' "$raw_table" | tr -d '[:space:]')
        [[ -z "$table" ]] && continue
        echo "Seeding shadow SQLite $db_name table '$table' from PostgreSQL..."
        if ! python3 "$helper" "$db_file" "$table" "$PG_SCHEMA"; then
            echo "WARNING: Failed to seed shadow table '$table' from PostgreSQL"
        fi
    done
}

# Pre-initialize a single SQLite database
init_single_sqlite_db() {
    local db_file="$1"
    local schema_file="$2"
    local db_name
    db_name=$(basename "$db_file")

    # Always rebuild: remove stale shadow DB to prevent schema drift.
    if [ -f "$db_file" ]; then
        echo "Rebuilding shadow SQLite $db_name (removing stale copy)..."
        rm -f "$db_file" "${db_file}-shm" "${db_file}-wal"
    fi

    echo "Creating shadow SQLite $db_name from schema..."
    if [ -f "$schema_file" ]; then
        sqlite3 "$db_file" < "$schema_file" 2>&1 || true
        echo "Shadow SQLite $db_name initialized"
    else
        echo "WARNING: Schema file not found: $schema_file"
    fi
    chown plex:plex "$db_file" 2>/dev/null || chown abc:abc "$db_file" 2>/dev/null || true

    # Sync migration versions from PG to SQLite
    sync_schema_migrations_to_sqlite "$db_file"
    seed_shadow_tables_from_pg "$db_file"
}

# Pre-initialize SQLite databases
init_sqlite_schema() {
    local db_dir="/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases"
    local schema_file="$SHIM_DIR/sqlite_schema.sql"

    mkdir -p "$db_dir"

    init_single_sqlite_db "$db_dir/com.plexapp.plugins.library.db" "$schema_file"
    init_single_sqlite_db "$db_dir/com.plexapp.plugins.library.blobs.db" "$schema_file"
}

# Pre-create required Plex directories
# Prevents boost::filesystem errors when Plex scans for plugins and metadata
init_plex_directories() {
    local plex_dir="/config/Library/Application Support/Plex Media Server"

    echo "Ensuring required Plex directories exist..."

    mkdir -p "$plex_dir/Plug-ins"
    mkdir -p "$plex_dir/Metadata"
    mkdir -p "$plex_dir/Cache"
    mkdir -p "$plex_dir/Logs"
    mkdir -p "$plex_dir/Crash Reports"

    # Ensure Preferences.xml exists (Plex crashes with boost::filesystem error without it)
    if [[ ! -f "$plex_dir/Preferences.xml" ]]; then
        local machine_id
        machine_id="$(cat /proc/sys/kernel/random/uuid 2>/dev/null | tr -d '-' || echo "plex-pg-$(date +%s)")"
        cat > "$plex_dir/Preferences.xml" << PREFEOF
<?xml version="1.0" encoding="utf-8"?>
<Preferences OldestPreviousVersion="1.43.0.10492-121068a07" MachineIdentifier="${machine_id}" ProcessedMachineIdentifier="${machine_id}" AnonymousMachineIdentifier="${machine_id}" AcceptedEULA="1" PublishServerOnPlexOnline="0"/>
PREFEOF
        echo "Created initial Preferences.xml (MachineIdentifier=${machine_id})"
    fi

    # Set ownership (plex user for plexinc image, abc for linuxserver)
    chown -R plex:plex "$plex_dir" 2>/dev/null || chown -R abc:abc "$plex_dir" 2>/dev/null || true

    echo "Plex directories initialized"
}

ensure_plex_temp_dir() {
    local temp_dir="/run/plex-temp"

    mkdir -p "$temp_dir"
    chmod 1777 "$temp_dir" 2>/dev/null || true
    chown plex:plex "$temp_dir" 2>/dev/null || true
}

# Locale setup removed — Plex's bundled musl+boost::locale handles locale internally.
# Setting LANG/LC_ALL/CHARSET can interfere with exception handling on aarch64.

# Verify shim is configured in the Plex run script
verify_plex_shim() {
    local shim_path="$SHIM_DIR/db_interpose_pg.so"
    local run_script="/etc/services.d/plex/run"

    if [ -f "$shim_path" ]; then
        echo "PostgreSQL shim library found: $shim_path"
        if grep -q "LD_PRELOAD=" "$run_script" 2>/dev/null; then
            echo "Plex run script configured for PostgreSQL shim (set at build time)"
        else
            echo "WARNING: Plex run script missing LD_PRELOAD - shim may not load!"
        fi
    else
        echo "WARNING: PostgreSQL shim library not found at $shim_path"
    fi
}

verify_media_mount() {
    local media_dir="/media"
    if [ ! -d "$media_dir" ]; then
        echo "WARNING: Media mount not found at $media_dir"
        echo "         Set PLEX_MEDIA_PATH in docker-compose/.env to your real library path."
        return 0
    fi

    if [ ! -r "$media_dir" ]; then
        echo "WARNING: Media mount exists but is not readable: $media_dir"
        echo "         Check host permissions and Docker file sharing settings."
        return 0
    fi

    local sample_file
    sample_file=$(find "$media_dir" -maxdepth 4 -type f 2>/dev/null | head -n 1 || true)
    if [ -n "$sample_file" ]; then
        echo "Media mount OK: found sample file: $sample_file"
    else
        echo "WARNING: Media mount is readable but no files found under $media_dir"
        echo "         Plex can start, but libraries will be empty until media is mounted."
    fi
}

# === Main ===

if [ -n "$PLEX_PG_HOST" ]; then
    wait_for_postgres
    init_schema

    # Run migration if source SQLite DB exists (mounted via -v)
    if [[ -f "$MIGRATE_LIB" ]] && [[ -f "$SQLITE_DB" ]]; then
        echo "Checking for data migration..."
        check_and_migrate || true
    fi

    ensure_plex_temp_dir
    init_plex_directories
    init_sqlite_schema
    verify_plex_shim
    verify_media_mount

    # Clean crash reports to prevent CrashUploader from running.
    # CrashUploader is replaced with a no-op binary, but cleaning reports
    # prevents any JobRunner invocation entirely.
    crash_dir="/config/Library/Application Support/Plex Media Server/Crash Reports"
    if [ -d "$crash_dir" ] && [ "$(ls -A "$crash_dir" 2>/dev/null)" ]; then
        rm -rf "${crash_dir:?}/"*
        echo "Cleaned crash reports (prevents CrashUploader invocation)"
    fi

    # Final permission fix - ensure Plex can write to its directories (including shadow DB folders)
    # This must be done after all directories and shadow database files are created.
    echo "Fixing final permissions..."
    chown -R plex:plex "/config/Library/Application Support/Plex Media Server" 2>/dev/null || chown -R abc:abc "/config/Library/Application Support/Plex Media Server" 2>/dev/null || true
else
    echo "PLEX_PG_HOST not set, skipping PostgreSQL initialization"
fi

echo "=== plex-postgresql standalone init complete ==="
exit 0
