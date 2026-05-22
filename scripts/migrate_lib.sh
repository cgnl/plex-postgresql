#!/bin/bash
# Shared migration library for plex-postgresql
# Source this file from install scripts or docker-entrypoint.sh
#
# Required variables before sourcing:
#   SQLITE_DB - path to Plex SQLite database
#   PG_HOST, PG_PORT, PG_DATABASE, PG_USER, PG_SCHEMA - PostgreSQL config
#   SHIM_DIR - path to plex-postgresql directory (for schema file)
#
# Optional:
#   PLEX_PG_PASSWORD - PostgreSQL password (default: plex)
#   MIGRATION_INTERACTIVE - set to "0" to skip prompts (default: 1)

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

MIGRATION_INTERACTIVE="${MIGRATION_INTERACTIVE:-1}"

check_and_migrate() {
    # Check if SQLite database exists and has data
    if [[ ! -f "$SQLITE_DB" ]]; then
        echo -e "${BLUE}No existing Plex database found. Fresh install.${NC}"
        return 0
    fi

    local sqlite_count=$(sqlite3 "$SQLITE_DB" "SELECT COUNT(*) FROM metadata_items;" 2>/dev/null || echo "0")

    if [[ "$sqlite_count" -eq 0 ]]; then
        echo -e "${BLUE}Existing Plex database is empty. No migration needed.${NC}"
        return 0
    fi

    echo -e "${YELLOW}========================================${NC}"
    echo -e "${YELLOW}  EXISTING PLEX DATA DETECTED${NC}"
    echo -e "${YELLOW}========================================${NC}"
    echo ""
    echo "Found SQLite database with $sqlite_count items:"
    echo "  $SQLITE_DB"
    echo ""

    # Show breakdown
    echo "Content breakdown:"
    sqlite3 "$SQLITE_DB" "
        SELECT
            CASE metadata_type
                WHEN 1 THEN '  Movies'
                WHEN 2 THEN '  TV Shows'
                WHEN 3 THEN '  Seasons'
                WHEN 4 THEN '  Episodes'
                ELSE '  Other'
            END as type,
            COUNT(*) as count
        FROM metadata_items
        GROUP BY metadata_type
        ORDER BY metadata_type;
    " 2>/dev/null || true
    echo ""

    # Check PostgreSQL connection
    export PGHOST="$PG_HOST"
    export PGPORT="$PG_PORT"
    export PGDATABASE="$PG_DATABASE"
    export PGUSER="$PG_USER"
    export PGPASSWORD="${PLEX_PG_PASSWORD:-plex}"

    if ! psql -c "SELECT 1" >/dev/null 2>&1; then
        echo -e "${RED}ERROR: Cannot connect to PostgreSQL at $PG_HOST:$PG_PORT${NC}"
        return 1
    fi

    # Check if PostgreSQL already has data (check multiple tables, not just metadata_items)
    local pg_count=$(psql -t -c "SELECT COUNT(*) FROM $PG_SCHEMA.metadata_items;" 2>/dev/null | tr -d ' ' || echo "0")
    local pg_sections=$(psql -t -c "SELECT COUNT(*) FROM $PG_SCHEMA.library_sections;" 2>/dev/null | tr -d ' ' || echo "0")
    local pg_accounts=$(psql -t -c "SELECT COUNT(*) FROM $PG_SCHEMA.accounts;" 2>/dev/null | tr -d ' ' || echo "0")
    local pg_has_data=0
    if [[ "$pg_count" -gt 0 ]] || [[ "$pg_sections" -gt 0 ]] || [[ "$pg_accounts" -gt 0 ]]; then
        pg_has_data=1
    fi

    if [[ "$pg_has_data" -eq 1 ]]; then
        echo -e "${YELLOW}PostgreSQL already has data (metadata_items=$pg_count, library_sections=$pg_sections, accounts=$pg_accounts).${NC}"

        if [[ "$MIGRATION_INTERACTIVE" == "1" ]]; then
            echo ""
            echo "Options:"
            echo "  1) Skip migration (keep existing PostgreSQL data)"
            echo "  2) Replace PostgreSQL data with SQLite data"
            echo "  3) Cancel"
            echo ""
            read -p "Choose [1/2/3]: " choice

            case $choice in
                1)
                    echo "Skipping migration."
                    return 0
                    ;;
                2)
                    echo "Will replace PostgreSQL data..."
                    ;;
                3|*)
                    echo "Cancelled."
                    return 1
                    ;;
            esac
        else
            echo "Non-interactive mode: skipping migration (PostgreSQL has data)"
            return 0
        fi
    else
        echo -e "${YELLOW}PostgreSQL database is empty.${NC}"

        if [[ "$MIGRATION_INTERACTIVE" == "1" ]]; then
            echo ""
            echo "Do you want to migrate your existing Plex data to PostgreSQL?"
            echo ""
            echo -e "  ${GREEN}Yes${NC} = Copy all data from SQLite to PostgreSQL"
            echo -e "  ${RED}No${NC}  = Start fresh (lose existing library data!)"
            echo ""
            read -p "Migrate data? [Y/n]: " migrate_choice

            if [[ "$migrate_choice" =~ ^[Nn] ]]; then
                echo -e "${YELLOW}Skipping migration.${NC}"
                return 0
            fi
        else
            echo "Non-interactive mode: auto-migrating to empty PostgreSQL"
        fi
    fi

    # Run migration
    echo ""
    echo -e "${GREEN}=== Starting Migration ===${NC}"
    echo ""

    migrate_sqlite_to_pg

    echo ""
    echo -e "${GREEN}=== Migration Complete ===${NC}"
}

