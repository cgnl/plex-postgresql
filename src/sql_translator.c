/*
 * SQL Translator - Main Orchestrator
 * SQLite to PostgreSQL SQL translation
 *
 * This file coordinates all translation modules:
 * - sql_tr_helpers.c     - String utilities
 * - sql_tr_placeholders.c - Placeholder translation
 * - sql_tr_functions.c   - Function translations
 * - sql_tr_query.c       - Query structure fixes
 * - sql_tr_types.c       - Type translations
 * - sql_tr_quotes.c      - Quote translations
 * - sql_tr_keywords.c    - Keyword translations
 */

#include "sql_translator.h"
#include "sql_translator_internal.h"
#include "pg_logging.h"

// ============================================================================
// Main Function Translator (orchestrates all function translations)
// ============================================================================

char* sql_translate_functions(const char *sql) {
    if (!sql) return NULL;
    LOG_ERROR("sql_translate_functions entry");

    char *current = strdup(sql);
    if (!current) return NULL;

    char *temp;

    // 0. FTS4 queries -> ILIKE queries
    temp = translate_fts(current);
    free(current);
    if (!temp) { LOG_ERROR("translate_fts returned NULL"); return NULL; }
    current = temp;

    // 0b. Convert SQLite NULL sorting to PostgreSQL NULLS LAST
    // This must happen before translate_distinct_orderby
    temp = translate_null_sorting(current);
    free(current);
    if (!temp) { LOG_ERROR("translate_null_sorting returned NULL"); return NULL; }
    current = temp;

    // 0c. Remove DISTINCT when ORDER BY is present
    temp = translate_distinct_orderby(current);
    free(current);
    if (!temp) { LOG_ERROR("translate_distinct_orderby returned NULL"); return NULL; }
    current = temp;

    // 0e. Simplify typeof fixup patterns (before iif/typeof translations)
    temp = simplify_typeof_fixup(current);
    free(current);
    if (!temp) { LOG_ERROR("simplify_typeof_fixup returned NULL"); return NULL; }
    current = temp;

    // 0f. Fix duplicate assignments (UPDATE set a=1, a=2)
    temp = fix_duplicate_assignments(current);
    free(current);
    if (!temp) { LOG_ERROR("fix_duplicate_assignments returned NULL"); return NULL; }
    current = temp;

    // 1. iif() -> CASE WHEN
    temp = translate_iif(current);
    free(current);
    if (!temp) { LOG_ERROR("translate_iif returned NULL"); return NULL; }
    current = temp;

    // 2. typeof() -> pg_typeof()::text
    temp = translate_typeof(current);
    free(current);
    if (!temp) { LOG_ERROR("translate_typeof returned NULL"); return NULL; }
    current = temp;

    // 3. strftime() -> EXTRACT/TO_CHAR
    temp = translate_strftime(current);
    free(current);
    if (!temp) { LOG_ERROR("translate_strftime returned NULL"); return NULL; }
    current = temp;

    // 4. unixepoch() -> EXTRACT(EPOCH FROM ...)
    temp = translate_unixepoch(current);
    free(current);
    if (!temp) { LOG_ERROR("translate_unixepoch returned NULL"); return NULL; }
    current = temp;

    // 5. datetime('now') -> NOW()
    temp = translate_datetime(current);
    free(current);
    if (!temp) { LOG_ERROR("translate_datetime returned NULL"); return NULL; }
    current = temp;

    // 5a. last_insert_rowid() -> lastval()
    temp = translate_last_insert_rowid(current);
    free(current);
    if (!temp) { LOG_ERROR("translate_last_insert_rowid returned NULL"); return NULL; }
    current = temp;

    // 5b. json_each() -> json_array_elements()
    temp = translate_json_each(current);
    free(current);
    if (!temp) { LOG_ERROR("translate_json_each returned NULL"); return NULL; }
    current = temp;

    // 6. IFNULL -> COALESCE
    temp = str_replace_nocase(current, "IFNULL(", "COALESCE(");
    free(current);
    if (!temp) { LOG_ERROR("IFNULL replacement returned NULL"); return NULL; }
    current = temp;

    // 7. SUBSTR -> SUBSTRING
    temp = str_replace_nocase(current, "SUBSTR(", "SUBSTRING(");
    free(current);
    if (!temp) { LOG_ERROR("SUBSTR replacement returned NULL"); return NULL; }
    current = temp;

    // 11. max(a, b) -> GREATEST(a, b)
    temp = translate_max_to_greatest(current);
    free(current);
    if (!temp) { LOG_ERROR("translate_max_to_greatest returned NULL"); return NULL; }
    current = temp;

    // 12. min(a, b) -> LEAST(a, b)
    temp = translate_min_to_least(current);
    free(current);
    if (!temp) { LOG_ERROR("translate_min_to_least returned NULL"); return NULL; }
    current = temp;

    // 13. CASE THEN 0/1 -> THEN FALSE/TRUE
    temp = translate_case_booleans(current);
    free(current);
    if (!temp) { LOG_ERROR("translate_case_booleans returned NULL"); return NULL; }
    current = temp;

    // 14. Add alias to subqueries in FROM clause
    temp = add_subquery_alias(current);
    free(current);
    if (!temp) { LOG_ERROR("add_subquery_alias returned NULL"); return NULL; }
    current = temp;

    // 15. Fix forward reference in self-joins
    temp = fix_forward_reference_joins(current);
    free(current);
    if (!temp) { LOG_ERROR("fix_forward_reference_joins returned NULL"); return NULL; }
    current = temp;

    // 15a. Fix integer/text mismatch
    temp = fix_integer_text_mismatch(current);
    free(current);
    if (!temp) { LOG_ERROR("fix_integer_text_mismatch returned NULL"); return NULL; }
    current = temp;

    // 15b. Fix GROUP BY strict mode (legacy single-case handler)
    temp = fix_group_by_strict(current);
    free(current);
    if (!temp) { LOG_ERROR("fix_group_by_strict returned NULL"); return NULL; }
    current = temp;

    // 15b2. Fix GROUP BY strict mode (complete rewriter)
    temp = fix_group_by_strict_complete(current);
    free(current);
    if (!temp) { LOG_ERROR("fix_group_by_strict_complete returned NULL"); return NULL; }
    current = temp;

    // 15c. Strip "collate icu_root"
    temp = strip_icu_collation(current);
    free(current);
    if (!temp) { LOG_ERROR("strip_icu_collation returned NULL"); return NULL; }
    current = temp;

    // 15d. Fix JSON operator ->> on TEXT columns
    temp = fix_json_operator_on_text(current);
    free(current);
    if (!temp) { LOG_ERROR("fix_json_operator_on_text returned NULL"); return NULL; }
    current = temp;

    // 16. Fix incomplete GROUP BY for specific queries
    if (strcasestr(current, "metadata_item_views.originally_available_at") &&
        strcasestr(current, "group by grandparents.id order by")) {
        temp = str_replace(current,
            "group by grandparents.id order by",
            "group by grandparents.id,metadata_item_views.originally_available_at,metadata_item_views.parent_index,metadata_item_views.\"index\",grandparents.library_section_id,grandparentsSettings.extra_data,metadata_item_views.viewed_at order by");
        free(current);
        if (!temp) { LOG_ERROR("fix_incomplete_group_by returned NULL"); return NULL; }
        current = temp;
    }

    // external_metadata_items query fix
    if (strcasestr(current, "external_metadata_items.id,uri,user_title") &&
        strcasestr(current, "group by title order by")) {
        temp = str_replace(current,
            "group by title order by",
            "group by title,external_metadata_items.id,uri,user_title,library_section_id,metadata_type,year,added_at,updated_at,extra_data order by");
        free(current);
        if (!temp) { LOG_ERROR("external_metadata_items fix returned NULL"); return NULL; }
        current = temp;
    }

    // metadata_item_clusterings fix
    if (strcasestr(current, "metadata_item_clusterings") &&
        strcasestr(current, "group by")) {
        if (strcasestr(current, "select DISTINCT")) {
            char *group_pos = strcasestr(current, " group by ");
            if (group_pos) {
                char *end = strcasestr(group_pos + 10, " order by ");
                if (!end) end = strcasestr(group_pos + 10, " limit ");
                if (!end) end = group_pos + strlen(group_pos);

                size_t before_len = group_pos - current;
                size_t after_len = strlen(end);
                char *new_sql = malloc(before_len + after_len + 1);
                if (new_sql) {
                    memcpy(new_sql, current, before_len);
                    memcpy(new_sql + before_len, end, after_len);
                    new_sql[before_len + after_len] = '\0';
                    free(current);
                    current = new_sql;
                }
            }
        }
    }

    return current;
}

