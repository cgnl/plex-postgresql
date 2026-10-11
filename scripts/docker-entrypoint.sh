#!/usr/bin/with-contenv bash
# Docker entrypoint for plex-postgresql
# Initializes PostgreSQL schema before Plex starts
# Uses with-contenv to access Docker environment variables in s6-overlay

set -eo pipefail

# Migration library location (copied by Dockerfile)
MIGRATE_LIB="/usr/local/lib/plex-postgresql/migrate_lib.sh"

# Set up variables for migration library
# Auto-detect source SQLite database from common locations
detect_sqlite_db() {
    local locations=(
        # Explicit mount point
        "/source-db/com.plexapp.plugins.library.db"
        # Linux standard location (if host path mounted)
        "/var/lib/plexmediaserver/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db"
        # macOS location (if host path mounted)
        "/Users/*/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db"
        # Alternative Linux locations
        "/opt/plexmediaserver/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db"
        # Container's own database (last resort)
        "/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db"
    )

    for pattern in "${locations[@]}"; do
        if [[ -f "$pattern" ]]; then
            echo "$pattern"
            return 0
        fi
        while IFS= read -r db; do
            if [[ -f "$db" ]]; then
                echo "$db"
                return 0
            fi
        done < <(compgen -G "$pattern" || true)
    done

    # Default fallback
    echo "/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db"
}

SQLITE_DB="${PLEX_SQLITE_SOURCE:-$(detect_sqlite_db)}"

if [[ -n "$SQLITE_DB" && "$SQLITE_DB" != "/config/"* ]]; then
    echo "Found source SQLite database for migration: $SQLITE_DB"
fi
PG_HOST="${PLEX_PG_HOST:-postgres}"
PG_PORT="${PLEX_PG_PORT:-5432}"
PG_DATABASE="${PLEX_PG_DATABASE:-plex}"
PG_USER="${PLEX_PG_USER:-plex}"
PG_SCHEMA="${PLEX_PG_SCHEMA:-plex}"
SHIM_DIR="/usr/local/lib/plex-postgresql"

# Non-interactive mode for Docker (auto-migrate if PG is empty)
MIGRATION_INTERACTIVE="${MIGRATION_INTERACTIVE:-0}"

# Source migration library if available
if [[ -f "$MIGRATE_LIB" ]]; then
    source "$MIGRATE_LIB"
fi

