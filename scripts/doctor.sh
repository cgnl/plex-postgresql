#!/bin/bash
# doctor.sh - Check and fix plex-postgresql schema and data
#
# Detects missing triggers, functions, tables and bad data.
# Schema issues are fixed automatically (safe, idempotent).
# Data issues are shown first and require confirmation before fixing.
#
# Safe to run multiple times — uses CREATE OR REPLACE / IF NOT EXISTS.
#
# Usage:
#   ./scripts/doctor.sh          # interactive (asks before fixing data)
#   ./scripts/doctor.sh --fix    # fix everything without asking
#   ./scripts/doctor.sh --check  # only check, don't fix anything

set -euo pipefail

MODE="interactive"
if [[ "${1:-}" == "--fix" ]]; then
    MODE="fix"
elif [[ "${1:-}" == "--check" ]]; then
    MODE="check"
fi

PSQL="/opt/homebrew/opt/postgresql@15/bin/psql"
if ! command -v "$PSQL" &>/dev/null; then
    PSQL="psql"
fi
if ! command -v "$PSQL" &>/dev/null; then
    echo "ERROR: psql not found"
    exit 1
fi

PG_HOST="${PLEX_PG_HOST:-localhost}"
PG_PORT="${PLEX_PG_PORT:-5432}"
PG_DATABASE="${PLEX_PG_DATABASE:-plex}"
PG_USER="${PLEX_PG_USER:-plex}"
PG_SCHEMA="${PLEX_PG_SCHEMA:-plex}"

export PGHOST="$PG_HOST"
export PGPORT="$PG_PORT"
export PGDATABASE="$PG_DATABASE"
export PGUSER="$PG_USER"
export PGPASSWORD="${PLEX_PG_PASSWORD:-plex}"

if [[ ! "$PG_SCHEMA" =~ ^[a-z_][a-z0-9_]*$ ]]; then
    echo "ERROR: PG_SCHEMA must be a lowercase SQL identifier" >&2
    exit 1
fi
if [[ "$MODE" == check ]]; then
    export PGOPTIONS="${PGOPTIONS:-} -c default_transaction_read_only=on"
fi

doctor_psql() {
    "$PSQL" -X -v ON_ERROR_STOP=1 "$@"
}

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo "=== plex-postgresql doctor ==="
echo ""
echo "PostgreSQL: $PG_USER@$PG_HOST:$PG_PORT/$PG_DATABASE (schema: $PG_SCHEMA)"
echo ""

# Check connection
if ! doctor_psql -c "SELECT 1" >/dev/null 2>&1; then
    echo -e "${RED}Cannot connect to PostgreSQL${NC}"
    exit 1
fi

fixes=0
ok=0
failed=0

check() {
    local name="$1"
    local check_sql="$2"
    local fix_sql="$3"

    local exists
    if ! exists=$(doctor_psql -t -A -c "$check_sql" | tr -d ' '); then
        printf "  %-50s ${RED}QUERY FAILED${NC}\n" "$name"
        failed=$((failed + 1))
        return 0
    fi

    if [[ "$exists" == "t" ]] || [[ "$exists" == "1" ]]; then
        printf "  %-50s ${GREEN}OK${NC}\n" "$name"
        ok=$((ok + 1))
    else
        printf "  %-50s ${YELLOW}MISSING${NC} " "$name"
        if [[ "$MODE" == check ]]; then
            echo "(read-only; not repaired)"
            failed=$((failed + 1))
        elif doctor_psql -q -c "$fix_sql" >/dev/null 2>&1; then
            echo -e "→ ${GREEN}FIXED${NC}"
            fixes=$((fixes + 1))
        else
            echo -e "→ ${RED}FAILED${NC}"
            failed=$((failed + 1))
        fi
    fi
}

# ============================================================================
# Tables
# ============================================================================
echo "Tables:"

check "maintenance_control" \
    "SELECT EXISTS (SELECT 1 FROM pg_tables WHERE schemaname = '$PG_SCHEMA' AND tablename = 'maintenance_control');" \
    "CREATE TABLE IF NOT EXISTS $PG_SCHEMA.maintenance_control (
        table_name text PRIMARY KEY,
        last_cleanup timestamp DEFAULT now(),
        cleanup_interval interval DEFAULT '1 hour',
        retention_days integer DEFAULT 7
    );
    INSERT INTO $PG_SCHEMA.maintenance_control (table_name) VALUES ('statistics_bandwidth'), ('statistics_resources') ON CONFLICT DO NOTHING;"

