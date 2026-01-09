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

    # Check if PostgreSQL already has data
    local pg_count=$(psql -t -c "SELECT COUNT(*) FROM $PG_SCHEMA.metadata_items;" 2>/dev/null | tr -d ' ' || echo "0")

    if [[ "$pg_count" -gt 0 ]]; then
        echo -e "${YELLOW}PostgreSQL already has $pg_count items.${NC}"

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

            # Get PostgreSQL columns
            local pg_cols=$(psql -t -c "SELECT string_agg(column_name, ',') FROM information_schema.columns WHERE table_schema = '$schema' AND table_name = '$table';" 2>/dev/null | tr -d ' ')

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

            # Export to CSV with quoted columns (handles reserved words like 'index')
            sqlite3 -header -csv "$SQLITE_DB" "SELECT $sqlite_select FROM \"$table\";" > "/tmp/plex_migrate_$table.csv" 2>/dev/null

            if [[ -s "/tmp/plex_migrate_$table.csv" ]]; then
                # Disable triggers for faster import
                psql -q -c "ALTER TABLE $schema.\"$table\" DISABLE TRIGGER ALL;" 2>/dev/null || true

                # Truncate existing data (faster than DELETE, handles FK)
                psql -q -c "TRUNCATE $schema.\"$table\" CASCADE;" 2>/dev/null || true

                # Import with quoted columns (handles reserved words like 'default')
                if psql -q -c "\\copy $schema.\"$table\"($pg_cols_list) FROM '/tmp/plex_migrate_$table.csv' WITH CSV HEADER" 2>/dev/null; then
                    echo -e "${GREEN}OK${NC}"
                    ((migrated++))
                else
                    echo -e "${RED}FAIL${NC}"
                    ((failed++))
                fi

                # Re-enable triggers
                psql -q -c "ALTER TABLE $schema.\"$table\" ENABLE TRIGGER ALL;" 2>/dev/null || true
            else
                echo -e "${YELLOW}EMPTY${NC}"
            fi

            rm -f "/tmp/plex_migrate_$table.csv"
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
}