# Wait for PostgreSQL to be ready
wait_for_postgres() {
    echo "Waiting for PostgreSQL at ${PLEX_PG_HOST}:${PLEX_PG_PORT}..."

    export PGHOST="${PLEX_PG_HOST:-postgres}"
    export PGPORT="${PLEX_PG_PORT:-5432}"
    export PGDATABASE="${PLEX_PG_DATABASE:-plex}"
    export PGUSER="${PLEX_PG_USER:-plex}"
    export PGPASSWORD="${PLEX_PG_PASSWORD:-plex}"

    local max_attempts=30
    local attempt=1

    while [ $attempt -le $max_attempts ]; do
        if migration_psql -c "SELECT 1" >/dev/null 2>&1; then
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

# Initialize schema if needed
init_schema() {
    local schema="${PLEX_PG_SCHEMA:-plex}"
    local schema_file="/usr/local/lib/plex-postgresql/plex_schema.sql"
    local compat_file="/usr/local/lib/plex-postgresql/pg_compat_functions.sql"

    migration_psql -c "CREATE SCHEMA IF NOT EXISTS $schema;" || return 1

    local table_count
    table_count=$(migration_psql -t -c "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '$schema';" 2>/dev/null | tr -d ' ') || return 1

    if [ "$table_count" -gt "0" ] 2>/dev/null; then
        echo "PostgreSQL schema '$schema' ready with $table_count tables"
        # Load sqlite_column_types metadata if table doesn't exist yet
        local types_file="/usr/local/lib/plex-postgresql/sqlite_column_types.sql"
        if [ -f "$types_file" ]; then
            local types_exists
            types_exists=$(migration_psql -t -c "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '$schema' AND table_name = 'sqlite_column_types';" 2>/dev/null | tr -d ' ') || return 1
            if [ "$types_exists" = "0" ] 2>/dev/null; then
                echo "Loading sqlite_column_types metadata..."
                load_pg_schema_file "$types_file" || return 1
            fi
        fi
    else
        echo "PostgreSQL schema '$schema' is empty, loading schema..."
        if [ -f "$schema_file" ]; then
            echo "Loading schema from $schema_file..."
            migration_psql -c "CREATE EXTENSION IF NOT EXISTS pg_trgm;" || return 1
            if load_pg_schema_file "$schema_file" "$SHIM_DIR/sqlite_column_types.sql" 2>&1; then
                local new_count
                new_count=$(migration_psql -t -c "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '$schema';" 2>/dev/null | tr -d ' ') || return 1
                echo "Schema loaded successfully! $new_count tables created."

                # Keep the dump's history for a fresh PostgreSQL bootstrap.
                # A source import replaces it atomically with the native library's
                # actual migration rows before synchronizing the SQLite shadows.
                local migration_count
                migration_count=$(migration_psql -t -c "SELECT COUNT(*) FROM ${schema}.schema_migrations;" 2>/dev/null | tr -d ' ') || return 1
                echo "schema_migrations has $migration_count entries (bootstrap history; source import replaces it)"
            else
                echo "ERROR: Schema load failed" >&2
                return 1
            fi
        else
            echo "ERROR: Schema file $schema_file not found!" >&2
            return 1
        fi

    fi

    validate_pg_schema_file "$schema_file" || return 1
    apply_sqlite_schema_parity_upgrades "$SHIM_DIR/sqlite_constraint_parity_upgrade.sql" \
        "$SHIM_DIR/fts_view_parity_upgrade.sql" || return 1

    # Ensure PostgreSQL compatibility helper functions exist.
    if [ -f "$compat_file" ]; then
        migration_psql -f "$compat_file" || return 1
    fi
    migration_psql -q -c "SELECT version FROM $schema.schema_migrations LIMIT 0; SELECT id FROM $schema.metadata_items LIMIT 0; SELECT id FROM $schema.accounts LIMIT 0; SELECT id FROM $schema.blobs LIMIT 0;" >/dev/null || return 1
}

# Sync schema_migrations from PostgreSQL to SQLite
# This ensures Plex doesn't try to re-run migrations that are already applied in PG
sync_schema_migrations_to_sqlite() {
    local db_file="$1"
    sync_shadow_migrations "$db_file"
}

seed_shadow_tables_from_pg() {
    local db_file="$1"
    local db_name helper table_list normalized_list raw_table table
    db_name="${2:-$(basename "$db_file")}"

    if [[ "$db_name" != "com.plexapp.plugins.library.db" ]]; then
        return 0
    fi

    if ! command -v psql >/dev/null 2>&1 || ! command -v python3 >/dev/null 2>&1; then
        echo "ERROR: Shadow seeding requires psql and python3" >&2
        return 1
    fi

    table_list="${PLEX_PG_SHADOW_SYNC_TABLES:-preferences}"
    normalized_list=$(printf '%s' "$table_list" | tr '[:upper:]' '[:lower:]' | tr -d '[:space:]')
    case "$normalized_list" in
        ""|0|none|false|off)
            echo "Shadow table seeding disabled"
            return 0
            ;;
    esac

    helper="$SHIM_DIR/seed_shadow_table_from_pg.py"
    if [[ ! -f "$helper" ]]; then
        echo "ERROR: Shadow seed helper missing: $helper" >&2
        return 1
    fi

    IFS=',' read -r -a shadow_tables <<< "$table_list"
    for raw_table in "${shadow_tables[@]}"; do
        table=$(printf '%s' "$raw_table" | tr -d '[:space:]')
        [[ -z "$table" ]] && continue
        echo "Seeding shadow SQLite $db_name table '$table' from PostgreSQL..."
        if ! python3 "$helper" "$db_file" "$table" "$PG_SCHEMA"; then
            echo "ERROR: Failed to seed shadow table '$table' from PostgreSQL" >&2
            return 1
        fi
    done
}

# Pre-initialize a single SQLite database
# The shadow SQLite is a disposable startup artifact — rebuilt every time
# to ensure the schema matches what Plex/SOCI expects. Data is not needed
# because the shim routes all reads/writes to PostgreSQL.
init_single_sqlite_db() {
    local db_file="$1"
    local schema_file="$2"
    build_shadow_database "$db_file" "$schema_file" || return 1
    chown abc:abc "$db_file" 2>/dev/null || true
}