echo ""

# ============================================================================
# Functions
# ============================================================================
echo "Functions:"

check "prevent_self_referential_parent()" \
    "SELECT EXISTS (SELECT 1 FROM pg_proc p JOIN pg_namespace n ON p.pronamespace = n.oid WHERE n.nspname = '$PG_SCHEMA' AND p.proname = 'prevent_self_referential_parent');" \
    "CREATE OR REPLACE FUNCTION $PG_SCHEMA.prevent_self_referential_parent() RETURNS trigger
    LANGUAGE plpgsql AS \$\$
    BEGIN
        IF NEW.parent_id IS NOT NULL AND NEW.parent_id = NEW.id THEN
            NEW.parent_id := NULL;
        END IF;
        RETURN NEW;
    END;
    \$\$;"

check "prevent_cross_section_parent()" \
    "SELECT EXISTS (SELECT 1 FROM pg_proc p JOIN pg_namespace n ON p.pronamespace = n.oid WHERE n.nspname = '$PG_SCHEMA' AND p.proname = 'prevent_cross_section_parent');" \
    "CREATE OR REPLACE FUNCTION $PG_SCHEMA.prevent_cross_section_parent() RETURNS trigger
    LANGUAGE plpgsql AS \$\$
    DECLARE
        parent_section_id INTEGER;
    BEGIN
        IF NEW.parent_id IS NOT NULL THEN
            SELECT library_section_id INTO parent_section_id
            FROM $PG_SCHEMA.metadata_items WHERE id = NEW.parent_id;
            IF parent_section_id IS NOT NULL AND NEW.library_section_id IS NOT NULL
               AND parent_section_id != NEW.library_section_id THEN
                NEW.parent_id := NULL;
            END IF;
        END IF;
        RETURN NEW;
    END;
    \$\$;"

check "fix_orphan_season_on_episode_insert()" \
    "SELECT EXISTS (SELECT 1 FROM pg_proc p JOIN pg_namespace n ON p.pronamespace = n.oid WHERE n.nspname = '$PG_SCHEMA' AND p.proname = 'fix_orphan_season_on_episode_insert');" \
    "$(cat <<'SQLFUNC'
CREATE OR REPLACE FUNCTION plex.fix_orphan_season_on_episode_insert() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    parent_rec RECORD;
    grandparent_rec RECORD;
BEGIN
    IF NEW.metadata_type = 4 AND NEW.parent_id IS NOT NULL THEN
        SELECT id, parent_id, library_section_id INTO parent_rec
        FROM plex.metadata_items WHERE id = NEW.parent_id;
        IF parent_rec IS NOT NULL AND parent_rec.parent_id IS NULL THEN
            SELECT id INTO grandparent_rec
            FROM plex.metadata_items
            WHERE library_section_id = COALESCE(parent_rec.library_section_id, NEW.library_section_id)
            AND metadata_type = 2
            LIMIT 1;
            IF grandparent_rec IS NOT NULL THEN
                UPDATE plex.metadata_items
                SET parent_id = grandparent_rec.id
                WHERE id = parent_rec.id;
            END IF;
        END IF;
    END IF;
    RETURN NEW;
END;
$$;
SQLFUNC
)"

check "fix_orphan_season_on_media_part_insert()" \
    "SELECT EXISTS (SELECT 1 FROM pg_proc p JOIN pg_namespace n ON p.pronamespace = n.oid WHERE n.nspname = '$PG_SCHEMA' AND p.proname = 'fix_orphan_season_on_media_part_insert');" \
    "$(cat <<'SQLFUNC'
CREATE OR REPLACE FUNCTION plex.fix_orphan_season_on_media_part_insert() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    mi_rec RECORD;
    season_rec RECORD;
    show_rec RECORD;
