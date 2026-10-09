#!/bin/bash
# Export PostgreSQL data, not a certified native Plex rollback database.
# Usage: migrate_pg_to_sqlite.sh [--yes] [--native-rollback]
# Stop any consumers of the destination files before replacing them.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
export PGHOST="${PLEX_PG_HOST:-${PGHOST:-localhost}}"
export PGPORT="${PLEX_PG_PORT:-${PGPORT:-5432}}"
export PGDATABASE="${PLEX_PG_DATABASE:-${PGDATABASE:-plex}}"
export PGUSER="${PLEX_PG_USER:-${PGUSER:-plex}}"
export PGPASSWORD="${PLEX_PG_PASSWORD-${PGPASSWORD-plex}}"

for cmd in psql python3; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "ERROR: '$cmd' not found." >&2
        exit 1
    fi
done

exec python3 "$SCRIPT_DIR/export_pg_to_sqlite.py" \
    --schema "${PLEX_PG_SCHEMA:-plex}" \
    --output-dir "${OUTPUT_DIR:-$(pwd)}" \
    --sqlite-schema "$SCRIPT_DIR/../schema/sqlite_schema.sql" "$@"
