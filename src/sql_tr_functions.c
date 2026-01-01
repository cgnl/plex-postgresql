/*
 * SQL Translator - Function Translations
 * Converts SQLite functions to PostgreSQL equivalents
 */

#include "sql_translator_internal.h"

// ============================================================================
// iif(cond, true_val, false_val) -> CASE WHEN cond THEN true_val ELSE false_val END
// ============================================================================

char* translate_iif(const char *sql) {
    char *result = malloc(MAX_SQL_LEN);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;

    while (*p) {
        // Find iif( case-insensitively
        if (strncasecmp(p, "iif(", 4) == 0) {
            const char *args_start = p + 4;
            char cond[4096] = {0};
            char true_val[4096] = {0};
            char false_val[4096] = {0};

            // Extract condition
            const char *next = extract_arg(args_start, cond, sizeof(cond));
            if (*next == ',') next++;

            // Extract true value
            next = extract_arg(next, true_val, sizeof(true_val));
            if (*next == ',') next++;

            // Extract false value
            next = extract_arg(next, false_val, sizeof(false_val));
            if (*next == ')') next++;

            // Write CASE WHEN
            out += sprintf(out, "CASE WHEN %s THEN %s ELSE %s END", cond, true_val, false_val);
            p = next;
        } else {
            *out++ = *p++;
        }
    }

    *out = '\0';
    return result;
}

// ============================================================================
// typeof(x) -> pg_typeof(x)::text
// ============================================================================

char* translate_typeof(const char *sql) {
    // Fast path: if no "typeof" in query, nothing to translate
    if (!strcasestr(sql, "typeof(")) {
        return strdup(sql);
    }

    char *temp = str_replace_nocase(sql, "typeof(", "pg_typeof(");
    if (!temp) return NULL;

    // Add ::text cast after pg_typeof(...)
    char *result = malloc(MAX_SQL_LEN);
    if (!result) { free(temp); return NULL; }

    char *out = result;
    const char *p = temp;

    while (*p) {
        if (strncasecmp(p, "pg_typeof(", 10) == 0) {
            // Copy pg_typeof(
            memcpy(out, p, 10);
            out += 10;
            p += 10;

            // Find matching )
            int depth = 1;
            while (*p && depth > 0) {
                if (*p == '(') depth++;
                else if (*p == ')') depth--;
                *out++ = *p++;
            }

            // Add ::text
            memcpy(out, "::text", 6);
            out += 6;
        } else {
            *out++ = *p++;
        }
    }

    *out = '\0';
    free(temp);

    // Translate type names in comparisons
    char *temp2 = str_replace_nocase(result, "in ('integer',", "in ('integer', 'bigint',");
    if (temp2) {
        free(result);
        result = temp2;
    }
    temp2 = str_replace_nocase(result, "in ( 'integer',", "in ('integer', 'bigint',");
    if (temp2) {
        free(result);
        result = temp2;
    }
    temp2 = str_replace(result, "'real'", "'double precision'");
    free(result);

    return temp2;
}

// ============================================================================
// strftime('%s', x) -> EXTRACT(EPOCH FROM x)::bigint
// ============================================================================

char* translate_strftime(const char *sql) {
    char *result = malloc(MAX_SQL_LEN);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;

    while (*p) {
        if (strncasecmp(p, "strftime(", 9) == 0) {
            const char *args_start = p + 9;
            char format[256] = {0};
            char value[4096] = {0};
            char extra[256] = {0};

            // Extract format string
            const char *next = extract_arg(args_start, format, sizeof(format));
            if (*next == ',') next++;

            // Extract value
            next = extract_arg(next, value, sizeof(value));

            // Check for extra argument (like 'utc')
            if (*next == ',') {
                next++;
                next = extract_arg(next, extra, sizeof(extra));
            }
            if (*next == ')') next++;

            // Translate based on format
            if (strcmp(format, "'%s'") == 0) {
                // Unix timestamp
                out += sprintf(out, "EXTRACT(EPOCH FROM %s)::bigint", value);
            } else if (strcmp(format, "'%Y-%m-%d'") == 0) {
                out += sprintf(out, "TO_CHAR(%s, 'YYYY-MM-DD')", value);
            } else if (strcmp(format, "'%Y-%m-%d %H:%M:%S'") == 0) {
                out += sprintf(out, "TO_CHAR(%s, 'YYYY-MM-DD HH24:MI:SS')", value);
            } else {
                // Generic: try to convert format
                out += sprintf(out, "TO_CHAR(%s, %s)", value, format);
            }

            p = next;
        } else {
            *out++ = *p++;
        }
    }

    *out = '\0';
    return result;
}

// ============================================================================
// unixepoch('now', '-7 day') -> EXTRACT(EPOCH FROM NOW() - INTERVAL '7 days')::bigint
// ============================================================================