BEGIN
    SELECT mi.id, mi.metadata_type, mi.parent_id, mi.library_section_id INTO mi_rec
    FROM plex.media_items mai
    JOIN plex.metadata_items mi ON mi.id = mai.metadata_item_id
    WHERE mai.id = NEW.media_item_id;
    IF mi_rec IS NOT NULL AND mi_rec.metadata_type = 4 AND mi_rec.parent_id IS NOT NULL THEN
        SELECT id, parent_id, library_section_id INTO season_rec
        FROM plex.metadata_items WHERE id = mi_rec.parent_id;
        IF season_rec IS NOT NULL AND season_rec.parent_id IS NULL THEN
            SELECT id INTO show_rec
            FROM plex.metadata_items
            WHERE library_section_id = COALESCE(season_rec.library_section_id, mi_rec.library_section_id)
            AND metadata_type = 2
            LIMIT 1;
            IF show_rec IS NOT NULL THEN
                UPDATE plex.metadata_items
                SET parent_id = show_rec.id
                WHERE id = season_rec.id;
            END IF;
        END IF;
    END IF;
    RETURN NEW;
END;
$$;
SQLFUNC
)"

check "maybe_cleanup_statistics()" \
    "SELECT EXISTS (SELECT 1 FROM pg_proc p JOIN pg_namespace n ON p.pronamespace = n.oid WHERE n.nspname = '$PG_SCHEMA' AND p.proname = 'maybe_cleanup_statistics');" \
    "$(cat <<'SQLFUNC'
CREATE OR REPLACE FUNCTION plex.maybe_cleanup_statistics() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    ctrl maintenance_control%ROWTYPE;
    cutoff_time BIGINT;
    deleted_count INTEGER;
BEGIN
    SELECT * INTO ctrl FROM maintenance_control WHERE table_name = TG_TABLE_NAME;
    IF ctrl IS NULL OR ctrl.last_cleanup + ctrl.cleanup_interval > NOW() THEN
        RETURN NEW;
    END IF;
    cutoff_time := EXTRACT(EPOCH FROM (NOW() - (ctrl.retention_days || ' days')::INTERVAL))::BIGINT;
    IF TG_TABLE_NAME = 'statistics_bandwidth' THEN
        DELETE FROM statistics_bandwidth WHERE at < cutoff_time;
    ELSIF TG_TABLE_NAME = 'statistics_resources' THEN
        DELETE FROM statistics_resources WHERE at < cutoff_time;
    END IF;
    GET DIAGNOSTICS deleted_count = ROW_COUNT;
    UPDATE maintenance_control SET last_cleanup = NOW() WHERE table_name = TG_TABLE_NAME;
    IF deleted_count > 0 THEN
        RAISE LOG 'Cleaned % rows from %', deleted_count, TG_TABLE_NAME;
    END IF;
    RETURN NEW;
END;
$$;
SQLFUNC
)"

check "reject_empty_statistics()" \
    "SELECT EXISTS (SELECT 1 FROM pg_proc p JOIN pg_namespace n ON p.pronamespace = n.oid WHERE n.nspname = '$PG_SCHEMA' AND p.proname = 'reject_empty_statistics');" \
    "CREATE OR REPLACE FUNCTION $PG_SCHEMA.reject_empty_statistics() RETURNS trigger
    LANGUAGE plpgsql AS \$\$
    BEGIN
        IF NEW.at IS NULL OR NEW.at = 0 THEN
            RETURN NULL;
        END IF;
        RETURN NEW;
    END;
    \$\$;"

check "set_available_at()" \
    "SELECT EXISTS (SELECT 1 FROM pg_proc p JOIN pg_namespace n ON p.pronamespace = n.oid WHERE n.nspname = '$PG_SCHEMA' AND p.proname = 'set_available_at');" \
    "CREATE OR REPLACE FUNCTION $PG_SCHEMA.set_available_at() RETURNS trigger
    LANGUAGE plpgsql AS \$\$
    BEGIN
        IF NEW.available_at IS NULL THEN
            NEW.available_at := NEW.created_at;
        END IF;
        RETURN NEW;
    END;
    \$\$;"

check "metadata_items_search_trigger()" \
    "SELECT EXISTS (SELECT 1 FROM pg_proc p JOIN pg_namespace n ON p.pronamespace = n.oid WHERE n.nspname = '$PG_SCHEMA' AND p.proname = 'metadata_items_search_trigger');" \
    "CREATE OR REPLACE FUNCTION $PG_SCHEMA.metadata_items_search_trigger() RETURNS trigger
    LANGUAGE plpgsql AS \$\$
    BEGIN
        NEW.title_sort := COALESCE(NEW.title_sort, NEW.title);
        RETURN NEW;
    END;
    \$\$;"

