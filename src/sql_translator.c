/*
 * SQL Translator - SQLite to PostgreSQL
 *
 * Comprehensive translation of SQLite SQL to PostgreSQL.
 */

#include "sql_translator.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>  // for strcasestr
#include <ctype.h>
#include <regex.h>

#define MAX_SQL_LEN 131072
#define MAX_PARAMS 512
#define MAX_PARAM_NAME 64

// ============================================================================
// Helper Functions
// ============================================================================

static char* str_replace(const char *str, const char *old, const char *new_str) {
    if (!str || !old || !new_str) return NULL;

    size_t old_len = strlen(old);
    size_t new_len = strlen(new_str);

    // Count occurrences
    int count = 0;
    const char *p = str;
    while ((p = strstr(p, old)) != NULL) {
        count++;
        p += old_len;
    }

    if (count == 0) return strdup(str);

    // Allocate result
    size_t result_len = strlen(str) + count * (new_len - old_len) + 1;
    char *result = malloc(result_len);
    if (!result) return NULL;

    char *out = result;
    p = str;
    while (*p) {
        if (strncmp(p, old, old_len) == 0) {
            memcpy(out, new_str, new_len);
            out += new_len;
            p += old_len;
        } else {
            *out++ = *p++;
        }
    }
    *out = '\0';

    return result;
}

static char* str_replace_nocase(const char *str, const char *old, const char *new_str) {
    if (!str || !old || !new_str) return NULL;

    size_t old_len = strlen(old);
    size_t new_len = strlen(new_str);
    size_t str_len = strlen(str);

    // Find all occurrences (case insensitive)
    char *result = malloc(str_len * 2 + new_len * 10 + 1);
    if (!result) return NULL;

    char *out = result;
    const char *p = str;

    while (*p) {
        if (strncasecmp(p, old, old_len) == 0) {
            memcpy(out, new_str, new_len);
            out += new_len;
            p += old_len;
        } else {
            *out++ = *p++;
        }
    }
    *out = '\0';

    return result;
}

// Skip whitespace
static const char* skip_ws(const char *p) {
    while (*p && isspace(*p)) p++;
    return p;
}

// Check if character is part of identifier
static int is_ident_char(char c) {
    return isalnum(c) || c == '_';
}

// Extract function argument (handles nested parentheses)
static const char* extract_arg(const char *start, char *buf, size_t bufsize) {
    const char *p = start;
    int depth = 0;
    size_t i = 0;

    p = skip_ws(p);

    while (*p && i < bufsize - 1) {
        if (*p == '(') depth++;
        else if (*p == ')') {
            if (depth == 0) break;
            depth--;
        }
        else if (*p == ',' && depth == 0) break;

        buf[i++] = *p++;
    }

    // Trim trailing whitespace
    while (i > 0 && isspace(buf[i-1])) i--;
    buf[i] = '\0';

    return p;
}

// ============================================================================
// Placeholder Translation (? and :name -> $1, $2, ...)
// ============================================================================

char* sql_translate_placeholders(const char *sql, char ***param_names_out, int *param_count_out) {
    if (!sql) return NULL;

    size_t sql_len = strlen(sql);
    char *result = malloc(sql_len * 2 + 1);
    if (!result) return NULL;

    char **param_names = NULL;
    int param_count = 0;
    int param_capacity = 0;

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
            }
            *out++ = *p++;
            continue;
        }

        if (in_string) {
            *out++ = *p++;
            continue;
        }

        // Handle ? placeholder
        if (*p == '?') {
            param_count++;
            out += sprintf(out, "$%d", param_count);
            p++;
            // Add space if next char is a letter (to avoid $1left instead of $1 left)
            if (isalpha(*p)) {
                *out++ = ' ';
            }
            continue;
        }

        // Handle :name placeholder
        if (*p == ':' && (p == sql || !is_ident_char(*(p-1)))) {
            const char *name_start = p + 1;
            if (isalpha(*name_start) || *name_start == '_') {
                const char *name_end = name_start;
                while (is_ident_char(*name_end)) name_end++;

                size_t name_len = name_end - name_start;

                // Check if we've seen this name before
                int found_idx = -1;
                for (int i = 0; i < param_count; i++) {
                    if (param_names && param_names[i] &&
                        strncmp(param_names[i], name_start, name_len) == 0 &&
                        param_names[i][name_len] == '\0') {
                        found_idx = i;
                        break;
                    }
                }

                if (found_idx >= 0) {
                    // Reuse existing parameter index
                    out += sprintf(out, "$%d", found_idx + 1);
                } else {
                    // New parameter
                    if (param_count >= param_capacity) {
                        param_capacity = param_capacity ? param_capacity * 2 : 16;
                        param_names = realloc(param_names, param_capacity * sizeof(char*));
                    }

                    param_names[param_count] = malloc(name_len + 1);
                    memcpy(param_names[param_count], name_start, name_len);
                    param_names[param_count][name_len] = '\0';

                    param_count++;
                    out += sprintf(out, "$%d", param_count);
                }

                p = name_end;
                continue;
            }
        }

        *out++ = *p++;
    }

    *out = '\0';

    if (param_names_out) *param_names_out = param_names;
    else if (param_names) {
        for (int i = 0; i < param_count; i++) free(param_names[i]);
        free(param_names);
    }

    if (param_count_out) *param_count_out = param_count;

    return result;
}

// ============================================================================
// Function Translation
// ============================================================================

