#!/bin/bash
# Migrate Plex SQLite database to PostgreSQL

set -e

# Configuration
SQLITE_DB="$HOME/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db"
PG_HOST="${PLEX_PG_HOST:-localhost}"
PG_PORT="${PLEX_PG_PORT:-5432}"
PG_DATABASE="${PLEX_PG_DATABASE:-plex}"
PG_USER="${PLEX_PG_USER:-plex}"
PG_SCHEMA="${PLEX_PG_SCHEMA:-plex}"

PSQL="psql -h $PG_HOST -p $PG_PORT -U $PG_USER -d $PG_DATABASE"
# migrate_table.py streams through plain psql; point it at the same database.
export PGHOST="$PG_HOST" PGPORT="$PG_PORT" PGUSER="$PG_USER" PGDATABASE="$PG_DATABASE"

echo "=== Plex SQLite to PostgreSQL Migration ==="
echo "SQLite: $SQLITE_DB"
echo "PostgreSQL: $PG_USER@$PG_HOST:$PG_PORT/$PG_DATABASE (schema: $PG_SCHEMA)"
echo ""

# Check if SQLite database exists
if [[ ! -f "$SQLITE_DB" ]]; then
    echo "ERROR: SQLite database not found: $SQLITE_DB"
    exit 1
fi

# Check PostgreSQL connection
if ! $PSQL -c "SELECT 1" >/dev/null 2>&1; then
    echo "ERROR: Cannot connect to PostgreSQL"
    exit 1
fi

echo "Step 1: Creating schema..."
$PSQL -c "CREATE SCHEMA IF NOT EXISTS $PG_SCHEMA;"
$PSQL -c "CREATE EXTENSION IF NOT EXISTS pg_trgm;" 2>/dev/null || true

echo "Step 2: Applying schema..."
$PSQL -f "$(dirname "$0")/../schema/plex_schema.sql"

echo "Step 2b: Applying PostgreSQL compatibility functions..."
$PSQL -f "$(dirname "$0")/../schema/pg_compat_functions.sql"

echo "Step 3: Snapshotting the SQLite sources..."

# Every table must come from one consistent point in time, and the live files
# are never read again. The snapshot refuses sources that change underneath it.
SNAPSHOT_DIR=$(mktemp -d "${TMPDIR:-/tmp}/plex-migration-source.XXXXXXXX")
trap 'rm -rf "$SNAPSHOT_DIR"' EXIT
if ! bash -c 'source "$1"; snapshot_sqlite_sources "$2" "$3"' sqlite-source-snapshot \
    "$(dirname "$0")/migrate_lib.sh" "$SQLITE_DB" "$SNAPSHOT_DIR"; then
    echo "ERROR: Could not snapshot $SQLITE_DB; stop Plex and retry (raise MIGRATION_SNAPSHOT_TIMEOUT for large libraries)"
    exit 1
fi
SOURCE_DB="$SNAPSHOT_DIR/library.db"
export MIGRATION_SQLITE_FROZEN=1

echo "Step 4: Exporting tables from SQLite..."

# Get list of tables
TABLES=$(sqlite3 "$SOURCE_DB" ".tables" | tr -s ' ' '\n' | grep -v '^$' | sort)
MIGRATE_LOG="${LOG_DIR:-/tmp}/migration_errors.log"
mkdir -p "$(dirname "$MIGRATE_LOG")" 2>/dev/null || true
FAILED_TABLES=()

for TABLE in $TABLES; do
    echo "  Migrating: $TABLE"

    # Get column names
    COLUMNS=$(sqlite3 "$SOURCE_DB" "PRAGMA table_info($TABLE);" | cut -d'|' -f2 | tr '\n' ',' | sed 's/,$//')

    # Use Python bridge for data transfer (avoids CSV truncation of large TEXT fields)
    $PSQL -c "TRUNCATE $PG_SCHEMA.$TABLE CASCADE;" 2>/dev/null || true
    if ! python3 "$(dirname "$0")/migrate_table.py" \
        "$SOURCE_DB" "$TABLE" "*" "$COLUMNS" "$PG_SCHEMA" 2>>"$MIGRATE_LOG"; then
        FAILED_TABLES+=("$TABLE")
    fi