check "tags_search_trigger()" \
    "SELECT EXISTS (SELECT 1 FROM pg_proc p JOIN pg_namespace n ON p.pronamespace = n.oid WHERE n.nspname = '$PG_SCHEMA' AND p.proname = 'tags_search_trigger');" \
    "CREATE OR REPLACE FUNCTION $PG_SCHEMA.tags_search_trigger() RETURNS trigger
    LANGUAGE plpgsql AS \$\$
    BEGIN
        RETURN NEW;
    END;
    \$\$;"

check "integer_equals_text()" \
    "SELECT EXISTS (SELECT 1 FROM pg_proc p JOIN pg_namespace n ON p.pronamespace = n.oid WHERE n.nspname = '$PG_SCHEMA' AND p.proname = 'integer_equals_text');" \
    "CREATE FUNCTION $PG_SCHEMA.integer_equals_text(integer, text) RETURNS boolean
    LANGUAGE sql IMMUTABLE AS \$\$
        SELECT \$1::text = \$2;
    \$\$;
    CREATE OPERATOR $PG_SCHEMA.= (
        LEFTARG = integer, RIGHTARG = text,
        FUNCTION = $PG_SCHEMA.integer_equals_text,
        COMMUTATOR = OPERATOR($PG_SCHEMA.=)
    );"

check "text_equals_integer()" \
    "SELECT EXISTS (SELECT 1 FROM pg_proc p JOIN pg_namespace n ON p.pronamespace = n.oid WHERE n.nspname = '$PG_SCHEMA' AND p.proname = 'text_equals_integer');" \
    "CREATE FUNCTION $PG_SCHEMA.text_equals_integer(text, integer) RETURNS boolean
    LANGUAGE sql IMMUTABLE AS \$\$
        SELECT \$1 = \$2::text;
    \$\$;
    CREATE OPERATOR $PG_SCHEMA.= (
        LEFTARG = text, RIGHTARG = integer,
        FUNCTION = $PG_SCHEMA.text_equals_integer,
        COMMUTATOR = OPERATOR($PG_SCHEMA.=)
    );"

echo ""

# ============================================================================
# Triggers
# ============================================================================
echo "Triggers:"

check "prevent_self_ref_parent" \
    "SELECT EXISTS (SELECT 1 FROM pg_trigger t JOIN pg_class c ON t.tgrelid = c.oid JOIN pg_namespace n ON c.relnamespace = n.oid WHERE n.nspname = '$PG_SCHEMA' AND t.tgname = 'prevent_self_ref_parent');" \
    "CREATE TRIGGER prevent_self_ref_parent BEFORE INSERT OR UPDATE ON $PG_SCHEMA.metadata_items FOR EACH ROW EXECUTE FUNCTION $PG_SCHEMA.prevent_self_referential_parent();"

check "check_cross_section_parent" \
    "SELECT EXISTS (SELECT 1 FROM pg_trigger t JOIN pg_class c ON t.tgrelid = c.oid JOIN pg_namespace n ON c.relnamespace = n.oid WHERE n.nspname = '$PG_SCHEMA' AND t.tgname = 'check_cross_section_parent');" \
    "CREATE TRIGGER check_cross_section_parent BEFORE INSERT OR UPDATE OF parent_id, library_section_id ON $PG_SCHEMA.metadata_items FOR EACH ROW WHEN (NEW.parent_id IS NOT NULL) EXECUTE FUNCTION $PG_SCHEMA.prevent_cross_section_parent();"

check "metadata_items_search_update" \
    "SELECT EXISTS (SELECT 1 FROM pg_trigger t JOIN pg_class c ON t.tgrelid = c.oid JOIN pg_namespace n ON c.relnamespace = n.oid WHERE n.nspname = '$PG_SCHEMA' AND t.tgname = 'metadata_items_search_update');" \
    "CREATE TRIGGER metadata_items_search_update BEFORE INSERT OR UPDATE ON $PG_SCHEMA.metadata_items FOR EACH ROW EXECUTE FUNCTION $PG_SCHEMA.metadata_items_search_trigger();"

