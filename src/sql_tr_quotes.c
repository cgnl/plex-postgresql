/*
 * SQL Translator - Quote/Identifier Translations
 * Converts SQLite identifier quoting to PostgreSQL style
 */

#include "sql_translator_internal.h"

// ============================================================================
// Translate backticks `column` -> "column"
// ============================================================================

char* translate_backticks(const char *sql) {
    if (!sql) return NULL;

    char *result = malloc(strlen(sql) + 1);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;

    while (*p) {
        if (*p == '`') {
            *out++ = '"';
        } else {
            *out++ = *p;
        }
        p++;
    }

    *out = '\0';
    return result;
}

// ============================================================================
// Translate table.'column' -> table."column"
// ============================================================================

char* translate_column_quotes(const char *sql) {
    if (!sql) return NULL;

    char *result = malloc(strlen(sql) * 2 + 1);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;
    int in_string = 0;

    while (*p) {
        // Check for table.'column' pattern
        if (*p == '\'' && p > sql && *(p-1) == '.') {
            *out++ = '"';
            p++;

            while (*p && *p != '\'') {
                *out++ = *p++;
            }

            if (*p == '\'') {
                *out++ = '"';
                p++;
            }
            continue;
        }

        // Track regular string literals
        if (*p == '\'' && !in_string) {
            in_string = 1;
            *out++ = *p++;
            continue;
        }
        if (*p == '\'' && in_string) {
            if (*(p+1) == '\'') {
                *out++ = *p++;
                *out++ = *p++;
                continue;
            }
            in_string = 0;
        }

        *out++ = *p++;
    }

    *out = '\0';
    return result;
}

// ============================================================================
// Translate AS 'alias' -> AS "alias"
// ============================================================================

char* translate_alias_quotes(const char *sql) {
    if (!sql) return NULL;

    char *result = malloc(strlen(sql) * 2 + 1);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;
    int in_string = 0;
    char string_char = 0;

    while (*p) {
        if ((*p == '\'' || *p == '"') && !in_string) {
            const char *back = p - 1;
            while (back > sql && isspace(*back)) back--;

            // Check if preceded by AS
            if (back >= sql + 1 &&
                (back[-1] == 'a' || back[-1] == 'A') &&
                (back[0] == 's' || back[0] == 'S') &&
                (back == sql + 1 || !is_ident_char(back[-2]))) {

                if (*p == '\'') {
                    *out++ = '"';
                    p++;

                    while (*p && *p != '\'') {
                        *out++ = *p++;
                    }

                    if (*p == '\'') {
                        *out++ = '"';
                        p++;
                    }
                    continue;
                }
            }

            in_string = 1;
            string_char = *p;
            *out++ = *p++;
            continue;
        }

        if (in_string && *p == string_char) {
            if (*(p+1) == string_char) {
                *out++ = *p++;
                *out++ = *p++;
                continue;
            }
            in_string = 0;
        }

        *out++ = *p++;
    }

    *out = '\0';
    return result;
}

// ============================================================================
// Translate DDL single-quoted identifiers -> double-quoted
// ============================================================================

