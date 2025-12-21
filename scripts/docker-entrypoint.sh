#!/bin/bash
# Docker entrypoint for plex-postgresql
# Initializes PostgreSQL schema before Plex starts

set -e

# Wait for PostgreSQL to be ready
wait_for_postgres() {
    echo "Waiting for PostgreSQL at ${PLEX_PG_HOST}:${PLEX_PG_PORT}..."

    # Build connection string for libpq
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

    # Check table count
    local table_count=$(psql -t -c "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '$schema';" 2>/dev/null | tr -d ' ')

    if [ "$table_count" -gt "0" ] 2>/dev/null; then
        echo "PostgreSQL schema ready with $table_count tables"
    else
        echo "PostgreSQL schema not initialized (PostgreSQL will init from mounted schema)"
    fi
}

# Main
echo "=== plex-postgresql entrypoint ==="
echo "PostgreSQL: ${PLEX_PG_USER}@${PLEX_PG_HOST}:${PLEX_PG_PORT}/${PLEX_PG_DATABASE}"

if [ -n "$PLEX_PG_HOST" ]; then
    wait_for_postgres
    init_schema
else
    echo "PLEX_PG_HOST not set, skipping PostgreSQL initialization"
fi

echo "Starting Plex Media Server..."
# Execute the original entrypoint
exec /init "$@"