check "metadata_items_set_available_at" \
    "SELECT EXISTS (SELECT 1 FROM pg_trigger t JOIN pg_class c ON t.tgrelid = c.oid JOIN pg_namespace n ON c.relnamespace = n.oid WHERE n.nspname = '$PG_SCHEMA' AND t.tgname = 'metadata_items_set_available_at');" \
    "CREATE TRIGGER metadata_items_set_available_at BEFORE INSERT ON $PG_SCHEMA.metadata_items FOR EACH ROW EXECUTE FUNCTION $PG_SCHEMA.set_available_at();"

check "tags_search_update" \
    "SELECT EXISTS (SELECT 1 FROM pg_trigger t JOIN pg_class c ON t.tgrelid = c.oid JOIN pg_namespace n ON c.relnamespace = n.oid WHERE n.nspname = '$PG_SCHEMA' AND t.tgname = 'tags_search_update');" \
    "CREATE TRIGGER tags_search_update BEFORE INSERT OR UPDATE ON $PG_SCHEMA.tags FOR EACH ROW EXECUTE FUNCTION $PG_SCHEMA.tags_search_trigger();"

check "statistics_media_reject_empty" \
    "SELECT EXISTS (SELECT 1 FROM pg_trigger t JOIN pg_class c ON t.tgrelid = c.oid JOIN pg_namespace n ON c.relnamespace = n.oid WHERE n.nspname = '$PG_SCHEMA' AND t.tgname = 'statistics_media_reject_empty');" \
    "CREATE TRIGGER statistics_media_reject_empty BEFORE INSERT ON $PG_SCHEMA.statistics_media FOR EACH ROW EXECUTE FUNCTION $PG_SCHEMA.reject_empty_statistics();"

check "trg_clean_statistics_resources" \
    "SELECT EXISTS (SELECT 1 FROM pg_trigger t JOIN pg_class c ON t.tgrelid = c.oid JOIN pg_namespace n ON c.relnamespace = n.oid WHERE n.nspname = '$PG_SCHEMA' AND t.tgname = 'trg_clean_statistics_resources');" \
    "CREATE TRIGGER trg_clean_statistics_resources AFTER INSERT ON $PG_SCHEMA.statistics_resources FOR EACH STATEMENT EXECUTE FUNCTION $PG_SCHEMA.maybe_cleanup_statistics();"

check "trg_fix_orphan_season" \
    "SELECT EXISTS (SELECT 1 FROM pg_trigger t JOIN pg_class c ON t.tgrelid = c.oid JOIN pg_namespace n ON c.relnamespace = n.oid WHERE n.nspname = '$PG_SCHEMA' AND t.tgname = 'trg_fix_orphan_season');" \
    "CREATE TRIGGER trg_fix_orphan_season AFTER INSERT ON $PG_SCHEMA.metadata_items FOR EACH ROW EXECUTE FUNCTION $PG_SCHEMA.fix_orphan_season_on_episode_insert();"

check "trg_fix_orphan_season_media" \
    "SELECT EXISTS (SELECT 1 FROM pg_trigger t JOIN pg_class c ON t.tgrelid = c.oid JOIN pg_namespace n ON c.relnamespace = n.oid WHERE n.nspname = '$PG_SCHEMA' AND t.tgname = 'trg_fix_orphan_season_media');" \
    "CREATE TRIGGER trg_fix_orphan_season_media AFTER INSERT ON $PG_SCHEMA.media_parts FOR EACH ROW EXECUTE FUNCTION $PG_SCHEMA.fix_orphan_season_on_media_part_insert();"

# ============================================================================
# Schema migrations (columns added in newer Plex versions)
# ============================================================================
echo ""
echo "Schema migrations:"

check_column() {
    local table="$1"
    local column="$2"
    local coltype="${3:-text}"

    check "$table.$column" \
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_schema = '$PG_SCHEMA' AND table_name = '$table' AND column_name = '$column');" \
        "ALTER TABLE $PG_SCHEMA.$table ADD COLUMN $column $coltype;"
}

# Plex 1.43.0 additions
check_column "metadata_items" "user_square_art_url" "text"
check_column "schema_migrations" "id" "serial"

# Guard constraint: prevent junk rows with both metadata_type and library_section_id NULL
check "chk_not_orphan constraint" \
    "SELECT EXISTS (SELECT 1 FROM information_schema.table_constraints WHERE constraint_schema = '$PG_SCHEMA' AND table_name = 'metadata_items' AND constraint_name = 'chk_not_orphan');" \
    "ALTER TABLE $PG_SCHEMA.metadata_items ADD CONSTRAINT chk_not_orphan CHECK (metadata_type IS NOT NULL OR library_section_id IS NOT NULL);"

