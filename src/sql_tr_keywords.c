/*
 * SQL Translator - Keyword Translations
 * Converts SQLite keywords and operators to PostgreSQL
 */

#include "sql_translator.h"
#include "sql_translator_internal.h"
#include <stdint.h>

// ============================================================================
// Check if string starts with a SQL keyword
// ============================================================================

// Pre-computed keyword lengths to avoid strlen() in loop
// Grouped by first character for O(1) first-char lookup
static const struct { const char *word; size_t len; } keywords[] = {
    {"from", 4}, {"where", 5}, {"join", 4}, {"inner", 5}, {"outer", 5},
    {"left", 4}, {"right", 5}, {"cross", 5}, {"on", 2}, {"and", 3},
    {"or", 2}, {"not", 3}, {"in", 2}, {"like", 4}, {"between", 7},
    {"order", 5}, {"group", 5}, {"having", 6}, {"limit", 5}, {"offset", 6},
    {"union", 5}, {"except", 6}, {"intersect", 9}, {"as", 2}, {"into", 4},
    {"values", 6}, {"set", 3}, {"delete", 6}, {"update", 6}, {"insert", 6},
    {NULL, 0}
};

// Bitmap of valid keyword first chars: a,b,c,d,e,f,g,h,i,j,l,n,o,r,s,u,v,w
// Bit position = char - 'a'
static const uint32_t keyword_first_chars =
    (1 << ('a'-'a')) | (1 << ('b'-'a')) | (1 << ('c'-'a')) | (1 << ('d'-'a')) |
    (1 << ('e'-'a')) | (1 << ('f'-'a')) | (1 << ('g'-'a')) | (1 << ('h'-'a')) |
    (1 << ('i'-'a')) | (1 << ('j'-'a')) | (1 << ('l'-'a')) | (1 << ('n'-'a')) |
    (1 << ('o'-'a')) | (1 << ('r'-'a')) | (1 << ('s'-'a')) | (1 << ('u'-'a')) |
    (1 << ('v'-'a')) | (1 << ('w'-'a'));

static int starts_with_keyword(const char *p) {
    // Fast path: check if first char could start a keyword
    char c = *p | 0x20;  // tolower without branch
    if (c < 'a' || c > 'z') return 0;
    if (!(keyword_first_chars & (1 << (c - 'a')))) return 0;

    // Only check keywords starting with this character
    for (int i = 0; keywords[i].word; i++) {
        if ((keywords[i].word[0] | 0x20) != c) continue;  // Skip non-matching first char
        if (strncasecmp(p, keywords[i].word, keywords[i].len) == 0) {
            char next = p[keywords[i].len];
            if (next == '\0' || next == ' ' || next == '\t' || next == '\n' ||
                next == '(' || next == ')' || next == ',') {
                return 1;
            }
        }
    }
    return 0;
}

// ============================================================================
// Fix operator spacing: !=-1 -> != -1
// ============================================================================

