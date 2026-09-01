#!/usr/bin/with-contenv bash
# Docker entrypoint for plex-postgresql
# Initializes PostgreSQL schema before Plex starts
# Uses with-contenv to access Docker environment variables in s6-overlay

set -e

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
        # Use glob expansion for wildcard patterns
        for db in $pattern; do
            if [[ -f "$db" ]]; then
                echo "$db"
                return 0
            fi
        done
    done

    # Default fallback
    echo "/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db"
}

SQLITE_DB=$(detect_sqlite_db)
if [[ "$SQLITE_DB" != "/config/"* ]]; then
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

# Initialize schema if needed
init_schema() {
    local schema="${PLEX_PG_SCHEMA:-plex}"
    local schema_file="/usr/local/lib/plex-postgresql/plex_schema.sql"
    local compat_file="/usr/local/lib/plex-postgresql/pg_compat_functions.sql"

    psql -c "CREATE SCHEMA IF NOT EXISTS $schema;" 2>/dev/null || true

    local table_count=$(psql -t -c "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '$schema';" 2>/dev/null | tr -d ' ')

    if [ "$table_count" -gt "0" ] 2>/dev/null; then
        echo "PostgreSQL schema '$schema' ready with $table_count tables"
        # Load sqlite_column_types metadata if table doesn't exist yet
        local types_file="/usr/local/lib/plex-postgresql/sqlite_column_types.sql"
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
            psql -c "CREATE EXTENSION IF NOT EXISTS pg_trgm;" 2>/dev/null || true
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
            else
                echo "WARNING: Schema load had errors, continuing anyway..."
            fi
        else
            echo "WARNING: Schema file $schema_file not found!"
        fi
        # Load sqlite_column_types metadata after fresh schema load
        local types_file="/usr/local/lib/plex-postgresql/sqlite_column_types.sql"
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
# The shadow SQLite is a disposable startup artifact — rebuilt every time
# to ensure the schema matches what Plex/SOCI expects. Data is not needed
# because the shim routes all reads/writes to PostgreSQL.
init_single_sqlite_db() {
    local db_file="$1"
    local schema_file="$2"
    local db_name
    db_name=$(basename "$db_file")

    # Always rebuild: remove stale shadow DB to prevent schema drift.
    # Over time, shim operations can mutate the shadow schema (DDL routing,
    # column additions, etc.) making it inconsistent with Plex's expectations.
    if [ -f "$db_file" ]; then
        echo "Rebuilding shadow SQLite $db_name (removing stale copy)..."
        rm -f "$db_file" "${db_file}-shm" "${db_file}-wal"
    fi

    echo "Creating shadow SQLite $db_name from schema..."
    if [ -f "$schema_file" ]; then
        # Ignore errors from virtual tables (spellfix1, fts4, rtree)
        sqlite3 "$db_file" < "$schema_file" 2>&1 || true
        echo "Shadow SQLite $db_name initialized"
    else
        echo "WARNING: Schema file not found: $schema_file"
    fi
    chown abc:abc "$db_file" 2>/dev/null || true

    # Sync migration versions from PG to SQLite
    # This prevents Plex from re-running migrations that are already in PostgreSQL
    sync_schema_migrations_to_sqlite "$db_file"
    seed_shadow_tables_from_pg "$db_file"
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

generate_uuid() {
    local uuid
    if [ -f /proc/sys/kernel/random/uuid ]; then
        uuid=$(cat /proc/sys/kernel/random/uuid)
    elif command -v uuidgen >/dev/null 2>&1; then
        uuid=$(uuidgen)
    elif command -v python3 >/dev/null 2>&1; then
        uuid=$(python3 -c 'import uuid; print(uuid.uuid4())')
    elif command -v python >/dev/null 2>&1; then
        uuid=$(python -c 'import uuid; print(uuid.uuid4())')
    else
        # Fallback pseudo-UUID using random hex digits
        local hex="0123456789abcdef"
        uuid=""
        for i in {1..32}; do
            uuid+="${hex:$((RANDOM % 16)):1}"
        done
        uuid="${uuid:0:8}-${uuid:8:4}-${uuid:12:4}-${uuid:16:4}-${uuid:20:12}"
    fi
    echo "$uuid" | tr '[:upper:]' '[:lower:]'
}

sha1_hex() {
    local str="$1"
    if command -v sha1sum >/dev/null 2>&1; then
        echo -n "$str" | sha1sum | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        echo -n "$str" | shasum | awk '{print $1}'
    elif command -v openssl >/dev/null 2>&1; then
        echo -n "$str" | openssl dgst -sha1 | awk '{print $NF}'
    elif command -v python3 >/dev/null 2>&1; then
        python3 -c "import hashlib; print(hashlib.sha1(b'$str').hexdigest())"
    elif command -v python >/dev/null 2>&1; then
        python -c "import hashlib; print(hashlib.sha1(b'$str').hexdigest())"
    else
        # Fallback: if absolutely nothing is available (unlikely), just use a 40-char dummy hex string
        # derived from the uuid.
        local clean_str
        clean_str=$(echo -n "$str" | tr -d '-')
        echo "${clean_str}00000008"
    fi
}

update_preferences_uuid() {
    local plex_dir="/config/Library/Application Support/Plex Media Server"
    local pref_file="$plex_dir/Preferences.xml"
    if [ -f "$pref_file" ]; then
        local machine_id anonymous_id processed_id
        machine_id=$(sed -n 's/.*[[:space:]]MachineIdentifier="\([^"]*\)".*/\1/p' "$pref_file" || true)
        anonymous_id=$(sed -n 's/.*[[:space:]]AnonymousMachineIdentifier="\([^"]*\)".*/\1/p' "$pref_file" || true)
        processed_id=$(sed -n 's/.*[[:space:]]ProcessedMachineIdentifier="\([^"]*\)".*/\1/p' "$pref_file" || true)

        local modified=0

        # Fix/Generate MachineIdentifier
        if [ -z "$machine_id" ]; then
            machine_id=$(generate_uuid)
            echo "Generating missing MachineIdentifier: $machine_id"
            sed -i "s/<Preferences /<Preferences MachineIdentifier=\"$machine_id\" /g" "$pref_file"
            modified=1
        elif [ ${#machine_id} -eq 32 ]; then
            local new_machine_id="${machine_id:0:8}-${machine_id:8:4}-${machine_id:12:4}-${machine_id:16:4}-${machine_id:20:12}"
            echo "Formatting 32-char MachineIdentifier to 36-char: $new_machine_id"
            sed -i "s/MachineIdentifier=\"$machine_id\"/MachineIdentifier=\"$new_machine_id\"/g" "$pref_file"
            machine_id="$new_machine_id"
            modified=1
        fi

        # Fix/Generate AnonymousMachineIdentifier
        if [ -z "$anonymous_id" ]; then
            anonymous_id=$(generate_uuid)
            echo "Generating missing AnonymousMachineIdentifier: $anonymous_id"
            sed -i "s/<Preferences /<Preferences AnonymousMachineIdentifier=\"$anonymous_id\" /g" "$pref_file"
            modified=1
        elif [ ${#anonymous_id} -eq 32 ]; then
            local new_anonymous_id="${anonymous_id:0:8}-${anonymous_id:8:4}-${anonymous_id:12:4}-${anonymous_id:16:4}-${anonymous_id:20:12}"
            echo "Formatting 32-char AnonymousMachineIdentifier to 36-char: $new_anonymous_id"
            sed -i "s/AnonymousMachineIdentifier=\"$anonymous_id\"/AnonymousMachineIdentifier=\"$new_anonymous_id\"/g" "$pref_file"
            anonymous_id="$new_anonymous_id"
            modified=1
        fi

        # Fix/Generate ProcessedMachineIdentifier
        if [ -z "$processed_id" ]; then
            processed_id=$(sha1_hex "$machine_id")
            echo "Generating missing ProcessedMachineIdentifier: $processed_id"
            sed -i "s/<Preferences /<Preferences ProcessedMachineIdentifier=\"$processed_id\" /g" "$pref_file"
            modified=1
        elif [ ${#processed_id} -ne 40 ]; then
            local new_processed_id
            new_processed_id=$(sha1_hex "$machine_id")
            echo "Updating invalid ProcessedMachineIdentifier to SHA-1: $new_processed_id"
            sed -i "s/ProcessedMachineIdentifier=\"$processed_id\"/ProcessedMachineIdentifier=\"$new_processed_id\"/g" "$pref_file"
            processed_id="$new_processed_id"
            modified=1
        fi

        if [ "$modified" -eq 1 ]; then
            echo "Preferences.xml was updated with valid UUIDs."
        fi
    fi
}

update_device_uuid() {
    local plex_dir="/config/Library/Application Support/Plex Media Server"
    if [ -f "$plex_dir/Preferences.xml" ]; then
        local machine_id
        machine_id=$(sed -n 's/.*[[:space:]]MachineIdentifier="\([^"]*\)".*/\1/p' "$plex_dir/Preferences.xml" || true)
        if [ -n "$machine_id" ]; then
            local formatted_uuid="$machine_id"
            if [ ${#machine_id} -eq 32 ]; then
                formatted_uuid="${machine_id:0:8}-${machine_id:8:4}-${machine_id:12:4}-${machine_id:16:4}-${machine_id:20:12}"
            fi
            echo "Updating PostgreSQL devices table with MachineIdentifier: $formatted_uuid"
            psql -c "UPDATE ${PLEX_PG_SCHEMA:-plex}.devices SET identifier = '$formatted_uuid' WHERE id = 1;" >/dev/null 2>&1 || true
        fi
    fi
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
        local machine_id anonymous_id processed_id
        machine_id=$(generate_uuid)
        anonymous_id=$(generate_uuid)
        processed_id=$(sha1_hex "$machine_id")
        cat > "$plex_dir/Preferences.xml" << PREFEOF
<?xml version="1.0" encoding="utf-8"?>
<Preferences OldestPreviousVersion="1.43.0.10492-121068a07" MachineIdentifier="${machine_id}" ProcessedMachineIdentifier="${processed_id}" AnonymousMachineIdentifier="${anonymous_id}" AcceptedEULA="1" PublishServerOnPlexOnline="0"/>
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
    wait_for_postgres
    init_schema

    # Run migration if source SQLite DB exists (mounted via -v)
    if [[ -f "$MIGRATE_LIB" ]] && [[ -f "$SQLITE_DB" ]]; then
        echo "Checking for data migration..."
        check_and_migrate || true
    fi

    ensure_plex_temp_dir
    init_plex_directories
    update_preferences_uuid
    update_device_uuid
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