// ============================================================================
// Initialization and Cleanup
// ============================================================================

void sql_translator_init(void) {
    // Nothing to initialize for now
}

void sql_translator_cleanup(void) {
    // Nothing to cleanup for now
}

// ============================================================================
// Main Translation Function
// ============================================================================

sql_translation_t sql_translate(const char *sqlite_sql) {
    sql_translation_t result = {0};

    if (!sqlite_sql) {
        strcpy(result.error, "NULL input SQL");
        return result;
    }

    // Step 1: Translate placeholders
    char *step1 = sql_translate_placeholders(sqlite_sql, &result.param_names, &result.param_count);
    if (!step1) {
        strcpy(result.error, "Placeholder translation failed");
        return result;
    }

    // Step 2: Translate functions
    char *step2 = sql_translate_functions(step1);
    free(step1);
    if (!step2) {
        strcpy(result.error, "Function translation failed");
        return result;
    }

    // Step 3: Translate types
    char *step3 = sql_translate_types(step2);
    free(step2);
    if (!step3) {
        strcpy(result.error, "Type translation failed");
        return result;
    }

    // Step 4: Translate keywords
    char *step4 = sql_translate_keywords(step3);
    free(step3);
    if (!step4) {
        strcpy(result.error, "Keyword translation failed");
        return result;
    }

    // Step 4a: Translate INSERT OR REPLACE to ON CONFLICT DO UPDATE
    char *step4a = translate_insert_or_replace(step4);
    free(step4);
    if (!step4a) {
        strcpy(result.error, "INSERT OR REPLACE translation failed");
        return result;
    }

    // Step 5: Translate DDL quotes
    char *step5 = translate_ddl_quotes(step4a);
    free(step4a);
    if (!step5) {
        strcpy(result.error, "DDL quote translation failed");
        return result;
    }

    // Step 6: Add IF NOT EXISTS
    char *step6 = add_if_not_exists(step5);
    free(step5);
    if (!step6) {
        strcpy(result.error, "IF NOT EXISTS translation failed");
        return result;
    }

    // Step 7: Fix operator spacing
    char *step7 = fix_operator_spacing(step6);
    free(step6);
    if (!step7) {
        strcpy(result.error, "Operator spacing fix failed");
        return result;
    }

    // Step 8: Fix ON CONFLICT quotes
    char *step8 = fix_on_conflict_quotes(step7);
    free(step7);
    if (!step8) {
        strcpy(result.error, "ON CONFLICT quote fix failed");
        return result;
    }

    // Step 9: Fix collections query (add metadata_type to SELECT)
    char *step9 = fix_collections_query(step8);
    free(step8);
    if (!step9) {
        strcpy(result.error, "Collections query fix failed");
        return result;
    }

    result.sql = step9;
    result.success = 1;

    return result;
}

// ============================================================================
// Cleanup Functions
// ============================================================================

void sql_translation_free(sql_translation_t *result) {
    if (!result) return;

    if (result->sql) {
        free(result->sql);
        result->sql = NULL;
    }

    if (result->param_names) {
        for (int i = 0; i < result->param_count; i++) {
            if (result->param_names[i]) {
                free(result->param_names[i]);
            }
        }
        free(result->param_names);
        result->param_names = NULL;
    }

    result->param_count = 0;
    result->success = 0;
}

void sql_translator_free(char *sql) {
    free(sql);
}
