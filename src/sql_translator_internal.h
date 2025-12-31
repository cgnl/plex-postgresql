/*
 * SQL Translator - Internal Header
 * Shared definitions between sql_translator modules
 */

#ifndef SQL_TRANSLATOR_INTERNAL_H
#define SQL_TRANSLATOR_INTERNAL_H

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>  // for strcasestr, strncasecmp
#include <ctype.h>

#define MAX_SQL_LEN 131072
#define MAX_PARAM_NAME 64
// Note: MAX_PARAMS is defined in pg_types.h (256) for struct array sizes

// ============================================================================
// Helper Functions (sql_tr_helpers.c)
// ============================================================================

char* str_replace(const char *str, const char *old, const char *new_str);
char* str_replace_nocase(const char *str, const char *old, const char *new_str);
const char* extract_arg(const char *start, char *buf, size_t bufsize);

// ============================================================================
// Inline Hot-Path Helpers (avoid function call overhead)
// ============================================================================

static inline const char* skip_ws(const char *p) {
    while (*p && (*p == ' ' || *p == '\t' || *p == '\n' || *p == '\r')) p++;
    return p;
}

static inline int is_ident_char(char c) {
    // Fast path: check common ranges directly instead of calling isalnum()
    return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
           (c >= '0' && c <= '9') || c == '_';
}

// ============================================================================
// Function Translations (sql_tr_functions.c)
// ============================================================================

char* translate_iif(const char *sql);
char* translate_typeof(const char *sql);
char* translate_strftime(const char *sql);
char* translate_unixepoch(const char *sql);
char* translate_datetime(const char *sql);
char* translate_last_insert_rowid(const char *sql);
char* translate_json_each(const char *sql);
char* simplify_typeof_fixup(const char *sql);

// ============================================================================
// Query Structure Translations (sql_tr_query.c)
// ============================================================================

char* fix_group_by_strict(const char *sql);
char* fix_group_by_strict_complete(const char *sql);  // Complete GROUP BY rewriter
char* add_subquery_alias(const char *sql);
char* translate_case_booleans(const char *sql);
char* translate_max_to_greatest(const char *sql);
char* translate_min_to_least(const char *sql);
char* translate_fts(const char *sql);
char* fix_forward_reference_joins(const char *sql);
char* translate_distinct_orderby(const char *sql);
char* translate_null_sorting(const char *sql);
char* fix_integer_text_mismatch(const char *sql);
char* fix_duplicate_assignments(const char *sql);
char* strip_icu_collation(const char *sql);
char* fix_json_operator_on_text(const char *sql);
char* fix_collections_query(const char *sql);

// ============================================================================
// Quote Translations (sql_tr_quotes.c)
// ============================================================================

char* translate_backticks(const char *sql);
char* translate_column_quotes(const char *sql);
char* translate_alias_quotes(const char *sql);
char* translate_ddl_quotes(const char *sql);
char* add_if_not_exists(const char *sql);
char* fix_on_conflict_quotes(const char *sql);

// ============================================================================
// Keyword Translations (sql_tr_keywords.c)
// ============================================================================

char* fix_operator_spacing(const char *sql);

// ============================================================================
// UPSERT Translations (sql_tr_upsert.c)
// ============================================================================

char* translate_insert_or_replace(const char *sql);

#endif // SQL_TRANSLATOR_INTERNAL_H