char* translate_ddl_quotes(const char *sql) {
    if (!sql) return NULL;

    const char *s = sql;
    while (*s && isspace(*s)) s++;
    int is_ddl = (strncasecmp(s, "CREATE", 6) == 0 ||
                  strncasecmp(s, "DROP", 4) == 0 ||
                  strncasecmp(s, "ALTER", 5) == 0);

    if (!is_ddl) {
        return strdup(sql);
    }

    char *result = malloc(strlen(sql) * 2 + 1);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;
    int in_parens = 0;

    while (*p) {
        if (*p == '(') in_parens++;
        if (*p == ')') in_parens--;

        if (*p == '\'') {
            const char *back = p - 1;
            while (back > sql && isspace(*back)) back--;

            int is_identifier = 0;

            if (back >= sql) {
                if ((back >= sql + 4 && strncasecmp(back - 4, "TABLE", 5) == 0) ||
                    (back >= sql + 4 && strncasecmp(back - 4, "INDEX", 5) == 0) ||
                    (back >= sql + 1 && strncasecmp(back - 1, "ON", 2) == 0) ||
                    (back >= sql + 5 && strncasecmp(back - 5, "UNIQUE", 6) == 0) ||
                    (back >= sql + 2 && strncasecmp(back - 2, "ADD", 3) == 0) ||
                    (back >= sql + 5 && strncasecmp(back - 5, "COLUMN", 6) == 0) ||
                    (back >= sql + 3 && strncasecmp(back - 3, "DROP", 4) == 0) ||
                    *back == '(' || *back == ',' || *back == '.') {
                    is_identifier = 1;
                }
            }

            if (p > sql) {
                const char *keyword = sql;
                while (*keyword && isspace(*keyword)) keyword++;
                if ((strncasecmp(keyword, "CREATE TABLE ", 13) == 0 ||
                     strncasecmp(keyword, "CREATE INDEX ", 13) == 0 ||
                     strncasecmp(keyword, "CREATE UNIQUE INDEX ", 20) == 0) &&
                    p > keyword) {
                    const char *check = p - 1;
                    while (check > keyword && isspace(*check)) check--;
                    if (check > keyword && (
                        (check >= keyword + 4 && strncasecmp(check - 4, "TABLE", 5) == 0) ||
                        (check >= keyword + 4 && strncasecmp(check - 4, "INDEX", 5) == 0))) {
                        is_identifier = 1;
                    }
                }
            }

            if (in_parens > 0 && back >= sql && (*back == '(' || *back == ',')) {
                is_identifier = 1;
            }

            if (is_identifier) {
                *out++ = '"';
                p++;

                while (*p && *p != '\'') {
                    *out++ = *p++;
                }

                if (*p == '\'') {
                    *out++ = '"';
                    p++;
                }
                continue;
            }
        }

        *out++ = *p++;
    }

    *out = '\0';
    return result;
}

// ============================================================================
// Add IF NOT EXISTS to CREATE TABLE/INDEX
// ============================================================================

char* add_if_not_exists(const char *sql) {
    if (!sql) return NULL;

    const char *s = sql;
    while (*s && isspace(*s)) s++;

    // CREATE TABLE
    if (strncasecmp(s, "CREATE TABLE ", 13) == 0 &&
        strncasecmp(s + 13, "IF NOT EXISTS ", 14) != 0) {
        size_t prefix_len = (s - sql) + 12;
        size_t rest_len = strlen(s + 12);
        char *result = malloc(prefix_len + 15 + rest_len + 1);
        if (!result) return NULL;

        memcpy(result, sql, prefix_len);
        memcpy(result + prefix_len, " IF NOT EXISTS", 14);
        strcpy(result + prefix_len + 14, s + 12);
        return result;
    }

    // CREATE INDEX
    if (strncasecmp(s, "CREATE INDEX ", 13) == 0 &&
        strncasecmp(s + 13, "IF NOT EXISTS ", 14) != 0) {
        size_t prefix_len = (s - sql) + 12;
        size_t rest_len = strlen(s + 12);
        char *result = malloc(prefix_len + 15 + rest_len + 1);
        if (!result) return NULL;

        memcpy(result, sql, prefix_len);
        memcpy(result + prefix_len, " IF NOT EXISTS", 14);
        strcpy(result + prefix_len + 14, s + 12);
        return result;
    }

    // CREATE UNIQUE INDEX
    if (strncasecmp(s, "CREATE UNIQUE INDEX ", 20) == 0 &&
        strncasecmp(s + 20, "IF NOT EXISTS ", 14) != 0) {
        size_t prefix_len = (s - sql) + 19;
        size_t rest_len = strlen(s + 19);
        char *result = malloc(prefix_len + 15 + rest_len + 1);
        if (!result) return NULL;

        memcpy(result, sql, prefix_len);
        memcpy(result + prefix_len, " IF NOT EXISTS", 14);
        strcpy(result + prefix_len + 14, s + 19);
        return result;
    }

    return strdup(sql);
}

// ============================================================================
// Fix ON CONFLICT quotes: ON CONFLICT("name") -> ON CONFLICT(name)
// PostgreSQL requires unquoted column names in ON CONFLICT clause
// ============================================================================