# Pre-initialize SQLite databases with correct schema
# This is needed because SOCI validates the SQLite schema before our shim can intercept
init_sqlite_schema() {
    local db_dir="/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases"
    local schema_file="/usr/local/lib/plex-postgresql/sqlite_schema.sql"

    mkdir -p "$db_dir"

    # Initialize both databases explicitly
    init_single_sqlite_db "$db_dir/com.plexapp.plugins.library.db" "$schema_file"
    init_single_sqlite_db "$db_dir/com.plexapp.plugins.library.blobs.db" "$schema_file"
}

# Pre-create required Plex directories
# Prevents boost::filesystem errors when Plex scans for plugins and metadata
init_plex_directories() {
    local plex_dir="/config/Library/Application Support/Plex Media Server"
    
    echo "Ensuring required Plex directories exist..."
    
    # Create standard Plex directories
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

    # Set ownership to abc:abc (PUID:PGID from environment)
    chown -R abc:abc "$plex_dir" 2>/dev/null || true

    echo "Plex directories initialized"
}

is_truthy() {
    case "${1:-}" in
        1|y|Y|t|T|true|TRUE|yes|YES) return 0 ;;
        *) return 1 ;;
    esac
}

clear_flags_dat() {
    local plex_dir="/config/Library/Application Support/Plex Media Server"
    local cache_dir="$plex_dir/Cache"
    local flags_file="$cache_dir/Flags.dat"

    if [[ ! -f "$flags_file" ]]; then
        return 0
    fi

    log_flags_dat_info "pre-clear"

    local ts
    ts=$(date +%Y%m%d_%H%M%S)
    local backup="${flags_file}.bad.${ts}"
    mv "$flags_file" "$backup"
    chown abc:abc "$backup" 2>/dev/null || true
    echo "WARNING: Flags.dat moved aside due to UUID parsing failure: $backup"
}

log_flags_dat_info() {
    local tag="$1"
    local plex_dir="/config/Library/Application Support/Plex Media Server"
    local flags_file="$plex_dir/Cache/Flags.dat"

    if [[ ! -f "$flags_file" ]]; then
        return 0
    fi

    local size mtime hash=""
    if size=$(stat -c %s "$flags_file" 2>/dev/null); then
        mtime=$(stat -c %Y "$flags_file" 2>/dev/null || echo "?")
    else
        size=$(wc -c < "$flags_file" 2>/dev/null || echo "?")
        mtime=$(date -r "$flags_file" +%s 2>/dev/null || echo "?")
    fi

    if command -v sha256sum >/dev/null 2>&1; then
        hash=$(sha256sum "$flags_file" 2>/dev/null | awk '{print $1}')
    elif command -v md5sum >/dev/null 2>&1; then
        hash=$(md5sum "$flags_file" 2>/dev/null | awk '{print $1}')
    fi

    if [[ -n "$hash" ]]; then
        echo "Flags.dat info (${tag}): size=${size} mtime=${mtime} hash=${hash}"
    else
        echo "Flags.dat info (${tag}): size=${size} mtime=${mtime}"
    fi
}

maybe_clear_flags_dat_on_uuid_error() {
    local plex_dir="/config/Library/Application Support/Plex Media Server"
    local log_file="${PLEX_PG_LOG_FILE:-/config/plex_redirect_pg.log}"
    local pms_log="$plex_dir/Logs/Plex Media Server.log"

    log_flags_dat_info "startup"

    if is_truthy "${PLEX_PG_CLEAR_FLAGS_DAT:-}"; then
        clear_flags_dat
        return 0
    fi

    if ! is_truthy "${PLEX_PG_CLEAR_FLAGS_DAT_ON_UUID_ERROR:-1}"; then
        return 0
    fi

    if [[ -f "$log_file" ]] && grep -q "Invalid uuid length" "$log_file"; then
        clear_flags_dat
        return 0
    fi
    if [[ -f "$pms_log" ]] && grep -q "Invalid uuid length" "$pms_log"; then
        clear_flags_dat
        return 0
    fi
}

ensure_plex_temp_dir() {
    local temp_dir="/run/plex-temp"

    # Plex expects this path to exist and be a directory.
    mkdir -p "$temp_dir"
    chmod 1777 "$temp_dir" 2>/dev/null || true
    chown abc:abc "$temp_dir" 2>/dev/null || true
}

# Locale setup removed — Plex's bundled musl+boost::locale handles locale internally.
# Setting LANG/LC_ALL/CHARSET can interfere with exception handling on aarch64.

