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

migration_psql() {
    psql -X -v ON_ERROR_STOP=1 "$@"
}

validate_migration_schema() {
    if [[ ! "$PG_SCHEMA" =~ ^[a-z_][a-z0-9_]*$ || ${#PG_SCHEMA} -gt 63 ]]; then
        echo "ERROR: PG_SCHEMA must be a lowercase SQL identifier" >&2
        return 1
    fi
}

load_pg_schema_file() (
    local schema_file="$1" types_file="${2:-}" server_version temporary trgm_schema
    [[ "$PG_SCHEMA" == plex ]] || { echo "ERROR: Bundled schema requires PG_SCHEMA=plex" >&2; return 1; }
    server_version=$(migration_psql -tA -c 'SHOW server_version_num;') || return 1
    migration_psql -q -c 'CREATE SCHEMA IF NOT EXISTS plex; CREATE EXTENSION IF NOT EXISTS pg_trgm WITH SCHEMA plex;' || return 1
    trgm_schema=$(migration_psql -tA -c "SELECT quote_ident(n.nspname) FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace WHERE e.extname = 'pg_trgm';") || return 1
    temporary=$(mktemp "${TMPDIR:-/tmp}/plex-schema.XXXXXX") || return 1
    trap 'rm -f "$temporary"' EXIT
    python3 - "$schema_file" "$types_file" "$server_version" "$trgm_schema" > "$temporary" <<'PY'
import pathlib
import re
import sys

schema_file, types_file, version, trgm_schema = sys.argv[1:]
source = pathlib.Path(schema_file).read_text()
source = re.sub(r'ALTER TABLE ONLY plex\.test\s+ADD CONSTRAINT test_pkey PRIMARY KEY \(id\);', '', source)
if types_file:
    source = re.sub(r'ALTER TABLE ONLY plex\.sqlite_column_types\s+ADD CONSTRAINT sqlite_column_types_pkey PRIMARY KEY \(table_name, column_name\);', '', source)
    source += '\n' + pathlib.Path(types_file).read_text()
source = source.replace('CREATE SCHEMA plex;', 'CREATE SCHEMA IF NOT EXISTS plex;')
source = source.replace('plex.gin_trgm_ops', trgm_schema + '.gin_trgm_ops').replace('plex.gist_trgm_ops', trgm_schema + '.gist_trgm_ops')
if int(version) < 170000:
    source = source.replace('SET transaction_timeout = 0;', '')
print(source)
PY
    [[ "$?" == 0 ]] || return 1
    migration_psql -1 -f "$temporary"
)

protect_sqlite_sources() {
    local shadow_dir="$1" source_file shadow_file
    [[ -n "${SQLITE_DB:-}" ]] || return 0
    for source_file in "${SQLITE_DB:-}" "${SQLITE_DB:-}.blobs.db" "${SQLITE_DB%.db}.blobs.db"; do
        [[ -f "$source_file" ]] || continue
        for shadow_file in "$shadow_dir/com.plexapp.plugins.library.db" "$shadow_dir/com.plexapp.plugins.library.blobs.db"; do
            if [[ "$source_file" -ef "$shadow_file" ]]; then
                echo "ERROR: SQLite source is the shadow destination: $source_file. Mount a separate source/backup before startup." >&2
                return 1
            fi
        done
    done
}

is_shadow_database() {
    [[ -f "$1" && -f "${1}.plex-pg-shadow" ]] || return 1
    python3 - "$1" <<'PY'
import pathlib
import sqlite3
import sys

path = pathlib.Path(sys.argv[1])
try:
    connection = sqlite3.connect(path.resolve().as_uri() + '?mode=ro', uri=True)
    identity = connection.execute('SELECT identity FROM plex_pg_shadow_marker').fetchall()
    expected = pathlib.Path(str(path) + '.plex-pg-shadow').read_text().strip()
    connection.close()
except (OSError, sqlite3.Error):
    sys.exit(1)
sys.exit(0 if identity == [(expected,)] and expected else 1)
PY
}

protect_shadow_destinations() {
    local shadow_dir="$1" db_file
    protect_sqlite_sources "$shadow_dir" || return 1
    for db_file in "$shadow_dir/com.plexapp.plugins.library.db" "$shadow_dir/com.plexapp.plugins.library.blobs.db"; do
        if [[ -e "$db_file" ]] && ! is_shadow_database "$db_file"; then
            echo "ERROR: Unmarked SQLite destination $db_file; preserve it as a separate source/backup before startup." >&2
            return 1
        fi
    done
}

validate_pg_schema_file() {
    local schema_file="$1" tables
    tables=$(migration_psql -tA -c "SELECT tablename FROM pg_tables WHERE schemaname = '$PG_SCHEMA';") || return 1
    python3 - "$schema_file" "$tables" <<'PY'
import pathlib
import re
import sys

schema_file, tables = sys.argv[1:]
required = set(re.findall(r'^CREATE TABLE plex\."?(\w+)"?\s*\(', pathlib.Path(schema_file).read_text(), re.MULTILINE))
if not required:
    raise RuntimeError('Bundled PostgreSQL schema has no required tables')
missing = required - set(tables.splitlines())
if missing:
    raise RuntimeError('Incomplete PostgreSQL schema: ' + ', '.join(sorted(missing)))
PY
}

destination_has_data() {
    local table tables count accounts
    count=$(migration_psql -tA -c "SELECT (SELECT count(*) FROM pg_constraint c JOIN pg_namespace n ON n.oid = c.connamespace WHERE n.nspname = '$PG_SCHEMA' AND NOT c.convalidated) + (SELECT count(*) FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = '$PG_SCHEMA' AND t.tgenabled NOT IN ('O','A'));") || return 2
    [[ "$count" == 0 ]] || { echo "ERROR: Destination has unvalidated constraints or disabled triggers" >&2; return 2; }
    tables=$(migration_psql -tA -c "SELECT tablename FROM pg_tables WHERE schemaname = '$PG_SCHEMA' ORDER BY tablename;") || return 2
    for table in $tables; do
        case "$table" in
            schema_migrations|sqlite_column_types|maintenance_control) continue ;;
        esac
        count=$(migration_psql -tA -c "SELECT count(*) FROM $PG_SCHEMA.\"${table//\"/\"\"}\";") || return 2
        [[ "$count" =~ ^[0-9]+$ ]] || return 2
        [[ "$count" -gt 0 ]] || continue
        if [[ "$table" == accounts && "$count" == 1 ]]; then
            accounts=$(migration_psql -tA -c "SELECT count(*) FROM $PG_SCHEMA.accounts a WHERE to_jsonb(a) = jsonb_build_object('id', 1, 'name', 'Administrator', 'created_at', 1289520473, 'updated_at', 1782210228, 'default_audio_language', '', 'default_subtitle_language', '', 'auto_select_subtitle', 1, 'auto_select_audio', 1) || (SELECT coalesce(jsonb_object_agg(key, value), '{}'::jsonb) FROM jsonb_each(to_jsonb(a)) WHERE key NOT IN ('id','name','created_at','updated_at','default_audio_language','default_subtitle_language','auto_select_subtitle','auto_select_audio') AND value = 'null'::jsonb);") || return 2
            [[ "$accounts" == 1 ]] && continue
        fi
        echo "Existing or partial PostgreSQL data in $table ($count rows); refusing implicit replacement." >&2
        return 0
    done
    return 1
}

sequence_sync_sql() {
    printf '%s\n' "
        DO \$do\$
        DECLARE rec record; maximum bigint; minimum bigint; initial bigint;
        BEGIN
            FOR rec IN
                SELECT table_schema, table_name, column_name,
                       pg_get_serial_sequence(format('%I.%I', table_schema, table_name), column_name) AS sequence_name
                FROM information_schema.columns
                WHERE table_schema = '$PG_SCHEMA'
                  AND (column_default LIKE 'nextval%' OR is_identity = 'YES')
            LOOP
                IF rec.sequence_name IS NULL THEN
                    RAISE EXCEPTION 'Missing sequence for %.%', rec.table_name, rec.column_name;
                END IF;
                SELECT seqmin, seqstart INTO minimum, initial FROM pg_sequence WHERE seqrelid = rec.sequence_name::regclass;
                EXECUTE format('SELECT max(%I) FROM %I.%I', rec.column_name, rec.table_schema, rec.table_name) INTO maximum;
                PERFORM setval(rec.sequence_name, greatest(coalesce(maximum, initial), minimum), maximum IS NOT NULL AND maximum >= minimum);
            END LOOP;
        END
        \$do\$;
    "
}

destination_guard_sql() {
    printf '%s\n' "
        DO \$guard\$
        DECLARE item record; row_count bigint; seed_count bigint;
        BEGIN
            FOR item IN SELECT tablename FROM pg_tables WHERE schemaname = '$PG_SCHEMA' ORDER BY tablename LOOP
                EXECUTE format('LOCK TABLE %I.%I IN ACCESS EXCLUSIVE MODE', '$PG_SCHEMA', item.tablename);
            END LOOP;
            FOR item IN SELECT tablename FROM pg_tables WHERE schemaname = '$PG_SCHEMA' ORDER BY tablename LOOP
                IF item.tablename IN ('schema_migrations', 'sqlite_column_types', 'maintenance_control') THEN CONTINUE; END IF;
                EXECUTE format('SELECT count(*) FROM %I.%I', '$PG_SCHEMA', item.tablename) INTO row_count;
                IF row_count = 0 THEN CONTINUE; END IF;
                IF item.tablename = 'accounts' AND row_count = 1 THEN
                    SELECT count(*) INTO seed_count FROM $PG_SCHEMA.accounts a
                    WHERE to_jsonb(a) = jsonb_build_object('id', 1, 'name', 'Administrator', 'created_at', 1289520473, 'updated_at', 1782210228, 'default_audio_language', '', 'default_subtitle_language', '', 'auto_select_subtitle', 1, 'auto_select_audio', 1) ||
                        (SELECT coalesce(jsonb_object_agg(key, value), '{}'::jsonb) FROM jsonb_each(to_jsonb(a)) WHERE key NOT IN ('id','name','created_at','updated_at','default_audio_language','default_subtitle_language','auto_select_subtitle','auto_select_audio') AND value = 'null'::jsonb);
                    IF seed_count = 1 THEN CONTINUE; END IF;
                END IF;
                RAISE EXCEPTION 'Destination changed or contains data: %', item.tablename;
            END LOOP;
        END \$guard\$;
    "
}

sync_all_sequences() {
    sequence_sync_sql | migration_psql -q
}

sync_shadow_migrations() {
    local db_file="$1" versions
    versions=$(migration_psql -tA -c "SELECT encode(convert_to(version, 'UTF8'), 'hex') FROM $PG_SCHEMA.schema_migrations ORDER BY version;") || return 1
    python3 - "$db_file" "$versions" <<'PY'
import sqlite3
import sys

database, encoded = sys.argv[1:]
versions = [bytes.fromhex(value).decode('utf-8') for value in encoded.splitlines()]
if len(set(versions)) != len(versions):
    raise RuntimeError('Duplicate PostgreSQL migration versions')
connection = sqlite3.connect(database)
with connection:
    connection.execute('DELETE FROM schema_migrations')
    connection.executemany('INSERT INTO schema_migrations(version) VALUES (?)', [(version,) for version in versions])
actual = [row[0] for row in connection.execute('SELECT version FROM schema_migrations ORDER BY version')]
if sorted(actual) != sorted(versions):
    raise RuntimeError('Shadow migration contents differ from PostgreSQL')
connection.close()
PY
}

build_shadow_database() (
    local db_file="$1" schema_file="$2" temporary
    [[ -f "$schema_file" ]] || { echo "ERROR: Missing shadow schema: $schema_file" >&2; return 1; }
    protect_sqlite_sources "$(dirname "$db_file")" || return 1
    if [[ -e "$db_file" ]] && ! is_shadow_database "$db_file"; then
        echo "ERROR: Unmarked SQLite destination $db_file; preserve it as a separate backup before rebuilding." >&2
        return 1
    fi
    temporary=$(mktemp "${db_file}.new.XXXXXX") || return 1
    trap 'rm -f "$temporary" "${temporary}-wal" "${temporary}-shm" "${temporary}.plex-pg-shadow"' EXIT
    python3 - "$temporary" "$schema_file" <<'PY'
import re
import sqlite3
import sys

database, schema_file = sys.argv[1:]
expected = sqlite3.connect(':memory:')
with open(schema_file) as schema:
    source = schema.read()
    for statement in re.findall(r'^CREATE TABLE .*?;', source, re.MULTILINE | re.DOTALL):
        expected.execute(statement)
actual = sqlite3.connect(database)
statement = ''
for line in source.splitlines(keepends=True):
    statement += line
    if not sqlite3.complete_statement(statement):
        continue
    virtual = re.match(r'^CREATE VIRTUAL TABLE (?:"?)(\w+)(?:"?) USING (spellfix1|fts4|rtree)\b', statement.strip())
    if virtual and virtual.group(2) == 'rtree':
        for suffix in ('node', 'rowid', 'parent'):
            backing_table = (virtual.group(1) + '_' + suffix).replace('"', '""')
            actual.execute(f'DROP TABLE IF EXISTS "{backing_table}"')
            expected.execute(f'DROP TABLE IF EXISTS "{backing_table}"')
        expected.executescript(statement)
    try:
        actual.executescript(statement)
    except sqlite3.OperationalError as error:
        virtual = re.match(r'^CREATE VIRTUAL TABLE (?:"?)(\w+)(?:"?) USING (spellfix1|fts4|rtree)\b', statement.strip())
        message = str(error)
        allowed = message in ('no such module: spellfix1', 'no such module: fts4', 'unknown tokenizer: collating')
        if virtual:
            table = virtual.group(1)
            allowed = allowed or (virtual.group(2) == 'fts4' and message == 'vtable constructor failed: ' + table and expected.execute('SELECT count(*) FROM sqlite_master WHERE name = ?', (table + '_segments',)).fetchone()[0] > 0)
        if not virtual or not allowed:
            raise
        print(f'Expected virtual-table absence: {error}', file=sys.stderr)
    statement = ''
if statement.strip():
    raise RuntimeError('Incomplete shadow schema statement')
for table in ('schema_migrations', 'metadata_items', 'media_items', 'media_parts', 'accounts', 'preferences', 'blobs'):
    if not expected.execute('SELECT 1 FROM sqlite_master WHERE name = ? AND type = ?', (table, 'table')).fetchone():
        raise RuntimeError(f'Required shadow schema absent: {table}')
for (table,) in expected.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'"):
    quoted = table.replace('"', '""')
    if list(actual.execute(f'PRAGMA table_info("{quoted}")')) != list(expected.execute(f'PRAGMA table_info("{quoted}")')):
        raise RuntimeError(f'Missing or incompatible shadow schema: {table}')
if actual.execute('PRAGMA quick_check').fetchall() != [('ok',)]:
    raise RuntimeError('Invalid shadow SQLite database')
actual.close()
expected.close()
PY
    [[ "$?" == 0 ]] || return 1
    sync_shadow_migrations "$temporary" || return 1
    seed_shadow_tables_from_pg "$temporary" "$(basename "$db_file")" || return 1
    sync_shadow_migrations "$temporary" || return 1
    python3 - "$temporary" <<'PY'
import pathlib
import sqlite3
import sys
import uuid

path = pathlib.Path(sys.argv[1])
identity = str(uuid.uuid4())
connection = sqlite3.connect(path)
with connection:
    connection.execute('CREATE TABLE plex_pg_shadow_marker(identity TEXT NOT NULL)')
    connection.execute('INSERT INTO plex_pg_shadow_marker VALUES (?)', (identity,))
connection.close()
pathlib.Path(str(path) + '.plex-pg-shadow').write_text(identity + '\n')
PY
    [[ "$?" == 0 ]] || return 1
    mv -f "$temporary" "$db_file" || return 1
    rm -f "${db_file}-wal" "${db_file}-shm" || return 1
    mv -f "${temporary}.plex-pg-shadow" "${db_file}.plex-pg-shadow" || return 1
)

check_and_migrate() {
    # Check if SQLite database exists and has data
    if [[ ! -f "$SQLITE_DB" ]]; then
        echo -e "${BLUE}No existing Plex database found. Fresh install.${NC}"
        return 0
    fi

    validate_migration_schema || return 1
    local sqlite_count
    sqlite_count=$(sqlite3 -readonly "$SQLITE_DB" "SELECT COUNT(*) FROM metadata_items;") || return 1
    [[ "$sqlite_count" =~ ^[0-9]+$ ]] || return 1

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
    " || return 1
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

    local destination_status
    if destination_has_data; then
        echo "ERROR: Remove the migration source mount to use an existing destination, or migrate into a separate empty database." >&2
        return 1
    else
        destination_status=$?
        [[ "$destination_status" == 1 ]] || return 1
    fi
    if [[ "$MIGRATION_INTERACTIVE" == "1" ]]; then
        local migrate_choice
        read -r -p "Migrate into the empty PostgreSQL destination? [Y/n]: " migrate_choice || return 1
        [[ ! "$migrate_choice" =~ ^[Nn] ]] || return 1
    fi

    # Run migration
    echo ""
    echo -e "${GREEN}=== Starting Migration ===${NC}"
    echo ""

    migrate_sqlite_to_pg || return 1

    echo ""
    echo -e "${GREEN}=== Migration Complete ===${NC}"
}

migrate_sqlite_to_pg() (
    set -o pipefail
    validate_migration_schema || return 1
    local schema="$PG_SCHEMA"
    migration_psql -q -c "SELECT id FROM $schema.metadata_items LIMIT 0; SELECT version FROM $schema.schema_migrations LIMIT 0;" >/dev/null || return 1
    local destination_status
    if destination_has_data; then
        echo "ERROR: Refusing to import into a populated destination" >&2
        return 1
    else
        destination_status=$?
        [[ "$destination_status" == 1 ]] || return 1
    fi
    local staging="plex_import_$$_${RANDOM}" work_dir tables
    work_dir=$(mktemp -d "${TMPDIR:-/tmp}/plex-import.XXXXXX") || return 1
    trap 'status=$?; migration_psql -q -c "DROP SCHEMA IF EXISTS $staging CASCADE;" >&2 || status=1; rm -rf "$work_dir" || status=1; exit "$status"' EXIT
    local original_source="$SQLITE_DB"
    python3 - "$original_source" "$work_dir" <<'PY'
import pathlib
import sqlite3
import sys

source, destination = sys.argv[1:]
for original, snapshot in ((source, 'library.db'), (source[:-3] + '.blobs.db', 'library.blobs.db')):
    if not pathlib.Path(original).is_file():
        continue
    connection = sqlite3.connect(pathlib.Path(original).resolve().as_uri() + '?mode=ro', uri=True)
    backup = sqlite3.connect(str(pathlib.Path(destination) / snapshot))
    connection.backup(backup)
    if backup.execute('PRAGMA quick_check').fetchall() != [('ok',)]:
        raise RuntimeError(f'Invalid SQLite source: {original}')
    backup.close()
    connection.close()
PY
    [[ "$?" == 0 ]] || return 1
    SQLITE_DB="$work_dir/library.db"
    migration_psql -q -c "CREATE SCHEMA $staging;" || return 1
    tables=$(sqlite3 -readonly "$SQLITE_DB" "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '%fts%' AND name NOT LIKE '%spellfix%' AND sql NOT LIKE 'CREATE VIRTUAL TABLE%' ORDER BY name;") || return 1
    printf 'BEGIN;\n' > "$work_dir/activate.sql" || return 1
    destination_guard_sql >> "$work_dir/activate.sql" || return 1

    local migrated=0
    local failed=0
    local skipped=0

    for table in $tables; do
        [[ "$table" =~ ^[a-z_][a-z0-9_]*$ ]] || { echo "ERROR: Unsupported source table name: $table" >&2; return 1; }
        case "$table" in schema_migrations|sqlite_column_types) continue ;; esac
        local count
        count=$(sqlite3 -readonly "$SQLITE_DB" "SELECT COUNT(*) FROM \"$table\";") || return 1
        [[ "$count" =~ ^[0-9]+$ ]] || return 1
        if [[ "$count" -ge 0 ]]; then
            printf "  %-35s %8s rows... " "$table" "$count"

            # Get SQLite columns
            local sqlite_cols_raw
            sqlite_cols_raw=$(sqlite3 -readonly "$SQLITE_DB" "PRAGMA table_info(\"$table\");" | cut -d'|' -f2) || return 1

            if [[ -z "$sqlite_cols_raw" ]]; then
                echo "ERROR: No source columns for $table" >&2
                return 1
            fi

            # Get PostgreSQL columns (exclude generated columns — COPY can't write to them)
            local pg_cols
            pg_cols=$(migration_psql -t -c "SELECT string_agg(column_name, ',') FROM information_schema.columns WHERE table_schema = '$schema' AND table_name = '$table' AND (is_generated = 'NEVER' OR is_generated IS NULL);" | tr -d ' ') || return 1

            if [[ -z "$pg_cols" ]]; then
                echo "ERROR: No destination table for $table" >&2
                return 1
            fi

            # Get column types from SQLite to detect BLOBs
            local col_types
            col_types=$(sqlite3 -readonly "$SQLITE_DB" "PRAGMA table_info(\"$table\");" | cut -d'|' -f2,3) || return 1

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
                echo "ERROR: No common columns for $table" >&2
                return 1
            fi

            migration_psql -q -c "CREATE TABLE $staging.\"$table\" (LIKE $schema.\"$table\" INCLUDING CONSTRAINTS INCLUDING GENERATED INCLUDING IDENTITY INCLUDING INDEXES);" || return 1

            # Find migrate_table.py: check SHIM_DIR first, then script dir, then PATH
            local migrate_py="${SHIM_DIR:-}/migrate_table.py"
            if [ ! -f "$migrate_py" ]; then
                migrate_py="$(dirname "$0")/migrate_table.py"
            fi
            if [ ! -f "$migrate_py" ]; then
                migrate_py="/usr/local/lib/plex-postgresql/migrate_table.py"
            fi

            local log_dir="${LOG_DIR:-/var/log/plex-postgresql}"
            mkdir -p "$log_dir" || return 1

            if python3 "$migrate_py" \
                "$SQLITE_DB" "$table" "$sqlite_select" "$pg_cols_list" "$staging" 2>>"$log_dir/migration_errors.log"; then
                echo -e "${GREEN}OK${NC}"
                migrated=$((migrated + 1))
            else
                echo -e "${RED}FAIL${NC}"
                failed=$((failed + 1))
            fi

            [[ "$failed" == 0 ]] || return 1
            local imported_count
            imported_count=$(migration_psql -tA -c "SELECT count(*) FROM $staging.\"$table\";") || return 1
            [[ "$imported_count" == "$count" ]] || { echo "ERROR: Row count mismatch for $table" >&2; return 1; }
            local trigger_restore
            trigger_restore=$(migration_psql -tA -c "SELECT format('ALTER TABLE %I.%I ENABLE %sTRIGGER %I;', '$schema', '$table', CASE WHEN tgenabled = 'A' THEN 'ALWAYS ' ELSE '' END, tgname) FROM pg_trigger WHERE tgrelid = '$schema.\"$table\"'::regclass AND NOT tgisinternal;") || return 1
            printf 'ALTER TABLE %s."%s" DISABLE TRIGGER USER;\nDELETE FROM %s."%s";\nINSERT INTO %s."%s" (%s) SELECT %s FROM %s."%s";\n%s\n' \
                "$schema" "$table" "$schema" "$table" "$schema" "$table" "$pg_cols_list" "$pg_cols_list" "$staging" "$table" "$trigger_restore" >> "$work_dir/activate.sql" || return 1
            printf 'DO $verify$ BEGIN IF EXISTS ((SELECT %s FROM %s."%s" EXCEPT ALL SELECT %s FROM %s."%s") UNION ALL (SELECT %s FROM %s."%s" EXCEPT ALL SELECT %s FROM %s."%s")) THEN RAISE EXCEPTION '\''Activation contents mismatch: %s'\''; END IF; END $verify$;\n' \
                "$pg_cols_list" "$schema" "$table" "$pg_cols_list" "$staging" "$table" "$pg_cols_list" "$staging" "$table" "$pg_cols_list" "$schema" "$table" "$table" >> "$work_dir/activate.sql" || return 1
        fi
    done

    echo ""
    echo "Migration summary:"
    echo "  Tables migrated: $migrated"
    echo "  Tables skipped:  $skipped"
    echo "  Tables failed:   $failed"

    # Verify JSON integrity in extra_data columns (catches truncation bugs)
    echo ""
    echo "Verifying data integrity..."
    local json_tables="media_parts media_items metadata_items metadata_item_settings tags"
    for jtable in $json_tables; do
        local invalid
        invalid=$(migration_psql -t -c "
            SELECT count(*) FROM $staging.\"$jtable\"
            WHERE extra_data IS NOT NULL AND extra_data LIKE '{%'
              AND extra_data !~ '}\s*$';" | tr -d ' ') || return 1
        if [[ "$invalid" -gt 0 ]]; then
            echo "ERROR: $jtable has $invalid rows with truncated extra_data" >&2
            return 1
        fi
    done
    echo -e "  ${GREEN}Data integrity check complete${NC}"

    local blobs_db="${SQLITE_DB%.db}.blobs.db" blob_count pg_blob_count
    if [[ -f "$blobs_db" ]]; then
        blob_count=$(sqlite3 -readonly "$blobs_db" "SELECT count(*) FROM blobs;") || return 1
        migration_psql -q -c "CREATE TABLE IF NOT EXISTS $staging.blobs (LIKE $schema.blobs INCLUDING CONSTRAINTS INCLUDING GENERATED INCLUDING IDENTITY INCLUDING INDEXES); DELETE FROM $staging.blobs;" || return 1
        local migrate_py="${SHIM_DIR}/migrate_table.py"
        [[ -f "$migrate_py" ]] || migrate_py="$(dirname "${BASH_SOURCE[0]}")/migrate_table.py"
        python3 "$migrate_py" "$blobs_db" blobs 'id, linked_type, linked_id, linked_guid, created_at, blob_type, blob' 'id, linked_type, linked_id, linked_guid, created_at, blob_type, blob' "$staging" || return 1
        pg_blob_count=$(migration_psql -tA -c "SELECT count(*) FROM $staging.blobs;") || return 1
        [[ "$pg_blob_count" == "$blob_count" ]] || return 1
        printf 'DELETE FROM %s.blobs; INSERT INTO %s.blobs (id, linked_type, linked_id, linked_guid, created_at, blob_type, blob) SELECT id, linked_type, linked_id, linked_guid, created_at, blob_type, blob FROM %s.blobs;\n' "$schema" "$schema" "$staging" >> "$work_dir/activate.sql" || return 1
        printf 'DO $verify$ BEGIN IF EXISTS ((TABLE %s.blobs EXCEPT ALL TABLE %s.blobs) UNION ALL (TABLE %s.blobs EXCEPT ALL TABLE %s.blobs)) THEN RAISE EXCEPTION '\''Blob activation mismatch'\''; END IF; END $verify$;\n' "$schema" "$staging" "$staging" "$schema" >> "$work_dir/activate.sql" || return 1
    fi
    sequence_sync_sql >> "$work_dir/activate.sql" || return 1
    printf 'COMMIT;\n' >> "$work_dir/activate.sql" || return 1
    migration_psql -q -f "$work_dir/activate.sql" || return 1
    echo "Validated import activated: $migrated tables"
)