done

if ((${#FAILED_TABLES[@]})); then
    echo ""
    echo "ERROR: ${#FAILED_TABLES[@]} table(s) failed to migrate: ${FAILED_TABLES[*]}"
    echo "PostgreSQL is incomplete; do not start Plex against it. Details: $MIGRATE_LOG"
    exit 1
fi

echo ""
echo "Step 5: Updating sequences..."
$PSQL -c "
SELECT setval(pg_get_serial_sequence('$PG_SCHEMA.metadata_items', 'id'),
       COALESCE((SELECT MAX(id) FROM $PG_SCHEMA.metadata_items), 1));
" 2>/dev/null || true

# ============================================================================
# Step 6: Migrate blobs from blobs.db (binary data requires special handling)
# ============================================================================
echo ""
echo "Step 6: Migrating blobs (binary data)..."

BLOBS_DB="$SNAPSHOT_DIR/library.blobs.db"
if [[ -f "$BLOBS_DB" ]]; then
    echo "  Found: ${SQLITE_DB%.db}.blobs.db"

    # Get blob count
    BLOB_COUNT=$(sqlite3 "$BLOBS_DB" "SELECT count(*) FROM blobs;")
    echo "  Blobs to migrate: $BLOB_COUNT"

    if [[ "$BLOB_COUNT" -gt 0 ]]; then
        # Export metadata (without blob column) via CSV
        echo "  Exporting blob metadata..."
        sqlite3 -csv "$BLOBS_DB" "SELECT id, linked_type, linked_id, linked_guid, created_at, blob_type FROM blobs;" > /tmp/blobs_meta.csv

        # Import metadata
        echo "  Importing blob metadata..."
        $PSQL -c "TRUNCATE $PG_SCHEMA.blobs;" 2>/dev/null || true
        $PSQL -c "\\copy $PG_SCHEMA.blobs(id, linked_type, linked_id, linked_guid, created_at, blob_type) FROM '/tmp/blobs_meta.csv' CSV" 2>/dev/null

        # Migrate blob data using hex encoding (CSV corrupts binary data)
        echo "  Migrating blob data (this may take a while)..."
        BATCH=100
        OFFSET=0
        MIGRATED=0

        while true; do
            # Export batch as SQL UPDATE statements with hex-encoded blobs
            sqlite3 "$BLOBS_DB" "SELECT 'UPDATE $PG_SCHEMA.blobs SET blob = decode(''' || hex(blob) || ''', ''hex'') WHERE id = ' || id || ';' FROM blobs WHERE blob IS NOT NULL LIMIT $BATCH OFFSET $OFFSET;" > /tmp/blob_batch.sql

            # Check if batch is empty
            if [[ ! -s /tmp/blob_batch.sql ]]; then
                break
            fi

            # Execute batch
            $PSQL -f /tmp/blob_batch.sql -q 2>/dev/null

            OFFSET=$((OFFSET + BATCH))
            MIGRATED=$((MIGRATED + BATCH))
            if [[ $MIGRATED -le $BLOB_COUNT ]]; then
                echo "    Progress: $MIGRATED / $BLOB_COUNT"
            fi
        done

        # Update sequence
        $PSQL -c "SELECT setval('$PG_SCHEMA.blobs_id_seq', COALESCE((SELECT MAX(id) FROM $PG_SCHEMA.blobs), 1));" 2>/dev/null || true

        # Verify
        PG_BLOB_COUNT=$($PSQL -t -c "SELECT count(*) FROM $PG_SCHEMA.blobs;" 2>/dev/null | tr -d ' ')
        echo "  Done: $PG_BLOB_COUNT blobs in PostgreSQL"

        # Cleanup
        rm -f /tmp/blobs_meta.csv /tmp/blob_batch.sql
    fi
else
    echo "  No blobs.db found at: ${SQLITE_DB%.db}.blobs.db"
    echo "  Skipping blob migration"
fi

echo ""
echo "=== Migration Complete ==="
echo ""
echo "Verify with:"
echo "  $PSQL -c 'SELECT COUNT(*) FROM $PG_SCHEMA.metadata_items;'"
echo "  $PSQL -c 'SELECT COUNT(*) FROM $PG_SCHEMA.blobs;'"