char* fix_on_conflict_quotes(const char *sql) {
    if (!sql) return NULL;

    // Quick check if there's even an ON CONFLICT clause
    const char *on_conflict = strcasestr(sql, "ON CONFLICT");
    if (!on_conflict) {
        return strdup(sql);
    }

    char *result = malloc(strlen(sql) + 1);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;
    int in_string = 0;
    char string_char = 0;
    int inside_on_conflict_parens = 0;
    int paren_depth = 0;

    while (*p) {
        // Track ON CONFLICT clause start
        if (!in_string && !inside_on_conflict_parens) {
            if (strncasecmp(p, "ON CONFLICT", 11) == 0) {
                // Copy "ON CONFLICT"
                memcpy(out, p, 11);
                out += 11;
                p += 11;

                // Skip whitespace
                while (*p && (*p == ' ' || *p == '\t' || *p == '\n')) {
                    *out++ = *p++;
                }

                // Check if opening paren follows
                if (*p == '(') {
                    *out++ = *p++;
                    inside_on_conflict_parens = 1;
                    paren_depth = 1;
                }
                continue;
            }
        }

        // Inside ON CONFLICT parentheses
        if (inside_on_conflict_parens && !in_string) {
            // Track paren depth
            if (*p == '(') {
                paren_depth++;
                *out++ = *p++;
                continue;
            } else if (*p == ')') {
                paren_depth--;
                *out++ = *p++;
                if (paren_depth == 0) {
                    inside_on_conflict_parens = 0;
                }
                continue;
            }

            // Remove quotes around identifiers
            if (*p == '"') {
                // Skip the opening quote
                p++;
                // Copy the identifier without quotes
                while (*p && *p != '"') {
                    *out++ = *p++;
                }
                // Skip the closing quote
                if (*p == '"') p++;
                continue;
            }
        }

        // Track string literals (single quotes)
        if (*p == '\'' && (p == sql || *(p-1) != '\\')) {
            if (!in_string) {
                in_string = 1;
                string_char = '\'';
            } else if (string_char == '\'') {
                // Check for escaped quotes
                if (*(p+1) == '\'') {
                    *out++ = *p++;
                    *out++ = *p++;
                    continue;
                }
                in_string = 0;
            }
        }

        *out++ = *p++;
    }

    *out = '\0';
    return result;
}

// ============================================================================
// Fix duplicate column assignments in UPDATE statements
// UPDATE table SET col=1, col=2 -> Keep only the last assignment
// ============================================================================