migrate_sqlite_to_pg() {
    local schema="$PG_SCHEMA"
    local compat_file="${SHIM_DIR}/schema/pg_compat_functions.sql"
    if [[ ! -f "$compat_file" ]]; then
        compat_file="/usr/local/lib/plex-postgresql/pg_compat_functions.sql"
    fi

    # Ensure schema exists
    psql -c "CREATE SCHEMA IF NOT EXISTS $schema;" 2>/dev/null || true

    # Load schema if needed
    local table_count=$(psql -t -c "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '$schema';" 2>/dev/null | tr -d ' ')
    if [[ "$table_count" -lt 10 ]]; then
        echo "Loading PostgreSQL schema..."
        local schema_file="${SHIM_DIR}/schema/plex_schema.sql"
        if [[ -f "$schema_file" ]]; then
            psql -f "$schema_file" >/dev/null 2>&1 || true
        else
            # Docker location
            schema_file="/usr/local/lib/plex-postgresql/plex_schema.sql"
            if [[ -f "$schema_file" ]]; then
                psql -f "$schema_file" >/dev/null 2>&1 || true
            fi
        fi
    fi

    # Ensure PostgreSQL compatibility helper functions exist.
    if [[ -f "$compat_file" ]]; then
        psql -f "$compat_file" >/dev/null 2>&1 || true
    fi

    # Get tables from SQLite (exclude FTS and virtual tables)
    local tables=$(sqlite3 "$SQLITE_DB" ".tables" | tr -s ' ' '\n' | grep -v '^$' | grep -v 'fts' | grep -v 'spellfix' | sort)

    local migrated=0
    local failed=0
    local skipped=0

    for table in $tables; do
        local count=$(sqlite3 "$SQLITE_DB" "SELECT COUNT(*) FROM \"$table\";" 2>/dev/null || echo "0")

        if [[ "$count" -gt 0 ]]; then
            printf "  %-35s %8s rows... " "$table" "$count"

            # Get SQLite columns
            local sqlite_cols_raw=$(sqlite3 "$SQLITE_DB" "PRAGMA table_info(\"$table\");" | cut -d'|' -f2)

            if [[ -z "$sqlite_cols_raw" ]]; then
                echo -e "${RED}SKIP (no columns)${NC}"
                continue
            fi

            # Get PostgreSQL columns (exclude generated columns — COPY can't write to them)
            local pg_cols=$(psql -t -c "SELECT string_agg(column_name, ',') FROM information_schema.columns WHERE table_schema = '$schema' AND table_name = '$table' AND (is_generated = 'NEVER' OR is_generated IS NULL);" 2>/dev/null | tr -d ' ')

            if [[ -z "$pg_cols" ]]; then
                echo -e "${YELLOW}SKIP (no PG table)${NC}"
                ((skipped++))
                continue
            fi

            # Get column types from SQLite to detect BLOBs
            local col_types=$(sqlite3 "$SQLITE_DB" "PRAGMA table_info(\"$table\");" | cut -d'|' -f2,3)

            # Find common columns and build quoted lists
            # For BLOB columns, use hex() to convert binary to hex string
            # For timestamp columns (*_at), cast float to integer (SQLite stores as float)
            local sqlite_select=""
            local pg_cols_list=""
            for col in $sqlite_cols_raw; do
                if echo ",$pg_cols," | grep -q ",$col,"; then
                    # Check if this column is a BLOB
                    local col_type=$(echo "$col_types" | grep "^$col|" | cut -d'|' -f2)
                    local select_expr
                    if [[ "$col_type" == "BLOB" ]]; then
                        # Use hex() for BLOB columns, prefix with \x for PostgreSQL bytea
                        select_expr="CASE WHEN \"$col\" IS NOT NULL THEN '\\x' || hex(\"$col\") ELSE NULL END AS \"$col\""
                    elif [[ "$col" == *_at ]]; then
                        # Timestamp columns: cast to integer (SQLite stores as float, PG expects bigint)
                        select_expr="CAST(\"$col\" AS INTEGER) AS \"$col\""
                    else
                        select_expr="\"$col\""
                    fi

                    if [[ -z "$sqlite_select" ]]; then
                        sqlite_select="$select_expr"
                        pg_cols_list="\"$col\""
                    else
                        sqlite_select="$sqlite_select,$select_expr"
                        pg_cols_list="$pg_cols_list,\"$col\""
                    fi
                fi
            done

            if [[ -z "$sqlite_select" ]]; then
                echo -e "${RED}SKIP (no common cols)${NC}"
                continue
            fi

            # Use Python bridge for data transfer (avoids CSV truncation of large TEXT fields)
            # Python reads from SQLite directly and streams to PostgreSQL via COPY FROM STDIN
            # Disable ALL constraints: triggers, foreign keys, AND check constraints
            # Check constraints can't be disabled with DISABLE TRIGGER, so we drop them
            # temporarily and re-create after import
            psql -q -c "ALTER TABLE $schema.\"$table\" DISABLE TRIGGER ALL;" 2>/dev/null || true

            # Save and drop check constraints (COPY fails on check constraints for dirty data)
            local check_constraints=$(psql -t -c "
                SELECT conname || '|' || pg_get_constraintdef(oid)
                FROM pg_constraint
                WHERE conrelid = '$schema.\"$table\"'::regclass AND contype = 'c';" 2>/dev/null)
            while IFS='|' read -r cname cdef; do
                cname=$(echo "$cname" | xargs)
                [ -z "$cname" ] && continue
                psql -q -c "ALTER TABLE $schema.\"$table\" DROP CONSTRAINT \"$cname\";" 2>/dev/null || true
            done <<< "$check_constraints"

            psql -q -c "TRUNCATE $schema.\"$table\" CASCADE;" 2>/dev/null || true

            # Find migrate_table.py: check SHIM_DIR first, then script dir, then PATH
            local migrate_py="${SHIM_DIR:-}/migrate_table.py"
            if [ ! -f "$migrate_py" ]; then
                migrate_py="$(dirname "$0")/migrate_table.py"
            fi
            if [ ! -f "$migrate_py" ]; then
                migrate_py="/usr/local/lib/plex-postgresql/migrate_table.py"
            fi

            local log_dir="${LOG_DIR:-/var/log/plex-postgresql}"
            mkdir -p "$log_dir" 2>/dev/null || true

            if python3 "$migrate_py" \
                "$SQLITE_DB" "$table" "$sqlite_select" "$pg_cols_list" "$schema" 2>>"$log_dir/migration_errors.log"; then
                echo -e "${GREEN}OK${NC}"
                ((migrated++))
            else
                echo -e "${RED}FAIL${NC}"
                ((failed++))
            fi

            # Restore check constraints and triggers
            while IFS='|' read -r cname cdef; do
                cname=$(echo "$cname" | xargs)
                [ -z "$cname" ] && continue
                psql -q -c "ALTER TABLE $schema.\"$table\" ADD CONSTRAINT \"$cname\" $cdef NOT VALID;" 2>/dev/null || true
            done <<< "$check_constraints"
            psql -q -c "ALTER TABLE $schema.\"$table\" ENABLE TRIGGER ALL;" 2>/dev/null || true
        fi
    done

    echo ""
    echo "Updating sequences..."
    psql -q -c "
        SELECT setval(pg_get_serial_sequence('$schema.metadata_items', 'id'), COALESCE((SELECT MAX(id) FROM $schema.metadata_items), 1));
        SELECT setval(pg_get_serial_sequence('$schema.media_items', 'id'), COALESCE((SELECT MAX(id) FROM $schema.media_items), 1));
        SELECT setval(pg_get_serial_sequence('$schema.media_parts', 'id'), COALESCE((SELECT MAX(id) FROM $schema.media_parts), 1));
        SELECT setval(pg_get_serial_sequence('$schema.tags', 'id'), COALESCE((SELECT MAX(id) FROM $schema.tags), 1));
    " >/dev/null 2>&1 || true

    echo ""
    echo "Migration summary:"
    echo "  Tables migrated: $migrated"
    echo "  Tables skipped:  $skipped"
    echo "  Tables failed:   $failed"

    local pg_total=$(psql -t -c "SELECT COUNT(*) FROM $schema.metadata_items;" 2>/dev/null | tr -d ' ')
    echo "  Total items in PostgreSQL: $pg_total"

    # Always clear Flags.dat after migration — it contains stale SQLite
    # state that causes "Invalid uuid length" segfaults on first PG startup.
    # Also probe a path derived from $SQLITE_DB (with symlinks resolved) so
    # the clear works when the migrate Job mounts source config at a
    # non-/config prefix (k8s / CI flows).
    local flags_dat source_root="" sqlite_real
    if [[ -n "${SQLITE_DB:-}" ]]; then
        sqlite_real="$(readlink -f "$SQLITE_DB" 2>/dev/null || echo "$SQLITE_DB")"
        source_root="$(dirname "$sqlite_real" | sed 's|/Plug-in Support/Databases$||')"
    fi
    for flags_dat in \
        "/config/Library/Application Support/Plex Media Server/Cache/Flags.dat" \
        "${PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR:-}/Plex Media Server/Cache/Flags.dat" \
        "${source_root}/Cache/Flags.dat"; do
        if [[ -n "$flags_dat" && -f "$flags_dat" ]]; then
            rm -f "$flags_dat"
            echo "  Cleared Flags.dat (stale SQLite cache): $flags_dat"
            break
        fi
    done

    # Verify JSON integrity in extra_data columns (catches truncation bugs)
    echo ""
    echo "Verifying data integrity..."
    local json_tables="media_parts media_items metadata_items metadata_item_settings tags"
    for jtable in $json_tables; do
        local invalid=$(psql -t -c "
            SELECT count(*) FROM $schema.\"$jtable\"
            WHERE extra_data IS NOT NULL AND extra_data LIKE '{%'
              AND extra_data !~ '}\s*$';" 2>/dev/null | tr -d ' ')
        if [[ "$invalid" -gt 0 ]]; then
            echo -e "  ${RED}WARNING: $jtable has $invalid rows with truncated extra_data${NC}"
        fi
    done
    echo -e "  ${GREEN}Data integrity check complete${NC}"

    # =========================================================================
    # Migrate blobs.db (separate database with binary thumbnail data)
    # =========================================================================
    local blobs_db="${SQLITE_DB%.db}.blobs.db"
    if [[ -f "$blobs_db" ]]; then
        echo ""
        echo "Migrating blobs.db (thumbnails/artwork)..."

        local blob_count=$(sqlite3 "$blobs_db" "SELECT COUNT(*) FROM blobs;" 2>/dev/null || echo "0")

        if [[ "$blob_count" -gt 0 ]]; then
            echo "  Found $blob_count blobs to migrate"

            # Export metadata (without blob column) via CSV
            sqlite3 -csv "$blobs_db" "SELECT id, linked_type, linked_id, linked_guid, created_at, blob_type FROM blobs;" > /tmp/blobs_meta.csv 2>/dev/null

            # Import metadata
            psql -q -c "TRUNCATE $schema.blobs CASCADE;" 2>/dev/null || true
            psql -q -c "\\copy $schema.blobs(id, linked_type, linked_id, linked_guid, created_at, blob_type) FROM '/tmp/blobs_meta.csv' CSV" 2>/dev/null || true

            # Migrate blob data using hex encoding (CSV corrupts binary data)
            local batch=100
            local offset=0
            local blob_migrated=0

            while true; do
                # Export batch as SQL UPDATE statements with hex-encoded blobs
                sqlite3 "$blobs_db" "SELECT 'UPDATE $schema.blobs SET blob = decode(''' || hex(blob) || ''', ''hex'') WHERE id = ' || id || ';' FROM blobs WHERE blob IS NOT NULL LIMIT $batch OFFSET $offset;" > /tmp/blob_batch.sql 2>/dev/null

                # Check if batch is empty
                if [[ ! -s /tmp/blob_batch.sql ]]; then
                    break
                fi

                # Execute batch
                psql -f /tmp/blob_batch.sql -q 2>/dev/null || true

                offset=$((offset + batch))
                blob_migrated=$((blob_migrated + batch))
                if [[ $blob_migrated -le $blob_count ]]; then
                    printf "    Progress: %d / %d\r" "$blob_migrated" "$blob_count"
                fi
            done
            echo ""

            # Update sequence
            psql -q -c "SELECT setval('$schema.blobs_id_seq', COALESCE((SELECT MAX(id) FROM $schema.blobs), 1));" 2>/dev/null || true

            # Verify
            local pg_blob_count=$(psql -t -c "SELECT COUNT(*) FROM $schema.blobs;" 2>/dev/null | tr -d ' ')
            echo -e "  ${GREEN}Done: $pg_blob_count blobs in PostgreSQL${NC}"

            # Cleanup
            rm -f /tmp/blobs_meta.csv /tmp/blob_batch.sql
        else
            echo "  No blobs to migrate"
        fi
    fi
}