char* fix_operator_spacing(const char *sql) {
    if (!sql) return NULL;

    // Fast path: if no operator-minus patterns exist, just return a copy
    // This avoids processing huge queries that don't need fixing
    int needs_fix = 0;
    for (const char *scan = sql; *scan; scan++) {
        if ((*scan == '=' || *scan == '>' || *scan == '<' || *scan == '!') &&
            scan[1] == '-' && isdigit(scan[2])) {
            needs_fix = 1;
            break;
        }
        // Also check two-char operators: !=, <>, >=, <=
        if ((scan[0] == '!' && scan[1] == '=' && scan[2] == '-' && isdigit(scan[3])) ||
            (scan[0] == '<' && scan[1] == '>' && scan[2] == '-' && isdigit(scan[3])) ||
            (scan[0] == '>' && scan[1] == '=' && scan[2] == '-' && isdigit(scan[3])) ||
            (scan[0] == '<' && scan[1] == '=' && scan[2] == '-' && isdigit(scan[3]))) {
            needs_fix = 1;
            break;
        }
    }
    if (!needs_fix) {
        return strdup(sql);
    }

    char *result = malloc(strlen(sql) * 2 + 1);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;
    int in_string = 0;
    char string_char = 0;

    while (*p) {
        // Track string literals
        if ((*p == '\'' || *p == '"') && (p == sql || *(p-1) != '\\')) {
            if (!in_string) {
                in_string = 1;
                string_char = *p;
            } else if (*p == string_char) {
                in_string = 0;
                *out++ = *p++;
                if (!in_string && *p && starts_with_keyword(p)) {
                    *out++ = ' ';
                }
                continue;
            }
            *out++ = *p++;
            continue;
        }

        if (in_string) {
            *out++ = *p++;
            continue;
        }

        // Two-char operators followed by -digit
        if ((p[0] == '!' && p[1] == '=' && p[2] == '-' && isdigit(p[3])) ||
            (p[0] == '<' && p[1] == '>' && p[2] == '-' && isdigit(p[3])) ||
            (p[0] == '>' && p[1] == '=' && p[2] == '-' && isdigit(p[3])) ||
            (p[0] == '<' && p[1] == '=' && p[2] == '-' && isdigit(p[3]))) {
            *out++ = *p++;
            *out++ = *p++;
            *out++ = ' ';
            continue;
        }

        // Single char operators followed by -digit
        if ((p[0] == '=' && p[1] == '-' && isdigit(p[2]) && (p == sql || (p[-1] != '!' && p[-1] != '>' && p[-1] != '<'))) ||
            (p[0] == '>' && p[1] == '-' && isdigit(p[2]) && (p == sql || p[-1] != '<')) ||
            (p[0] == '<' && p[1] == '-' && isdigit(p[2]) && (p == sql || p[-1] != '>'))) {
            *out++ = *p++;
            *out++ = ' ';
            continue;
        }

        *out++ = *p++;
    }

    *out = '\0';
    return result;
}

// ============================================================================
// Main Keyword Translation
// ============================================================================