char* fix_duplicate_assignments(const char *sql) {
    if (!sql) return NULL;

    // Quick check - only applies to UPDATE statements
    const char *s = sql;
    while (*s && isspace(*s)) s++;
    if (strncasecmp(s, "UPDATE", 6) != 0) {
        return strdup(sql);
    }

    // Find the SET clause
    const char *set_pos = strcasestr(sql, " SET ");
    if (!set_pos) {
        return strdup(sql);
    }

    // Find WHERE clause (end of SET clause)
    const char *where_pos = strcasestr(set_pos, " WHERE ");
    const char *set_end = where_pos ? where_pos : (sql + strlen(sql));

    // Parse assignments in SET clause
    // NOTE: Using heap allocation instead of stack to prevent stack overflow
    // when called from deeply nested translation chain (38KB on stack was too much)
    #define MAX_ASSIGNMENTS 256
    typedef struct {
        char column[128];
        const char *value_start;
        const char *value_end;
        int keep;
    } assignment_t;

    assignment_t *assignments = calloc(MAX_ASSIGNMENTS, sizeof(assignment_t));
    if (!assignments) {
        return strdup(sql);
    }
    int assign_count = 0;

    const char *p = set_pos + 5;  // Skip " SET "
    while (p < set_end && assign_count < MAX_ASSIGNMENTS) {
        // Skip whitespace
        while (*p && isspace(*p) && p < set_end) p++;
        if (p >= set_end) break;

        // Extract column name (may be quoted)
        const char *col_start = p;
        if (*p == '"') {
            p++;
            while (*p && *p != '"' && p < set_end) p++;
            if (*p == '"') p++;
        } else {
            while (*p && !isspace(*p) && *p != '=' && p < set_end) p++;
        }

        size_t col_len = p - col_start;
        if (col_len >= sizeof(assignments[0].column)) col_len = sizeof(assignments[0].column) - 1;

        // Normalize column name (remove quotes)
        char column[128] = {0};
        const char *src = col_start;
        char *dst = column;
        while (src < col_start + col_len && dst < column + sizeof(column) - 1) {
            if (*src != '"') {
                *dst++ = tolower(*src);
            }
            src++;
        }
        *dst = '\0';

        // Skip whitespace and '='
        while (*p && (isspace(*p) || *p == '=') && p < set_end) p++;

        // Find value (ends at ',' or WHERE)
        const char *value_start = p;
        int paren_depth = 0;
        int in_string = 0;
        while (p < set_end) {
            if (*p == '\'' && (p == sql || *(p-1) != '\\')) {
                in_string = !in_string;
            }
            if (!in_string) {
                if (*p == '(') paren_depth++;
                if (*p == ')') paren_depth--;
                if (*p == ',' && paren_depth == 0) break;
            }
            p++;
        }
        const char *value_end = p;

        // Store assignment
        strncpy(assignments[assign_count].column, column, sizeof(assignments[assign_count].column) - 1);
        assignments[assign_count].value_start = value_start;
        assignments[assign_count].value_end = value_end;
        assignments[assign_count].keep = 1;
        assign_count++;

        // Skip comma
        if (*p == ',') p++;
    }

    // Mark duplicates - keep only the LAST occurrence of each column
    for (int i = 0; i < assign_count; i++) {
        for (int j = i + 1; j < assign_count; j++) {
            if (strcmp(assignments[i].column, assignments[j].column) == 0) {
                assignments[i].keep = 0;  // Remove earlier occurrence
                break;
            }
        }
    }

    // Count how many assignments we're keeping
    int keep_count = 0;
    for (int i = 0; i < assign_count; i++) {
        if (assignments[i].keep) keep_count++;
    }

    // If no duplicates, return original
    if (keep_count == assign_count) {
        free(assignments);
        return strdup(sql);
    }

    // Rebuild SQL with deduplicated assignments
    size_t result_len = strlen(sql) + 1;
    char *result = malloc(result_len);
    if (!result) {
        free(assignments);
        return strdup(sql);
    }

    // Copy up to SET clause
    size_t prefix_len = (set_pos + 5) - sql;
    memcpy(result, sql, prefix_len);
    char *out = result + prefix_len;

    // Copy kept assignments
    int first = 1;
    for (int i = 0; i < assign_count; i++) {
        if (!assignments[i].keep) continue;

        if (!first) {
            *out++ = ',';
            *out++ = ' ';
        }
        first = 0;

        // Find original column name with quotes from source
        const char *orig_col_start = assignments[i].value_start;
        while (orig_col_start > sql && *orig_col_start != '=' && *orig_col_start != ',') orig_col_start--;
        if (*orig_col_start == '=' || *orig_col_start == ',') orig_col_start++;
        while (*orig_col_start && isspace(*orig_col_start)) orig_col_start++;

        const char *orig_col_end = orig_col_start;
        if (*orig_col_end == '"') {
            orig_col_end++;
            while (*orig_col_end && *orig_col_end != '"') orig_col_end++;
            if (*orig_col_end == '"') orig_col_end++;
        } else {
            while (*orig_col_end && !isspace(*orig_col_end) && *orig_col_end != '=') orig_col_end++;
        }

        // Copy column name
        size_t col_len = orig_col_end - orig_col_start;
        memcpy(out, orig_col_start, col_len);
        out += col_len;

        // Copy = and value
        *out++ = '=';
        size_t value_len = assignments[i].value_end - assignments[i].value_start;
        memcpy(out, assignments[i].value_start, value_len);
        out += value_len;
    }

    // Copy rest of SQL (WHERE clause, etc.)
    if (where_pos) {
        strcpy(out, where_pos);
    } else {
        *out = '\0';
    }

    free(assignments);
    return result;
}
