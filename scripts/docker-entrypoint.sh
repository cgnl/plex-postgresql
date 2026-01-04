#!/bin/bash
# Docker entrypoint for plex-postgresql
# Initializes PostgreSQL schema before Plex starts

set -e

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

    psql -c "CREATE SCHEMA IF NOT EXISTS $schema;" 2>/dev/null || true

    local table_count=$(psql -t -c "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '$schema';" 2>/dev/null | tr -d ' ')

    if [ "$table_count" -gt "0" ] 2>/dev/null; then
        echo "PostgreSQL schema '$schema' ready with $table_count tables"
    else
        echo "PostgreSQL schema '$schema' is empty, loading schema..."
        if [ -f "$schema_file" ]; then
            echo "Loading schema from $schema_file..."
            if psql -f "$schema_file" 2>&1; then
                local new_count=$(psql -t -c "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '$schema';" 2>/dev/null | tr -d ' ')
                echo "Schema loaded successfully! $new_count tables created."
            else
                echo "WARNING: Schema load had errors, continuing anyway..."
            fi
        else
            echo "WARNING: Schema file $schema_file not found!"
        fi
    fi
}

# Pre-initialize a single SQLite database
init_single_sqlite_db() {
    local db_file="$1"
    local schema_file="$2"
    local db_name
    db_name=$(basename "$db_file")

    if [ ! -f "$db_file" ]; then
        echo "Pre-initializing SQLite database $db_name..."
        if [ -f "$schema_file" ]; then
            # Ignore errors from virtual tables (spellfix1, fts4, rtree)
            sqlite3 "$db_file" < "$schema_file" 2>&1 || true
            sqlite3 "$db_file" "INSERT OR IGNORE INTO schema_migrations (version) VALUES ('pg_adapter_1.0.0');" 2>/dev/null || true
            echo "SQLite database $db_name initialized"
        else
            echo "WARNING: Schema file not found: $schema_file"
        fi
        chown abc:abc "$db_file" 2>/dev/null || true
    else
        # Database exists, ensure it has the min_version column
        if ! sqlite3 "$db_file" "SELECT min_version FROM schema_migrations LIMIT 1" >/dev/null 2>&1; then
            echo "Adding min_version column to $db_name..."
            sqlite3 "$db_file" "ALTER TABLE schema_migrations ADD COLUMN min_version TEXT;" 2>/dev/null || true
        fi
    fi
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

# Modify s6 run script to inject LD_PRELOAD
setup_plex_shim() {
    local shim_path="/usr/local/lib/plex-postgresql/db_interpose_pg.so"
    local s6_run="/etc/s6-overlay/s6-rc.d/svc-plex/run"

    if [ -f "$shim_path" ] && [ -f "$s6_run" ]; then
        # Check if we've already modified it
        if ! grep -q "LD_PRELOAD=" "$s6_run"; then
            echo "Modifying s6 run script to inject PostgreSQL shim..."

            # Insert LD_PRELOAD and LD_LIBRARY_PATH exports after the shebang line
            sed -i '2i\
# PostgreSQL shim injection\
export LD_LIBRARY_PATH="/usr/local/lib/plex-postgresql:/usr/lib/plexmediaserver/lib:$LD_LIBRARY_PATH"\
export LD_PRELOAD="/usr/local/lib/plex-postgresql/db_interpose_pg.so"' "$s6_run"

            echo "s6 run script modified for PostgreSQL shim"
            cat "$s6_run"
        else
            echo "s6 run script already configured for PostgreSQL shim"
        fi
    else
        echo "Warning: shim or s6 run script not found"
    fi
}

# Main
echo "=== plex-postgresql entrypoint ==="
echo "PostgreSQL: ${PLEX_PG_USER}@${PLEX_PG_HOST}:${PLEX_PG_PORT}/${PLEX_PG_DATABASE}"

if [ -n "$PLEX_PG_HOST" ]; then
    wait_for_postgres
    init_schema
    init_sqlite_schema
    setup_plex_shim
else
    echo "PLEX_PG_HOST not set, skipping PostgreSQL initialization"
fi

echo "Starting Plex Media Server..."
# Execute the original entrypoint WITHOUT LD_PRELOAD (wrapper handles it)
exec /init "$@"