char* translate_unixepoch(const char *sql) {
    char *result = malloc(MAX_SQL_LEN);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;

    while (*p) {
        if (strncasecmp(p, "unixepoch(", 10) == 0) {
            const char *args_start = p + 10;
            char arg1[256] = {0};
            char arg2[256] = {0};

            // Extract first argument
            const char *next = extract_arg(args_start, arg1, sizeof(arg1));

            // Check for second argument
            if (*next == ',') {
                next++;
                next = extract_arg(next, arg2, sizeof(arg2));
            }
            if (*next == ')') next++;

            // Parse interval from arg2 if present
            if (strcasecmp(arg1, "'now'") == 0) {
                if (arg2[0]) {
                    // Parse interval like '-7 day' or '+1 hour'
                    char interval[256];
                    if (arg2[0] == '\'') {
                        strncpy(interval, arg2 + 1, sizeof(interval) - 1);
                        char *q = strrchr(interval, '\'');
                        if (q) *q = '\0';
                    } else {
                        strncpy(interval, arg2, sizeof(interval) - 1);
                    }

                    out += sprintf(out, "EXTRACT(EPOCH FROM NOW() + INTERVAL '%s')::bigint", interval);
                } else {
                    out += sprintf(out, "EXTRACT(EPOCH FROM NOW())::bigint");
                }
            } else {
                // Generic case
                out += sprintf(out, "EXTRACT(EPOCH FROM %s)::bigint", arg1);
            }

            p = next;
        } else {
            *out++ = *p++;
        }
    }

    *out = '\0';
    return result;
}

// ============================================================================
// datetime('now') -> NOW()
// ============================================================================

char* translate_datetime(const char *sql) {
    return str_replace_nocase(sql, "datetime('now')", "NOW()");
}

// ============================================================================
// last_insert_rowid() -> lastval()
// ============================================================================

char* translate_last_insert_rowid(const char *sql) {
    return str_replace_nocase(sql, "last_insert_rowid()", "lastval()");
}

// ============================================================================
// json_each() -> json_array_elements()
// ============================================================================

char* translate_json_each(const char *sql) {
    if (!sql) return NULL;

    char *result = malloc(MAX_SQL_LEN);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;

    while (*p) {
        // Look for json_each( - case insensitive
        if (strncasecmp(p, "json_each(", 10) == 0) {
            const char *args_start = p + 10;

            // Extract the JSON argument manually
            const char *scan = args_start;
            int depth = 0;
            int in_string = 0;
            char string_char = 0;

            while (*scan) {
                if (in_string) {
                    if (*scan == string_char && (scan == args_start || *(scan-1) != '\\')) {
                        in_string = 0;
                    }
                } else {
                    if (*scan == '\'' || *scan == '"') {
                        in_string = 1;
                        string_char = *scan;
                    } else if (*scan == '(') {
                        depth++;
                    } else if (*scan == ')') {
                        if (depth == 0) break;
                        depth--;
                    }
                }
                scan++;
            }

            // Copy the argument
            size_t arg_len = scan - args_start;
            char *arg = malloc(arg_len + 1);
            if (arg) {
                memcpy(arg, args_start, arg_len);
                arg[arg_len] = '\0';

                // Trim whitespace
                char *arg_start = arg;
                char *arg_end = arg + arg_len - 1;
                while (*arg_start && isspace(*arg_start)) arg_start++;
                while (arg_end > arg_start && isspace(*arg_end)) {
                    *arg_end = '\0';
                    arg_end--;
                }

                // Convert to json_array_elements with proper casting
                out += sprintf(out, "json_array_elements(%s::json)", arg_start);

                free(arg);
            }

            // Move past the closing )
            p = scan;
            if (*p == ')') p++;
        } else {
            *out++ = *p++;
        }
    }

    *out = '\0';

    // Fix references to the value column
    char *temp = str_replace(result, " value FROM json_array_elements", " (value::text)::integer FROM json_array_elements");
    if (temp) {
        free(result);
        result = temp;
    }

    return result;
}

// ============================================================================
// Simplify typeof-based fixup patterns
// iif(typeof(x) in ('integer', 'real'), x, strftime('%s', x, 'utc')) -> x
// ============================================================================

char* simplify_typeof_fixup(const char *sql) {
    if (!sql) return NULL;

    char *result = malloc(MAX_SQL_LEN);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;

    while (*p) {
        // Look for pattern: iif(typeof(
        if (strncasecmp(p, "iif(typeof(", 11) == 0) {
            const char *start = p;
            p += 11;

            // Extract the column/expression name (X)
            char col_name[256];
            int col_len = 0;
            int depth = 1;
            while (*p && depth > 0 && col_len < 255) {
                if (*p == '(') depth++;
                else if (*p == ')') depth--;
                if (depth > 0) col_name[col_len++] = *p;
                p++;
            }
            col_name[col_len] = '\0';

            // Check if this looks like the pattern we want
            if (strncasecmp(p, " in ('integer'", 14) == 0 ||
                strncasecmp(p, " in ( 'integer'", 15) == 0) {

                // Find the matching closing ) for the iif()
                int iif_depth = 1;
                const char *scan = start + 4;  // after "iif("
                while (*scan && iif_depth > 0) {
                    if (*scan == '(') iif_depth++;
                    else if (*scan == ')') iif_depth--;
                    scan++;
                }

                // If we found the complete iif(), replace with just col_name
                if (iif_depth == 0) {
                    strcpy(out, col_name);
                    out += strlen(col_name);
                    p = scan;
                    continue;
                }
            }

            // Not the pattern - copy original and continue
            p = start;
        }

        *out++ = *p++;
    }

    *out = '\0';
    return result;
}
