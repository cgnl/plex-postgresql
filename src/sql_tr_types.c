/*
 * SQL Translator - Type Translations
 * Converts SQLite types to PostgreSQL equivalents
 */

#include "sql_translator.h"
#include "sql_translator_internal.h"

// ============================================================================
// Type Translation
// ============================================================================

char* sql_translate_types(const char *sql) {
    if (!sql) return NULL;

    // Fast path: if no type-related patterns, skip all translations
    // These patterns only appear in DDL statements (CREATE TABLE, etc.)
    if (!strcasestr(sql, "autoincrement") &&
        !strcasestr(sql, "dt_integer") &&
        !strcasestr(sql, "integer(8)") &&
        !strcasestr(sql, " blob") &&
        !strcasestr(sql, " boolean")) {
        return strdup(sql);
    }

    char *current = strdup(sql);
    if (!current) return NULL;

    char *temp;

    // INTEGER PRIMARY KEY AUTOINCREMENT -> SERIAL PRIMARY KEY
    temp = str_replace_nocase(current, "INTEGER PRIMARY KEY AUTOINCREMENT", "SERIAL PRIMARY KEY");
    free(current);
    current = temp;

    // Remove AUTOINCREMENT (handled by SERIAL)
    temp = str_replace_nocase(current, "AUTOINCREMENT", "");
    free(current);
    current = temp;

    // dt_integer(8) -> BIGINT
    temp = str_replace_nocase(current, "dt_integer(8)", "BIGINT");
    free(current);
    current = temp;

    // integer(8) -> BIGINT
    temp = str_replace_nocase(current, "integer(8)", "BIGINT");
    free(current);
    current = temp;

    // BLOB -> BYTEA (only in DDL context, not table names)
    temp = str_replace_nocase(current, " BLOB)", " BYTEA)");
    free(current);
    current = temp;

    temp = str_replace_nocase(current, " BLOB,", " BYTEA,");
    free(current);
    current = temp;

    temp = str_replace_nocase(current, " BLOB ", " BYTEA ");
    free(current);
    current = temp;

    // boolean DEFAULT 't' -> boolean DEFAULT TRUE
    temp = str_replace(current, "DEFAULT 't'", "DEFAULT TRUE");
    free(current);
    current = temp;

    temp = str_replace(current, "DEFAULT 'f'", "DEFAULT FALSE");
    free(current);
    current = temp;

    // datetime -> TIMESTAMP
    temp = str_replace_nocase(current, " datetime)", " TIMESTAMP)");
    free(current);
    current = temp;

    temp = str_replace_nocase(current, " datetime,", " TIMESTAMP,");
    free(current);
    current = temp;

    temp = str_replace_nocase(current, " datetime ", " TIMESTAMP ");
    free(current);
    current = temp;

    return current;
}