# Unique index on blobs for ON CONFLICT upsert (linked_type, linked_id, blob_type)
check "blobs upsert unique index" \
    "SELECT EXISTS (SELECT 1 FROM pg_indexes WHERE schemaname = '$PG_SCHEMA' AND tablename = 'blobs' AND indexname = 'idx_blobs_linked_type_id_blob_type');" \
    "CREATE UNIQUE INDEX idx_blobs_linked_type_id_blob_type ON $PG_SCHEMA.blobs (linked_type, linked_id, blob_type);"

echo ""

# ============================================================================
# Data integrity
# ============================================================================
echo ""
check "all constraints validated" \
    "SELECT NOT EXISTS (SELECT 1 FROM pg_constraint c JOIN pg_namespace n ON n.oid = c.connamespace WHERE n.nspname = '$PG_SCHEMA' AND NOT c.convalidated);" \
    "DO \$do\$ DECLARE item record; BEGIN FOR item IN SELECT c.conrelid::regclass AS relation, c.conname FROM pg_constraint c JOIN pg_namespace n ON n.oid = c.connamespace WHERE n.nspname = '$PG_SCHEMA' AND NOT c.convalidated LOOP EXECUTE format('ALTER TABLE %s VALIDATE CONSTRAINT %I', item.relation, item.conname); END LOOP; END \$do\$;"
check "all triggers enabled" \
    "SELECT NOT EXISTS (SELECT 1 FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = '$PG_SCHEMA' AND t.tgenabled NOT IN ('O', 'A'));" \
    "DO \$do\$ DECLARE item record; BEGIN FOR item IN SELECT c.oid::regclass AS relation, t.tgname FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = '$PG_SCHEMA' AND t.tgenabled NOT IN ('O', 'A') LOOP EXECUTE format('ALTER TABLE %s ENABLE TRIGGER %I', item.relation, item.tgname); END LOOP; END \$do\$;"

echo "Data:"

data_issues=0

# Self-referential parents (parent_id = id)
self_ref=$(doctor_psql -t -A -c "SELECT COUNT(*) FROM $PG_SCHEMA.metadata_items WHERE parent_id = id;")
if [[ "$self_ref" -gt 0 ]]; then
    printf "  %-50s ${YELLOW}%s rows${NC}\n" "self-referential parent_id" "$self_ref"
    data_issues=$((data_issues + 1))
else
    printf "  %-50s ${GREEN}OK${NC}\n" "self-referential parent_id"
    ok=$((ok + 1))
fi