char* sql_translate_keywords(const char *sql) {
    if (!sql) return NULL;

    char *current = strdup(sql);
    if (!current) return NULL;

    char *temp;

    // ALTER TABLE ADD COLUMN -> ADD COLUMN IF NOT EXISTS
    // SQLite doesn't support IF NOT EXISTS for columns, but PostgreSQL does
    // This prevents "duplicate column" errors when Plex reruns migrations
    if (strcasestr(current, "ALTER TABLE") && strcasestr(current, " ADD ")) {
        // Pattern: ALTER TABLE 'x' ADD 'col' -> ALTER TABLE 'x' ADD COLUMN IF NOT EXISTS 'col'
        // Also handle: ALTER TABLE "x" ADD "col"
        temp = str_replace_nocase(current, " ADD '", " ADD COLUMN IF NOT EXISTS '");
        if (temp) { free(current); current = temp; }
        temp = str_replace_nocase(current, " ADD \"", " ADD COLUMN IF NOT EXISTS \"");
        if (temp) { free(current); current = temp; }
        // Handle unquoted column names
        if (!strcasestr(current, "IF NOT EXISTS")) {
            temp = str_replace_nocase(current, " ADD ", " ADD COLUMN IF NOT EXISTS ");
            if (temp) { free(current); current = temp; }
        }
    }

    // Transaction modes
    temp = str_replace_nocase(current, "BEGIN IMMEDIATE", "BEGIN");
    free(current);
    current = temp;

    temp = str_replace_nocase(current, "BEGIN DEFERRED", "BEGIN");
    free(current);
    current = temp;

    temp = str_replace_nocase(current, "BEGIN EXCLUSIVE", "BEGIN");
    free(current);
    current = temp;

    // NOTE: INSERT OR REPLACE is handled by translate_insert_or_replace() in sql_tr_upsert.c
    // We DO NOT translate it here to avoid conflicts with REPLACE INTO translation below.

    // INSERT OR IGNORE -> INSERT (with ON CONFLICT DO NOTHING added later)
    temp = str_replace_nocase(current, "INSERT OR IGNORE INTO", "INSERT INTO");
    free(current);
    current = temp;

    // REPLACE INTO -> INSERT INTO (standalone REPLACE, not part of INSERT OR REPLACE)
    // This handles SQLite's REPLACE INTO syntax which is equivalent to INSERT OR REPLACE
    // But we only do this AFTER checking for INSERT OR REPLACE above
    if (!strcasestr(current, "INSERT OR")) {
        temp = str_replace_nocase(current, "REPLACE INTO", "INSERT INTO");
        free(current);
        current = temp;
    }

    // GLOB -> LIKE
    temp = str_replace_nocase(current, " GLOB ", " LIKE ");
    free(current);
    current = temp;

    // AS 'alias' -> AS "alias"
    temp = translate_alias_quotes(current);
    free(current);
    current = temp;

    // table.'column' -> table."column"
    temp = translate_column_quotes(current);
    free(current);
    current = temp;

    // `column` -> "column"
    temp = translate_backticks(current);
    free(current);
    current = temp;

    // Remove COLLATE icu_root
    temp = str_replace_nocase(current, " collate icu_root", "");
    free(current);
    current = temp;

    // Fix empty IN clause - use empty subquery that returns no rows
    // IN () in SQLite returns 0 rows (empty set), PostgreSQL rejects the syntax
    // Use integer literal -1 instead of NULL to avoid type inference issues
    // (-1 will never match any positive ID, and the WHERE FALSE ensures no rows)
    // Use case-insensitive replace to handle both "IN" and "in"
    temp = str_replace_nocase(current, " in ()", " IN (SELECT -1 WHERE FALSE)");
    free(current);
    current = temp;
    temp = str_replace_nocase(current, " in (  )", " IN (SELECT -1 WHERE FALSE)");
    free(current);
    current = temp;
    temp = str_replace_nocase(current, " in ( )", " IN (SELECT -1 WHERE FALSE)");
    free(current);
    current = temp;

    // Fix GROUP BY NULL
    temp = str_replace_nocase(current, " GROUP BY NULL", "");
    free(current);
    current = temp;

    // Fix HAVING with aliases
    temp = str_replace_nocase(current, " HAVING cnt = 0", " HAVING count(media_items.id) = 0");
    free(current);
    current = temp;

    // Translate sqlite_master
    if (strcasestr(current, "sqlite_master") || strcasestr(current, "sqlite_schema")) {
        const char *sqlite_master_pg =
            "(SELECT "
            "CASE WHEN table_type = 'BASE TABLE' THEN 'table' "
            "     WHEN table_type = 'VIEW' THEN 'view' END AS type, "
            "table_name AS name, "
            "table_name AS tbl_name, "
            "0 AS rootpage, "
            "'' AS sql "
            "FROM information_schema.tables "
            "WHERE table_schema = current_schema() "
            "UNION ALL "
            "SELECT 'index' AS type, "
            "indexname AS name, "
            "tablename AS tbl_name, "
            "0 AS rootpage, "
            "indexdef AS sql "
            "FROM pg_indexes "
            "WHERE schemaname = current_schema()) AS _sqlite_master_";

        temp = str_replace_nocase(current, "\"main\".sqlite_master", sqlite_master_pg);
        if (strcmp(temp, current) != 0) {
            free(current);
            current = temp;
        } else {
            free(temp);
            temp = str_replace_nocase(current, "main.sqlite_master", sqlite_master_pg);
            if (strcmp(temp, current) != 0) {
                free(current);
                current = temp;
            } else {
                free(temp);
                temp = str_replace_nocase(current, "sqlite_master", sqlite_master_pg);
                if (strcmp(temp, current) != 0) {
                    free(current);
                    current = temp;
                } else {
                    free(temp);
                    temp = str_replace_nocase(current, "sqlite_schema", sqlite_master_pg);
                    free(current);
                    current = temp;
                }
            }
        }

        temp = str_replace_nocase(current, " ORDER BY rowid", "");
        free(current);
        current = temp;
    }

    // Remove INDEXED BY hints
    temp = current;
    char *indexed_pos;
    while ((indexed_pos = strcasestr(temp, " indexed by ")) != NULL) {
        char *end = indexed_pos + 12;
        while (*end && !isspace(*end) && *end != ')' && *end != ',') end++;

        size_t prefix_len = indexed_pos - temp;
        size_t suffix_len = strlen(end);
        char *new_str = malloc(prefix_len + suffix_len + 1);
        if (new_str) {
            memcpy(new_str, temp, prefix_len);
            memcpy(new_str + prefix_len, end, suffix_len);
            new_str[prefix_len + suffix_len] = '\0';
            if (temp != current) free(temp);
            temp = new_str;
        } else {
            break;
        }
    }
    if (temp != current) {
        free(current);
        current = temp;
    }

    return current;
}