// Translate iif(cond, true_val, false_val) -> CASE WHEN cond THEN true_val ELSE false_val END
static char* translate_iif(const char *sql) {
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

// Translate typeof(x) -> pg_typeof(x)::text
static char* translate_typeof(const char *sql) {
    // Also need to translate the comparison values
    // SQLite typeof() returns: 'integer', 'real', 'text', 'blob', 'null'
    // PostgreSQL pg_typeof() returns: 'integer', 'bigint', 'double precision', etc.

    char *temp = str_replace_nocase(sql, "typeof(", "pg_typeof(");
    if (!temp) return NULL;

    // Add ::text cast after pg_typeof(...)
    // This is complex because we need to find the matching )
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

    // Now translate the type names in comparisons
    char *temp2 = str_replace(result, "'integer'", "'integer'");  // same
    free(result);
    temp2 = str_replace(temp2, "'real'", "'double precision'");
    // Note: SQLite 'integer' could be 'integer' or 'bigint' in PG

    return temp2;
}

// Translate strftime('%s', x, 'utc') -> EXTRACT(EPOCH FROM x)::bigint
static char* translate_strftime(const char *sql) {
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

// Translate unixepoch('now', '-7 day') -> EXTRACT(EPOCH FROM NOW() - INTERVAL '7 days')::bigint
static char* translate_unixepoch(const char *sql) {
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
                    // Remove quotes
                    char interval[256];
                    if (arg2[0] == '\'') {
                        strncpy(interval, arg2 + 1, sizeof(interval) - 1);
                        char *q = strrchr(interval, '\'');
                        if (q) *q = '\0';
                    } else {
                        strncpy(interval, arg2, sizeof(interval) - 1);
                    }

                    // Convert to PostgreSQL interval
                    // SQLite: '-7 day' -> PostgreSQL: INTERVAL '-7 days'
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

// Translate datetime('now') -> NOW()
static char* translate_datetime(const char *sql) {
    char *result = str_replace_nocase(sql, "datetime('now')", "NOW()");
    return result;
}

// Translate last_insert_rowid() -> lastval()
// SQLite: SELECT last_insert_rowid()
// PostgreSQL: SELECT lastval() (returns last value from any sequence in current session)
static char* translate_last_insert_rowid(const char *sql) {
    char *result = str_replace_nocase(sql, "last_insert_rowid()", "lastval()");
    return result;
}

// Translate json_each() -> json_array_elements()
// SQLite: SELECT value FROM json_each('[1,2,3]')
// PostgreSQL: SELECT value::text FROM json_array_elements('[1,2,3]'::json)
static char* translate_json_each(const char *sql) {
    if (!sql) return NULL;

    char *result = malloc(MAX_SQL_LEN);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;

    while (*p) {
        // Look for json_each( - case insensitive
        if (strncasecmp(p, "json_each(", 10) == 0) {
            const char *args_start = p + 10;

            // Extract the JSON argument manually to handle string literals properly
            // We need to find the closing ) taking into account quotes and nested parens
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

    // Now fix references to the value column - need to cast to appropriate type
    // Pattern: "value FROM json_array_elements" -> "(value::text)::integer FROM json_array_elements"
    // Cast to text first, then to integer to handle numeric comparisons
    char *temp = str_replace(result, " value FROM json_array_elements", " (value::text)::integer FROM json_array_elements");
    if (temp) {
        free(result);
        result = temp;
    }

    return result;
}

// Fix GROUP BY strict mode - add missing columns that appear in SELECT
// PostgreSQL requires all non-aggregate columns in SELECT to be in GROUP BY
static char* fix_group_by_strict(const char *sql) {
    if (!sql) return NULL;

    // Only process if it has GROUP BY
    if (!strcasestr(sql, "group by")) {
        return strdup(sql);
    }

    // Special case: metadata_item_clusterings query with missing column
    // SELECT metadata_item_id,metadata_item_cluster_id ... GROUP BY metadata_item_id
    // Need to add metadata_item_cluster_id to GROUP BY
    if (strcasestr(sql, "metadata_item_clusterings") &&
        strcasestr(sql, "metadata_item_cluster_id") &&
        strcasestr(sql, "group by") &&
        strcasestr(sql, "metadata_item_id")) {

        // Find the GROUP BY clause
        char *group_pos = strcasestr(sql, "group by");
        if (group_pos) {
            // Check if metadata_item_cluster_id is NOT already in GROUP BY
            char *having_pos = strcasestr(group_pos, "having");
            char *end_pos = having_pos ? having_pos : (group_pos + strlen(group_pos));

            // Create a substring of just the GROUP BY clause
            size_t group_clause_len = end_pos - group_pos;
            char *group_clause = malloc(group_clause_len + 1);
            if (group_clause) {
                memcpy(group_clause, group_pos, group_clause_len);
                group_clause[group_clause_len] = '\0';

                // Check if metadata_item_cluster_id is already in GROUP BY
                if (!strcasestr(group_clause, "metadata_item_cluster_id")) {
                    // Need to add it
                    // Find "GROUP BY metadata_item_clusterings.metadata_item_id"
                    // and add ",metadata_item_clusterings.metadata_item_cluster_id"
                    char *result = str_replace_nocase(sql,
                        "group by metadata_item_clusterings.metadata_item_id",
                        "group by metadata_item_clusterings.metadata_item_id,metadata_item_clusterings.metadata_item_cluster_id");
                    free(group_clause);
                    return result ? result : strdup(sql);
                }
                free(group_clause);
            }
        }
    }

    return strdup(sql);
}

// Add alias to subqueries in FROM clause
// PostgreSQL requires: FROM (SELECT ...) AS alias
// SQLite allows: FROM (SELECT ...)
static char* add_subquery_alias(const char *sql) {
    if (!sql) return NULL;

    // Look for pattern: FROM (SELECT ... ) followed by ) or ORDER or WHERE etc
    char *result = malloc(strlen(sql) * 2 + 100);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;
    int alias_counter = 0;

    while (*p) {
        // Look for "from (" or "FROM ("
        if ((strncasecmp(p, "from (", 6) == 0 || strncasecmp(p, "from  (", 7) == 0) &&
            (p == sql || !is_ident_char(*(p-1)))) {

            // Copy "from ("
            while (*p && *p != '(') *out++ = *p++;
            if (*p == '(') *out++ = *p++;

            // Check if this is a subquery (starts with SELECT)
            const char *after_paren = skip_ws(p);
            if (strncasecmp(after_paren, "select", 6) == 0) {
                // Find matching closing paren
                int depth = 1;
                while (*p && depth > 0) {
                    if (*p == '(') depth++;
                    else if (*p == ')') depth--;
                    if (depth > 0) *out++ = *p++;
                }

                if (*p == ')') {
                    // Check if already has alias (next non-space is AS or identifier)
                    const char *after_close = skip_ws(p + 1);
                    if (strncasecmp(after_close, "as ", 3) != 0 &&
                        strncasecmp(after_close, ")", 1) != 0 &&
                        strncasecmp(after_close, "order", 5) != 0 &&
                        strncasecmp(after_close, "where", 5) != 0 &&
                        strncasecmp(after_close, "group", 5) != 0 &&
                        strncasecmp(after_close, "having", 6) != 0 &&
                        strncasecmp(after_close, "limit", 5) != 0 &&
                        strncasecmp(after_close, "union", 5) != 0 &&
                        strncasecmp(after_close, ";", 1) != 0 &&
                        *after_close != '\0') {
                        // Has identifier after - probably already has alias
                        *out++ = *p++;
                    } else {
                        // No alias - add one
                        *out++ = *p++;  // copy )
                        out += sprintf(out, " AS subq%d", alias_counter++);
                    }
                }
            } else {
                // Not a subquery, just copy
            }
        } else {
            *out++ = *p++;
        }
    }

    *out = '\0';
    return result;
}

// Translate CASE with mixed boolean/integer types
// SQLite allows mixing 0/1 with booleans, PostgreSQL doesn't
static char* translate_case_booleans(const char *sql) {
    if (!sql) return NULL;

    char *current = strdup(sql);
    if (!current) return NULL;

    char *temp;

    // "else 1 end)" in boolean context -> "else true end)"
    // This fixes cases like: case when X then Y > 0 else 1 end
    temp = str_replace_nocase(current, " else 1 end)", " else true end)");
    free(current);
    if (!temp) return NULL;
    current = temp;

    // "else 0 end)" in boolean context -> "else false end)"
    temp = str_replace_nocase(current, " else 0 end)", " else false end)");
    free(current);
    if (!temp) return NULL;
    current = temp;

    // Handle integer CASE used as boolean: "and (case when ... then 0 else 1 end)"
    // Add <> 0 to make it a boolean expression
    temp = str_replace_nocase(current, "then 0 else true end)", "then false else true end)");
    free(current);
    if (!temp) return NULL;
    current = temp;

    temp = str_replace_nocase(current, "then 1 else false end)", "then true else false end)");
    free(current);
    if (!temp) return NULL;
    current = temp;

    return current;
}

// Translate max(a, b) -> GREATEST(a, b) when used with 2+ arguments
// SQLite's max() with multiple args returns the largest, PostgreSQL needs GREATEST()
static char* translate_max_to_greatest(const char *sql) {
    if (!sql) return NULL;

    char *result = malloc(strlen(sql) * 2 + 1);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;

    while (*p) {
        // Look for max( - case insensitive
        if ((p == sql || !is_ident_char(*(p-1))) &&
            strncasecmp(p, "max(", 4) == 0) {

            const char *start = p;
            p += 4;  // skip "max("

            // Extract the content inside parentheses
            int depth = 1;
            const char *content_start = p;
            while (*p && depth > 0) {
                if (*p == '(') depth++;
                else if (*p == ')') depth--;
                if (depth > 0) p++;
            }

            // Check if there's a comma (meaning 2+ args)
            int has_comma = 0;
            int inner_depth = 0;
            for (const char *c = content_start; c < p; c++) {
                if (*c == '(') inner_depth++;
                else if (*c == ')') inner_depth--;
                else if (*c == ',' && inner_depth == 0) {
                    has_comma = 1;
                    break;
                }
            }

            if (has_comma) {
                // Multiple args - use GREATEST
                memcpy(out, "GREATEST(", 9);
                out += 9;
                size_t content_len = p - content_start;
                memcpy(out, content_start, content_len);
                out += content_len;
                *out++ = ')';
                p++;  // skip closing )
            } else {
                // Single arg - keep as max() (aggregate)
                size_t len = (p + 1) - start;
                memcpy(out, start, len);
                out += len;
                p++;  // skip closing )
            }
        } else {
            *out++ = *p++;
        }
    }

    *out = '\0';
    return result;
}

// Translate min(a, b) -> LEAST(a, b) when used with 2+ arguments
static char* translate_min_to_least(const char *sql) {
    if (!sql) return NULL;

    char *result = malloc(strlen(sql) * 2 + 1);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;

    while (*p) {
        // Look for min( - case insensitive
        if ((p == sql || !is_ident_char(*(p-1))) &&
            strncasecmp(p, "min(", 4) == 0) {

            const char *start = p;
            p += 4;  // skip "min("

            // Extract the content inside parentheses
            int depth = 1;
            const char *content_start = p;
            while (*p && depth > 0) {
                if (*p == '(') depth++;
                else if (*p == ')') depth--;
                if (depth > 0) p++;
            }

            // Check if there's a comma (meaning 2+ args)
            int has_comma = 0;
            int inner_depth = 0;
            for (const char *c = content_start; c < p; c++) {
                if (*c == '(') inner_depth++;
                else if (*c == ')') inner_depth--;
                else if (*c == ',' && inner_depth == 0) {
                    has_comma = 1;
                    break;
                }
            }

            if (has_comma) {
                // Multiple args - use LEAST
                memcpy(out, "LEAST(", 6);
                out += 6;
                size_t content_len = p - content_start;
                memcpy(out, content_start, content_len);
                out += content_len;
                *out++ = ')';
                p++;  // skip closing )
            } else {
                // Single arg - keep as min() (aggregate)
                size_t len = (p + 1) - start;
                memcpy(out, start, len);
                out += len;
                p++;  // skip closing )
            }
        } else {
            *out++ = *p++;
        }
    }

    *out = '\0';
    return result;
}

// Translate SQLite FTS4 queries to PostgreSQL ILIKE queries
// Example: join fts4_metadata_titles_icu on ... where fts4_metadata_titles_icu.title match 'Gooi*'
// Becomes: where metadata_items.title ILIKE '%Gooi%'
static char* translate_fts(const char *sql) {
    if (!sql) return NULL;

    // Only process if it contains FTS4 tables
    if (!strcasestr(sql, "fts4_")) {
        return strdup(sql);
    }

    char *result = malloc(strlen(sql) * 2 + 100);
    if (!result) return NULL;

    // Copy the SQL and process it
    strcpy(result, sql);

    // Remove JOIN with fts4_metadata_titles_icu
    char *join_start = strcasestr(result, "join fts4_metadata_titles_icu");
    if (join_start) {
        // Find the end of the JOIN clause (next WHERE, JOIN, GROUP, ORDER, etc.)
        char *join_end = join_start;
        while (*join_end) {
            if (strncasecmp(join_end, " where ", 7) == 0 ||
                strncasecmp(join_end, " join ", 6) == 0 ||
                strncasecmp(join_end, " left ", 6) == 0 ||
                strncasecmp(join_end, " group ", 7) == 0 ||
                strncasecmp(join_end, " order ", 7) == 0) {
                break;
            }
            join_end++;
        }
        // Remove the JOIN clause by shifting the rest
        memmove(join_start, join_end, strlen(join_end) + 1);
    }

    // Remove JOIN with fts4_tag_titles_icu
    join_start = strcasestr(result, "join fts4_tag_titles_icu");
    if (join_start) {
        char *join_end = join_start;
        while (*join_end) {
            if (strncasecmp(join_end, " where ", 7) == 0 ||
                strncasecmp(join_end, " join ", 6) == 0 ||
                strncasecmp(join_end, " left ", 6) == 0 ||
                strncasecmp(join_end, " group ", 7) == 0 ||
                strncasecmp(join_end, " order ", 7) == 0) {
                break;
            }
            join_end++;
        }
        memmove(join_start, join_end, strlen(join_end) + 1);
    }

    // Find and translate MATCH clauses
    // fts4_metadata_titles_icu.title match 'Gooi*' -> metadata_items.title ILIKE '%Gooi%'
    char *match_pos;
    while ((match_pos = strcasestr(result, "fts4_metadata_titles_icu.title match ")) != NULL) {
        // Find the search term (between quotes)
        char *quote_start = strchr(match_pos, '\'');
        if (!quote_start) break;
        quote_start++;
        char *quote_end = strchr(quote_start, '\'');
        if (!quote_end) break;

        // Extract search term (remove * at end if present)
        char search_term[256] = {0};
        size_t term_len = quote_end - quote_start;
        if (term_len > 254) term_len = 254;
        strncpy(search_term, quote_start, term_len);
        search_term[term_len] = '\0';

        // Remove trailing * if present
        if (term_len > 0 && search_term[term_len-1] == '*') {
            search_term[term_len-1] = '\0';
        }

        // Create replacement: metadata_items.title ILIKE '%term%'
        char replacement[512];
        snprintf(replacement, sizeof(replacement), "metadata_items.title ILIKE '%%%s%%'", search_term);

        // Calculate positions and lengths
        size_t old_len = (quote_end + 1) - match_pos;
        size_t new_len = strlen(replacement);

        // Make room if needed and replace
        if (new_len > old_len) {
            size_t rest_len = strlen(quote_end + 1);
            memmove(match_pos + new_len, quote_end + 1, rest_len + 1);
        } else {
            memmove(match_pos + new_len, quote_end + 1, strlen(quote_end + 1) + 1);
        }
        memcpy(match_pos, replacement, new_len);
    }

    // Also handle title_sort match
    while ((match_pos = strcasestr(result, "fts4_metadata_titles_icu.title_sort match ")) != NULL) {
        char *quote_start = strchr(match_pos, '\'');
        if (!quote_start) break;
        quote_start++;
        char *quote_end = strchr(quote_start, '\'');
        if (!quote_end) break;

        char search_term[256] = {0};
        size_t term_len = quote_end - quote_start;
        if (term_len > 254) term_len = 254;
        strncpy(search_term, quote_start, term_len);
        if (term_len > 0 && search_term[term_len-1] == '*') {
            search_term[term_len-1] = '\0';
        }

        char replacement[512];
        snprintf(replacement, sizeof(replacement), "metadata_items.title_sort ILIKE '%%%s%%'", search_term);

        size_t old_len = (quote_end + 1) - match_pos;
        size_t new_len = strlen(replacement);

        if (new_len > old_len) {
            memmove(match_pos + new_len, quote_end + 1, strlen(quote_end + 1) + 1);
        } else {
            memmove(match_pos + new_len, quote_end + 1, strlen(quote_end + 1) + 1);
        }
        memcpy(match_pos, replacement, new_len);
    }

    // Handle tag titles: fts4_tag_titles_icu.title or .tag match -> tags.tag ILIKE
    // First try .title match, then .tag match
    while (1) {
        match_pos = strcasestr(result, "fts4_tag_titles_icu.title match ");
        if (!match_pos) {
            match_pos = strcasestr(result, "fts4_tag_titles_icu.tag match ");
        }
        if (!match_pos) break;
        char *quote_start = strchr(match_pos, '\'');
        if (!quote_start) break;
        quote_start++;
        char *quote_end = strchr(quote_start, '\'');
        if (!quote_end) break;

        char search_term[256] = {0};
        size_t term_len = quote_end - quote_start;
        if (term_len > 254) term_len = 254;
        strncpy(search_term, quote_start, term_len);
        if (term_len > 0 && search_term[term_len-1] == '*') {
            search_term[term_len-1] = '\0';
        }

        char replacement[512];
        snprintf(replacement, sizeof(replacement), "tags.tag ILIKE '%%%s%%'", search_term);

        size_t old_len = (quote_end + 1) - match_pos;
        size_t new_len = strlen(replacement);

        if (new_len > old_len) {
            memmove(match_pos + new_len, quote_end + 1, strlen(quote_end + 1) + 1);
        } else {
            memmove(match_pos + new_len, quote_end + 1, strlen(quote_end + 1) + 1);
        }
        memcpy(match_pos, replacement, new_len);
    }

    return result;
}

// Fix forward references in self-joins
// SQLite allows: FROM a JOIN b AS x ON x.id=a.col JOIN a ON a.guid=...
// PostgreSQL requires the unaliased table to be defined first
// Pattern: join metadata_items as X on X.id=metadata_items.Y ... join metadata_items on
static char* fix_forward_reference_joins(const char *sql) {
    if (!sql) return NULL;

    // Check if this query has the problematic pattern
    // Look for: join TABLE as ALIAS on ... TABLE. ... join TABLE on
    const char *first_alias_join = strcasestr(sql, "join metadata_items as ");
    if (!first_alias_join) return strdup(sql);

    // Check if there's a forward reference to metadata_items before the unaliased join
    const char *unaliased_join = strcasestr(sql, " join metadata_items on ");
    if (!unaliased_join) return strdup(sql);

    // Check if the unaliased join comes AFTER the aliased joins
    if (unaliased_join < first_alias_join) return strdup(sql);

    // Check if there's a forward reference (metadata_items.something) before the unaliased join
    const char *check = first_alias_join;
    int has_forward_ref = 0;
    while (check < unaliased_join) {
        if (strncasecmp(check, "metadata_items.", 15) == 0) {
            // Make sure this isn't part of "as metadata_items"
            if (check > sql && *(check-1) != ' ' && *(check-1) != '=') {
                // This is a reference to metadata_items, not a definition
            }
            has_forward_ref = 1;
            break;
        }
        check++;
    }

    if (!has_forward_ref) return strdup(sql);

    // We need to move the unaliased join before the aliased joins
    // Find the start of "join metadata_items on" and its end
    const char *move_start = unaliased_join + 1; // skip leading space

    // Find end of this JOIN clause (next JOIN, WHERE, GROUP, ORDER, or end)
    const char *move_end = move_start;
    int paren_depth = 0;
    while (*move_end) {
        if (*move_end == '(') paren_depth++;
        else if (*move_end == ')') paren_depth--;
        else if (paren_depth == 0) {
            if (strncasecmp(move_end, " join ", 6) == 0 ||
                strncasecmp(move_end, " left ", 6) == 0 ||
                strncasecmp(move_end, " where ", 7) == 0 ||
                strncasecmp(move_end, " group ", 7) == 0 ||
                strncasecmp(move_end, " order ", 7) == 0 ||
                strncasecmp(move_end, " limit ", 7) == 0) {
                break;
            }
        }
        move_end++;
    }

    // Calculate sizes
    size_t prefix_len = first_alias_join - sql;  // Everything before first aliased join
    size_t move_len = move_end - move_start;     // The unaliased join clause to move
    size_t middle_len = move_start - first_alias_join - 1; // Between first alias and unaliased (minus space)
    size_t suffix_len = strlen(move_end);        // Everything after unaliased join

    // Build result: prefix + moved_join + " " + middle + suffix
    size_t result_len = prefix_len + move_len + 1 + middle_len + suffix_len + 1;
    char *result = malloc(result_len);
    if (!result) return strdup(sql);

    char *out = result;

    // Copy prefix (up to first aliased join)
    memcpy(out, sql, prefix_len);
    out += prefix_len;

    // Copy the unaliased join (moved here)
    memcpy(out, move_start, move_len);
    out += move_len;

    // Add space
    *out++ = ' ';

    // Copy middle (the aliased joins, without the trailing space before unaliased)
    memcpy(out, first_alias_join, middle_len);
    out += middle_len;

    // Copy suffix (everything after the original unaliased join position)
    memcpy(out, move_end, suffix_len);
    out += suffix_len;

    *out = '\0';

    return result;
}

// Remove DISTINCT when it would conflict with PostgreSQL ORDER BY requirements
// PostgreSQL requires ORDER BY columns to be in SELECT list when using DISTINCT
static char* translate_distinct_orderby(const char *sql) {
    if (!sql) return NULL;

    // Only process if it has DISTINCT
    if (!strcasestr(sql, "distinct")) {
        return strdup(sql);
    }

    // Remove DISTINCT when:
    // 1. GROUP BY is present (DISTINCT is redundant)
    // 2. ORDER BY is present (might conflict with PostgreSQL)
    if (strcasestr(sql, "group by") || strcasestr(sql, "order by")) {
        char *result = str_replace_nocase(sql, "select distinct", "select");
        return result ? result : strdup(sql);
    }

    return strdup(sql);
}

// Main function translator
char* sql_translate_functions(const char *sql) {
    if (!sql) return NULL;

    char *current = strdup(sql);
    if (!current) return NULL;

    // Apply translations in order
    char *temp;

    // 0. FTS4 queries -> ILIKE queries
    temp = translate_fts(current);
    free(current);
    if (!temp) return NULL;
    current = temp;

    // 0b. Remove DISTINCT when ORDER BY is present (PostgreSQL compatibility)
    temp = translate_distinct_orderby(current);
    free(current);
    if (!temp) return NULL;
    current = temp;

    // 1. iif() -> CASE WHEN
    temp = translate_iif(current);
    free(current);
    if (!temp) return NULL;
    current = temp;

    // 2. typeof() -> pg_typeof()::text
    temp = translate_typeof(current);
    free(current);
    if (!temp) return NULL;
    current = temp;

    // 3. strftime() -> EXTRACT/TO_CHAR
    temp = translate_strftime(current);
    free(current);
    if (!temp) return NULL;
    current = temp;

    // 4. unixepoch() -> EXTRACT(EPOCH FROM ...)
    temp = translate_unixepoch(current);
    free(current);
    if (!temp) return NULL;
    current = temp;

    // 5. datetime('now') -> NOW()
    temp = translate_datetime(current);
    free(current);
    if (!temp) return NULL;
    current = temp;

    // 5a. last_insert_rowid() -> lastval()
    temp = translate_last_insert_rowid(current);
    free(current);
    if (!temp) return NULL;
    current = temp;

    // 5b. json_each() -> json_array_elements()
    temp = translate_json_each(current);
    free(current);
    if (!temp) return NULL;
    current = temp;

    // 6. IFNULL -> COALESCE
    temp = str_replace_nocase(current, "IFNULL(", "COALESCE(");
    free(current);
    if (!temp) return NULL;
    current = temp;

    // 7. SUBSTR -> SUBSTRING
    temp = str_replace_nocase(current, "SUBSTR(", "SUBSTRING(");
    free(current);
    if (!temp) return NULL;
    current = temp;

    // 8. RANDOM() is the same in both

    // 9. LENGTH() is the same in both

    // 10. INSTR(str, substr) -> POSITION(substr IN str)
    // This is complex, skip for now

    // 11. max(a, b) with 2+ args -> GREATEST(a, b)
    // SQLite's max() can be used as scalar function with multiple args
    // PostgreSQL's max() is only aggregate, use GREATEST() for scalar
    temp = translate_max_to_greatest(current);
    free(current);
    if (!temp) return NULL;
    current = temp;

    // 12. min(a, b) with 2+ args -> LEAST(a, b)
    temp = translate_min_to_least(current);
    free(current);
    if (!temp) return NULL;
    current = temp;

    // 13. CASE THEN 0/1 -> THEN FALSE/TRUE (for boolean context)
    temp = translate_case_booleans(current);
    free(current);
    if (!temp) return NULL;
    current = temp;

    // 14. Add alias to subqueries in FROM clause
    temp = add_subquery_alias(current);
    free(current);
    if (!temp) return NULL;
    current = temp;

    // 15. Fix forward reference in self-joins (SQLite allows, PostgreSQL doesn't)
    temp = fix_forward_reference_joins(current);
    free(current);
    if (!temp) return NULL;
    current = temp;

    // 15b. Fix GROUP BY strict mode - add missing columns
    temp = fix_group_by_strict(current);
    free(current);
    if (!temp) return NULL;
    current = temp;

    // 16. Fix incomplete GROUP BY clauses for specific problematic queries
    // metadata_item_views query: add missing columns to GROUP BY
    if (strcasestr(current, "metadata_item_views.originally_available_at") &&
        strcasestr(current, "group by grandparents.id order by")) {
        temp = str_replace(current,
            "group by grandparents.id order by",
            "group by grandparents.id,metadata_item_views.originally_available_at,metadata_item_views.parent_index,metadata_item_views.\"index\",grandparents.library_section_id,grandparentsSettings.extra_data,metadata_item_views.viewed_at order by");
        free(current);
        if (!temp) return NULL;
        current = temp;
    }

    // external_metadata_items query: add missing columns to GROUP BY
    if (strcasestr(current, "external_metadata_items.id,uri,user_title") &&
        strcasestr(current, "group by title order by")) {
        temp = str_replace(current,
            "group by title order by",
            "group by title,external_metadata_items.id,uri,user_title,library_section_id,metadata_type,year,added_at,updated_at,extra_data order by");
        free(current);
        if (!temp) return NULL;
        current = temp;
    }

    // metadata_item_clusterings query: remove GROUP BY entirely (causes issues)
    if (strcasestr(current, "metadata_item_clusterings") &&
        strcasestr(current, "group by")) {
        // For this query, using DISTINCT instead of GROUP BY works better
        if (strcasestr(current, "select DISTINCT")) {
            // Already has DISTINCT, just remove GROUP BY clause
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
// Type Translation
// ============================================================================

char* sql_translate_types(const char *sql) {
    if (!sql) return NULL;

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

    // varchar(255) -> VARCHAR(255) (same, but normalize case)
    // TEXT is the same
    // BLOB -> BYTEA
    temp = str_replace_nocase(current, " BLOB", " BYTEA");
    free(current);
    current = temp;

    // boolean DEFAULT 't' -> boolean DEFAULT TRUE
    temp = str_replace(current, "DEFAULT 't'", "DEFAULT TRUE");
    free(current);
    current = temp;

    temp = str_replace(current, "DEFAULT 'f'", "DEFAULT FALSE");
    free(current);
    current = temp;

    // datetime -> TIMESTAMP (SQLite datetime type)
    temp = str_replace_nocase(current, " datetime)", " TIMESTAMP)");
    free(current);
    current = temp;

    temp = str_replace_nocase(current, " datetime,", " TIMESTAMP,");
    free(current);
    current = temp;

    temp = str_replace_nocase(current, " datetime ", " TIMESTAMP ");
    free(current);
    current = temp;

    // float -> FLOAT (should be REAL or DOUBLE PRECISION, but FLOAT works)
    // This is already compatible

    return current;
}

// ============================================================================
// Alias Quote Translation
// ============================================================================

// Translate backticks `column` -> "column"
// SQLite/MySQL use backticks for identifiers, PostgreSQL uses double quotes
static char* translate_backticks(const char *sql) {
    if (!sql) return NULL;

    char *result = malloc(strlen(sql) + 1);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;

    while (*p) {
        if (*p == '`') {
            *out++ = '"';  // Replace backtick with double quote
        } else {
            *out++ = *p;
        }
        p++;
    }

    *out = '\0';
    return result;
}

// Translate table.'column' -> table."column"
// SQLite allows single quotes for column names, PostgreSQL requires double quotes
static char* translate_column_quotes(const char *sql) {
    if (!sql) return NULL;

    char *result = malloc(strlen(sql) * 2 + 1);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;
    int in_string = 0;

    while (*p) {
        // Check for table.'column' pattern: preceded by dot
        if (*p == '\'' && p > sql && *(p-1) == '.') {
            *out++ = '"';  // Replace opening single with double
            p++;

            // Copy content until closing single quote
            while (*p && *p != '\'') {
                *out++ = *p++;
            }

            if (*p == '\'') {
                *out++ = '"';  // Replace closing single with double
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
            // Check for escaped single quote ('')
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

// Translate AS 'alias' -> AS "alias"
// SQLite allows single quotes for aliases, PostgreSQL requires double quotes
static char* translate_alias_quotes(const char *sql) {
    if (!sql) return NULL;

    char *result = malloc(strlen(sql) * 2 + 1);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;
    int in_string = 0;
    char string_char = 0;

    while (*p) {
        // Track string literals (not after AS)
        if ((*p == '\'' || *p == '"') && !in_string) {
            // Check if this is after AS (alias context)
            const char *back = p - 1;
            while (back > sql && isspace(*back)) back--;

            // Check if preceded by AS
            if (back >= sql + 1 &&
                (back[-1] == 'a' || back[-1] == 'A') &&
                (back[0] == 's' || back[0] == 'S') &&
                (back == sql + 1 || !is_ident_char(back[-2]))) {

                // This is an alias - if single quote, convert to double
                if (*p == '\'') {
                    *out++ = '"';  // Replace opening single with double
                    p++;

                    // Copy content until closing single quote
                    while (*p && *p != '\'') {
                        *out++ = *p++;
                    }

                    if (*p == '\'') {
                        *out++ = '"';  // Replace closing single with double
                        p++;
                    }
                    continue;
                }
            }

            // Not an alias context - track string
            in_string = 1;
            string_char = *p;
            *out++ = *p++;
            continue;
        }

        if (in_string && *p == string_char) {
            // Check for escaped quotes (doubled)
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
// DDL IF NOT EXISTS Translation
// ============================================================================

// Add IF NOT EXISTS to CREATE TABLE/INDEX to make them idempotent
static char* add_if_not_exists(const char *sql) {
    if (!sql) return NULL;

    const char *s = sql;
    while (*s && isspace(*s)) s++;

    // Check for CREATE TABLE (without IF NOT EXISTS already)
    if (strncasecmp(s, "CREATE TABLE ", 13) == 0 &&
        strncasecmp(s + 13, "IF NOT EXISTS ", 14) != 0) {
        // Insert IF NOT EXISTS after CREATE TABLE
        size_t prefix_len = (s - sql) + 12; // "CREATE TABLE" without space
        size_t rest_len = strlen(s + 12);
        char *result = malloc(prefix_len + 15 + rest_len + 1); // +15 for " IF NOT EXISTS"
        if (!result) return NULL;

        memcpy(result, sql, prefix_len);
        memcpy(result + prefix_len, " IF NOT EXISTS", 14);
        strcpy(result + prefix_len + 14, s + 12);
        return result;
    }

    // Check for CREATE INDEX (without IF NOT EXISTS already)
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

    // Check for CREATE UNIQUE INDEX
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
// DDL Identifier Quote Translation
// ============================================================================

// Translate 'identifier' -> "identifier" for DDL statements
// In SQLite, single quotes can be used for identifiers in DDL
// In PostgreSQL, single quotes are only for string literals
static char* translate_ddl_quotes(const char *sql) {
    if (!sql) return NULL;

    // Only process DDL statements
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
        // Track parentheses depth
        if (*p == '(') in_parens++;
        if (*p == ')') in_parens--;

        // Check for single-quoted identifier (not a string literal)
        if (*p == '\'') {
            // Check if this looks like an identifier context
            const char *back = p - 1;
            while (back > sql && isspace(*back)) back--;

            int is_identifier = 0;

            // After CREATE TABLE, CREATE INDEX, ON, ADD, etc.
            if (back >= sql) {
                // After keywords like TABLE, INDEX, ON, UNIQUE, ADD, etc.
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

            // If at start after CREATE, also an identifier
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

            // Inside column definitions after ( or ,
            if (in_parens > 0 && back >= sql && (*back == '(' || *back == ',')) {
                is_identifier = 1;
            }

            if (is_identifier) {
                // Convert to double-quoted identifier
                *out++ = '"';
                p++;

                // Copy content until closing single quote
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
// Keyword Translation
// ============================================================================

char* sql_translate_keywords(const char *sql) {
    if (!sql) return NULL;

    char *current = strdup(sql);
    if (!current) return NULL;

    char *temp;

    // SQLite transaction modes -> PostgreSQL BEGIN
    temp = str_replace_nocase(current, "BEGIN IMMEDIATE", "BEGIN");
    free(current);
    current = temp;

    temp = str_replace_nocase(current, "BEGIN DEFERRED", "BEGIN");
    free(current);
    current = temp;

    temp = str_replace_nocase(current, "BEGIN EXCLUSIVE", "BEGIN");
    free(current);
    current = temp;

    // REPLACE INTO -> INSERT INTO ... ON CONFLICT DO UPDATE
    // This is complex - for now just convert to INSERT
    temp = str_replace_nocase(current, "REPLACE INTO", "INSERT INTO");
    free(current);
    current = temp;

    // INSERT OR IGNORE -> INSERT ... ON CONFLICT DO NOTHING
    temp = str_replace_nocase(current, "INSERT OR IGNORE INTO", "INSERT INTO");
    free(current);
    current = temp;

    // INSERT OR REPLACE -> INSERT ... ON CONFLICT DO UPDATE
    temp = str_replace_nocase(current, "INSERT OR REPLACE INTO", "INSERT INTO");
    free(current);
    current = temp;

    // GLOB -> LIKE (patterns need conversion too, but basic replacement)
    temp = str_replace_nocase(current, " GLOB ", " LIKE ");
    free(current);
    current = temp;

    // "index" is a reserved word in PostgreSQL - but "index" (quoted) is valid
    // Only need to quote unquoted uses, but our backtick translation already
    // handles this. Remove the incorrect index_field translation.

    // AS 'alias' -> AS "alias" (SQLite allows single quotes, PostgreSQL requires double)
    temp = translate_alias_quotes(current);
    free(current);
    current = temp;

    // table.'column' -> table."column" (SQLite allows single quotes for column names)
    temp = translate_column_quotes(current);
    free(current);
    current = temp;

    // `column` -> "column" (backticks to double quotes)
    temp = translate_backticks(current);
    free(current);
    current = temp;

    // Remove COLLATE icu_root (SQLite ICU collation not available in PostgreSQL)
    temp = str_replace_nocase(current, " collate icu_root", "");
    free(current);
    current = temp;

    // Fix empty IN clause: IN () -> IN (NULL) (SQLite accepts empty, PostgreSQL doesn't)
    temp = str_replace(current, " in ()", " IN (NULL)");
    free(current);
    current = temp;
    temp = str_replace(current, " IN ()", " IN (NULL)");
    free(current);
    current = temp;

    // Translate sqlite_master to PostgreSQL equivalent
    // SQLite's sqlite_master contains: type, name, tbl_name, rootpage, sql
    // We emulate this with a UNION of tables, views, and indexes from information_schema
    if (strcasestr(current, "sqlite_master") || strcasestr(current, "sqlite_schema")) {
        // Replace simple SELECT * FROM sqlite_master queries
        // with a PostgreSQL equivalent
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

        // Replace various forms - use placeholder first then do single replacement
        // Replace "main".sqlite_master first (most specific)
        temp = str_replace_nocase(current, "\"main\".sqlite_master", sqlite_master_pg);
        if (strcmp(temp, current) != 0) {
            free(current);
            current = temp;
        } else {
            free(temp);
            // Try main.sqlite_master
            temp = str_replace_nocase(current, "main.sqlite_master", sqlite_master_pg);
            if (strcmp(temp, current) != 0) {
                free(current);
                current = temp;
            } else {
                free(temp);
                // Try bare sqlite_master
                temp = str_replace_nocase(current, "sqlite_master", sqlite_master_pg);
                if (strcmp(temp, current) != 0) {
                    free(current);
                    current = temp;
                } else {
                    free(temp);
                    // Try sqlite_schema
                    temp = str_replace_nocase(current, "sqlite_schema", sqlite_master_pg);
                    free(current);
                    current = temp;
                }
            }
        }

        // Remove ORDER BY rowid (not applicable for our emulated table)
        temp = str_replace_nocase(current, " ORDER BY rowid", "");
        free(current);
        current = temp;
    }

    // Remove INDEXED BY hints (SQLite-specific)
    // Pattern: "indexed by index_name" - need regex-like removal
    // For now, handle common patterns
    temp = current;
    char *indexed_pos;
    while ((indexed_pos = strcasestr(temp, " indexed by ")) != NULL) {
        // Find end of index name (next space or end of clause)
        char *end = indexed_pos + 12;  // skip " indexed by "
        while (*end && !isspace(*end) && *end != ')' && *end != ',') end++;

        // Create new string without this part
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

// ============================================================================
// Operator Spacing Fix
// ============================================================================

// Fix operator spacing: !=-1 -> != -1, >=-1 -> >= -1, etc.
// PostgreSQL doesn't recognize !=-  or >=- as operators
static char* fix_operator_spacing(const char *sql) {
    if (!sql) return NULL;

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
            }
            *out++ = *p++;
            continue;
        }

        if (in_string) {
            *out++ = *p++;
            continue;
        }

        // Check for operators followed immediately by - and digit
        // Patterns: !=- , <>- , >=- , <=- , =- (but not ==-)
        if ((p[0] == '!' && p[1] == '=' && p[2] == '-' && isdigit(p[3])) ||
            (p[0] == '<' && p[1] == '>' && p[2] == '-' && isdigit(p[3])) ||
            (p[0] == '>' && p[1] == '=' && p[2] == '-' && isdigit(p[3])) ||
            (p[0] == '<' && p[1] == '=' && p[2] == '-' && isdigit(p[3]))) {
            // Copy the two-char operator
            *out++ = *p++;
            *out++ = *p++;
            // Add space before the negative number
            *out++ = ' ';
            continue;
        }

        // Single char operators followed by -digit: =-, >-, <-
        // But be careful: <- could be valid, so only fix when followed by digit
        if ((p[0] == '=' && p[1] == '-' && isdigit(p[2]) && (p == sql || (p[-1] != '!' && p[-1] != '>' && p[-1] != '<'))) ||
            (p[0] == '>' && p[1] == '-' && isdigit(p[2]) && (p == sql || p[-1] != '<')) ||
            (p[0] == '<' && p[1] == '-' && isdigit(p[2]) && (p == sql || p[-1] != '>'))) {
            // Copy the operator
            *out++ = *p++;
            // Add space before the negative number
            *out++ = ' ';
            continue;
        }

        *out++ = *p++;
    }

    *out = '\0';
    return result;
}

// ============================================================================
// Main Translation Function
// ============================================================================

void sql_translator_init(void) {
    // Nothing to initialize for now
}

void sql_translator_cleanup(void) {
    // Nothing to cleanup for now
}

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

    // Step 3: Translate types (mainly for DDL)
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

    // Step 5: Translate DDL single-quoted identifiers
    char *step5 = translate_ddl_quotes(step4);
    free(step4);
    if (!step5) {
        strcpy(result.error, "DDL quote translation failed");
        return result;
    }

    // Step 6: Add IF NOT EXISTS to CREATE TABLE/INDEX
    char *step6 = add_if_not_exists(step5);
    free(step5);
    if (!step6) {
        strcpy(result.error, "IF NOT EXISTS translation failed");
        return result;
    }

    // Step 7: Fix operator spacing (!=-1 -> != -1)
    char *step7 = fix_operator_spacing(step6);
    free(step6);
    if (!step7) {
        strcpy(result.error, "Operator spacing fix failed");
        return result;
    }

    result.sql = step7;
    result.success = 1;

    return result;
}

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