# Cross-section parents (parent in different library_section)
cross_section=$(doctor_psql -t -A -c "
    SELECT COUNT(*) FROM $PG_SCHEMA.metadata_items m
    JOIN $PG_SCHEMA.metadata_items p ON m.parent_id = p.id
    WHERE m.parent_id IS NOT NULL
    AND m.library_section_id IS NOT NULL
    AND p.library_section_id IS NOT NULL
    AND m.library_section_id != p.library_section_id;
")
if [[ "$cross_section" -gt 0 ]]; then
    printf "  %-50s ${YELLOW}%s rows${NC}\n" "cross-section parent_id" "$cross_section"
    data_issues=$((data_issues + 1))
else
    printf "  %-50s ${GREEN}OK${NC}\n" "cross-section parent_id"
    ok=$((ok + 1))
fi

# Orphan seasons (metadata_type=3 with no parent)
orphan_seasons=$(doctor_psql -t -A -c "
    SELECT COUNT(*) FROM $PG_SCHEMA.metadata_items
    WHERE metadata_type = 3 AND parent_id IS NULL;
")
if [[ "$orphan_seasons" -gt 0 ]]; then
    printf "  %-50s ${YELLOW}%s rows${NC}\n" "orphan seasons (no parent)" "$orphan_seasons"
    data_issues=$((data_issues + 1))
else
    printf "  %-50s ${GREEN}OK${NC}\n" "orphan seasons (no parent)"
    ok=$((ok + 1))
fi

# Junk metadata_items (both metadata_type and library_section_id NULL)
junk_items=$(doctor_psql -t -A -c "
    SELECT COUNT(*) FROM $PG_SCHEMA.metadata_items
    WHERE metadata_type IS NULL AND library_section_id IS NULL;
")
if [[ "$junk_items" -gt 0 ]]; then
    printf "  %-50s ${YELLOW}%s rows${NC}\n" "junk metadata_items (no type/library)" "$junk_items"
    data_issues=$((data_issues + 1))
else
    printf "  %-50s ${GREEN}OK${NC}\n" "junk metadata_items (no type/library)"
    ok=$((ok + 1))
fi

# Truncated JSON in extra_data (caused by old CSV migration bug)
truncated_json=$(doctor_psql -t -A -c "
    SELECT sum(cnt)::integer FROM (
        SELECT count(*) AS cnt FROM $PG_SCHEMA.media_parts
        WHERE extra_data IS NOT NULL AND extra_data LIKE '{%'
          AND extra_data !~ '}\s*$'
        UNION ALL
        SELECT count(*) FROM $PG_SCHEMA.media_items
        WHERE extra_data IS NOT NULL AND extra_data LIKE '{%'
          AND extra_data !~ '}\s*$'
        UNION ALL
        SELECT count(*) FROM $PG_SCHEMA.metadata_items
        WHERE extra_data IS NOT NULL AND extra_data LIKE '{%'
          AND extra_data !~ '}\s*$'
        UNION ALL
        SELECT count(*) FROM $PG_SCHEMA.metadata_item_settings
        WHERE extra_data IS NOT NULL AND extra_data LIKE '{%'
          AND extra_data !~ '}\s*$'
    ) t;
")
if [[ "$truncated_json" -gt 0 ]]; then
    printf "  %-50s ${YELLOW}%s rows${NC}\n" "truncated extra_data JSON" "$truncated_json"
    data_issues=$((data_issues + 1))
else
    printf "  %-50s ${GREEN}OK${NC}\n" "truncated extra_data JSON"
    ok=$((ok + 1))
fi

# Empty statistics rows (at = 0 or NULL)
empty_stats=$(doctor_psql -t -A -c "
    SELECT COUNT(*) FROM $PG_SCHEMA.statistics_media WHERE at IS NULL OR at = 0;
")
if [[ "$empty_stats" -gt 0 ]]; then
    printf "  %-50s ${YELLOW}%s rows${NC}\n" "empty statistics_media rows" "$empty_stats"
    data_issues=$((data_issues + 1))
else
    printf "  %-50s ${GREEN}OK${NC}\n" "empty statistics_media rows"
    ok=$((ok + 1))
fi

# Old statistics (> 7 days)
old_stats=$(doctor_psql -t -A -c "
    SELECT COUNT(*) FROM $PG_SCHEMA.statistics_resources
    WHERE at < EXTRACT(EPOCH FROM (NOW() - INTERVAL '7 days'))::BIGINT;
")
if [[ "$old_stats" -gt 0 ]]; then
    printf "  %-50s ${YELLOW}%s rows${NC}\n" "stale statistics_resources (>7d)" "$old_stats"
    data_issues=$((data_issues + 1))
else
    printf "  %-50s ${GREEN}OK${NC}\n" "stale statistics_resources (>7d)"
    ok=$((ok + 1))
fi

# Fix data issues if any
if [[ $data_issues -gt 0 ]]; then
    echo ""

    if [[ "$MODE" == "check" ]]; then
        echo -e "  ${YELLOW}$data_issues data issue(s) found. Run without --check to fix.${NC}"
    else
        do_fix=false
        if [[ "$MODE" == "fix" ]]; then
            do_fix=true
        else
            echo -e "  ${YELLOW}$data_issues data issue(s) found.${NC}"
            read -p "  Fix them? [y/N]: " answer
            if [[ "$answer" =~ ^[Yy] ]]; then
                do_fix=true
            fi
        fi

        if $do_fix; then
            echo ""
            if [[ "$self_ref" -gt 0 ]]; then
                printf "  fixing self-referential parent_id... "
                doctor_psql -q -c "UPDATE $PG_SCHEMA.metadata_items SET parent_id = NULL WHERE parent_id = id;" 2>/dev/null
                echo -e "${GREEN}$self_ref rows${NC}"
                fixes=$((fixes + 1))
            fi
            if [[ "$cross_section" -gt 0 ]]; then
                printf "  fixing cross-section parent_id... "
                doctor_psql -q -c "
                    UPDATE $PG_SCHEMA.metadata_items m SET parent_id = NULL
                    FROM $PG_SCHEMA.metadata_items p
                    WHERE m.parent_id = p.id
                    AND m.library_section_id IS NOT NULL
                    AND p.library_section_id IS NOT NULL
                    AND m.library_section_id != p.library_section_id;
                " 2>/dev/null
                echo -e "${GREEN}$cross_section rows${NC}"
                fixes=$((fixes + 1))
            fi
            if [[ "$orphan_seasons" -gt 0 ]]; then
                printf "  fixing orphan seasons... "
                fixed=$(doctor_psql -t -A -c "
                    WITH fixes AS (
                        UPDATE $PG_SCHEMA.metadata_items season
                        SET parent_id = (
                            SELECT show.id FROM $PG_SCHEMA.metadata_items show
                            WHERE show.library_section_id = season.library_section_id
                            AND show.metadata_type = 2
                            LIMIT 1
                        )
                        WHERE season.metadata_type = 3
                        AND season.parent_id IS NULL
                        AND EXISTS (
                            SELECT 1 FROM $PG_SCHEMA.metadata_items show
                            WHERE show.library_section_id = season.library_section_id
                            AND show.metadata_type = 2
                        )
                        RETURNING 1
                    )
                    SELECT COUNT(*) FROM fixes;
                ")
                echo -e "${GREEN}$fixed rows${NC}"
                fixes=$((fixes + 1))
            fi
            if [[ "$junk_items" -gt 0 ]]; then
                printf "  deleting junk metadata_items... "
                doctor_psql -q -c "DELETE FROM $PG_SCHEMA.metadata_items WHERE metadata_type IS NULL AND library_section_id IS NULL;" 2>/dev/null
                echo -e "${GREEN}$junk_items rows${NC}"
                fixes=$((fixes + 1))
            fi
            if [[ "$truncated_json" -gt 0 ]]; then
                printf "  fixing truncated extra_data JSON... "
                fixed_json=0
                for repair_table in media_parts media_items metadata_items metadata_item_settings; do
                    repaired=$(doctor_psql -t -A -c "
                        WITH fixed AS (
                            UPDATE $PG_SCHEMA.$repair_table
                            SET extra_data = left(extra_data, position('\"url\":\"' in extra_data) - 2) || '}'
                            WHERE extra_data IS NOT NULL AND extra_data LIKE '{%'
                              AND extra_data !~ '}\s*$'
                              AND position('\"url\":\"' in extra_data) > 0
                            RETURNING 1
                        ) SELECT count(*) FROM fixed;
                    ")
                    fixed_json=$((fixed_json + repaired))
                done
                echo -e "${GREEN}$fixed_json rows${NC}"
                fixes=$((fixes + 1))
            fi
            if [[ "$empty_stats" -gt 0 ]]; then
                printf "  cleaning empty statistics_media... "
                doctor_psql -q -c "DELETE FROM $PG_SCHEMA.statistics_media WHERE at IS NULL OR at = 0;" 2>/dev/null
                echo -e "${GREEN}$empty_stats rows${NC}"
                fixes=$((fixes + 1))
            fi
            if [[ "$old_stats" -gt 0 ]]; then
                printf "  cleaning stale statistics_resources... "
                doctor_psql -q -c "
                    DELETE FROM $PG_SCHEMA.statistics_resources
                    WHERE at < EXTRACT(EPOCH FROM (NOW() - INTERVAL '7 days'))::BIGINT;
                " 2>/dev/null
                echo -e "${GREEN}$old_stats rows${NC}"
                fixes=$((fixes + 1))
            fi
        else
            echo "  Skipped."
        fi
    fi
fi

echo ""
echo "=== Done ==="
if [[ $fixes -gt 0 ]]; then
    echo -e "  $ok OK, ${GREEN}$fixes fixed${NC}, $failed failed"
elif [[ $failed -gt 0 ]]; then
    echo -e "  $ok OK, ${RED}$failed failed${NC}"
else
    echo -e "  ${GREEN}All $ok checks passed${NC}"
fi

if [[ "$MODE" != check && "$fixes" -gt 0 ]]; then
    exec bash "$0" --check
fi
if [[ "$failed" -gt 0 || "$data_issues" -gt 0 ]]; then
    exit 1
fi