# Verify PostgreSQL shim configuration
# Note: The shim is now injected at Docker build time via Dockerfile
# This function just verifies the configuration is in place
verify_plex_shim() {
    local shim_path="/usr/local/lib/plex-postgresql/db_interpose_pg.so"
    # Check both s6-overlay v3 (linuxserver) and v2 (plexinc) paths
    local s6_run=""
    if [ -f "/etc/s6-overlay/s6-rc.d/svc-plex/run" ]; then
        s6_run="/etc/s6-overlay/s6-rc.d/svc-plex/run"
    elif [ -f "/etc/services.d/plex/run" ]; then
        s6_run="/etc/services.d/plex/run"
    fi

    if [ -f "$shim_path" ]; then
        echo "PostgreSQL shim library found: $shim_path"
        if [ -n "$s6_run" ] && grep -q "LD_PRELOAD=" "$s6_run" 2>/dev/null; then
            echo "Plex run script configured for PostgreSQL shim (set at build time)"
        else
            echo "WARNING: Plex run script missing LD_PRELOAD - shim may not load!"
        fi
    else
        echo "Warning: PostgreSQL shim library not found at $shim_path"
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

# Check if PLEX_CLAIM is set and warn if not
check_plex_claim() {
    if [ -z "$PLEX_CLAIM" ]; then
        echo ""
        echo "==========================================================="
        echo "  WARNING: PLEX_CLAIM token is not set!"
        echo "==========================================================="
        echo ""
        echo "  Your Plex server will start UNCLAIMED. This means:"
        echo "  - The web UI will not be accessible remotely"
        echo "  - Libraries and settings cannot be configured"
        echo "  - All database queries will return empty results"
        echo ""
        echo "  To fix this:"
        echo "  1. Go to https://plex.tv/claim"
        echo "  2. Copy your claim token (starts with 'claim-')"
        echo "  3. Add it to your docker-compose.yml:"
        echo ""
        echo "     environment:"
        echo "       - PLEX_CLAIM=claim-xxxxxxxxxxxxxxxxxxxx"
        echo ""
        echo "  4. Recreate the container:"
        echo ""
        echo "     docker compose down"
        echo "     docker compose up -d"
        echo ""
        echo "  Note: Claim tokens expire after 4 minutes!"
        echo "  Generate a fresh one right before running docker compose up."
        echo "==========================================================="
        echo ""
    fi
}

# Main
echo "=== plex-postgresql entrypoint ==="
echo "PostgreSQL: ${PLEX_PG_USER}@${PLEX_PG_HOST}:${PLEX_PG_PORT}/${PLEX_PG_DATABASE}"

check_plex_claim

if [ -n "$PLEX_PG_HOST" ]; then
    [[ -f "$MIGRATE_LIB" ]] || { echo "ERROR: Migration safety library missing" >&2; exit 1; }
    validate_migration_schema
    if [[ -n "${PLEX_SQLITE_SOURCE:-}" && ! -f "$SQLITE_DB" ]]; then
        echo "ERROR: Explicit SQLite migration source does not exist: $SQLITE_DB" >&2
        exit 1
    fi
    if [[ -z "${PLEX_SQLITE_SOURCE:-}" && "$SQLITE_DB" == /config/* ]] && { [[ ! -f "$SQLITE_DB" ]] || is_shadow_database "$SQLITE_DB"; }; then
        SQLITE_DB=""
    fi
    protect_shadow_destinations "/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases"
    wait_for_postgres
    init_schema
    if [[ ! -f "$SQLITE_DB" ]]; then
        seed_fresh_pg_defaults
    fi

    # Run migration if source SQLite DB exists (mounted via -v)
    if [[ -f "$MIGRATE_LIB" ]] && [[ -f "$SQLITE_DB" ]]; then
        echo "Checking for data migration..."
        check_and_migrate
    fi

    ensure_plex_temp_dir
    init_plex_directories
    maybe_clear_flags_dat_on_uuid_error
    init_sqlite_schema
    verify_plex_shim
    verify_media_mount
    
    # Final permission fix - ensure Plex can write to its directories
    # This must be done after all directories are created
    echo "Fixing final permissions..."
    chown -R abc:abc "/config/Library/Application Support/Plex Media Server" 2>/dev/null || true
else
    echo "PLEX_PG_HOST not set, skipping PostgreSQL initialization"
fi

echo "PostgreSQL initialization complete"
# When called as s6-overlay init script, just exit successfully
# s6 will continue with the rest of the init sequence
exit 0
