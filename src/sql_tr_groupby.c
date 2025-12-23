/*
 * SQL Translator - GROUP BY Query Rewriter
 * Complete solution for PostgreSQL GROUP BY strict mode
 *
 * PostgreSQL requires all non-aggregate columns in SELECT to appear in GROUP BY
 * SQLite is permissive and allows selecting ungrouped columns
 * This module automatically adds missing columns to GROUP BY clause
 */

#include "sql_translator_internal.h"
#include "pg_logging.h"
#include <ctype.h>
#include <string.h>
#include <stdlib.h>

#define MAX_COLUMNS 512
#define MAX_COLUMN_LEN 256
#define MAX_QUERY_LEN 262144

// Column reference tracking
typedef struct {
    char name[MAX_COLUMN_LEN];  // Full column reference (e.g., "table.column")
    int is_aggregate;            // 1 if inside aggregate function
    int is_alias;                // 1 if this is an alias name
} column_ref_t;

// Aggregate function names
static const char *AGGREGATE_FUNCS[] = {
    "count", "sum", "avg", "max", "min", "group_concat",
    "string_agg", "array_agg", "bool_and", "bool_or",
    "every", "json_agg", "jsonb_agg", "xmlagg",
    NULL
};

// ============================================================================
// Helper: Check if token is an aggregate function
// ============================================================================

static int is_aggregate_func(const char *str, size_t len) {
    for (int i = 0; AGGREGATE_FUNCS[i]; i++) {
        size_t func_len = strlen(AGGREGATE_FUNCS[i]);
        if (len == func_len && strncasecmp(str, AGGREGATE_FUNCS[i], len) == 0) {
            return 1;
        }
    }
    return 0;
}

// ============================================================================
// Helper: Skip to matching closing parenthesis
// ============================================================================

static const char* skip_to_matching_paren(const char *p) {
    int depth = 1;
    p++; // Skip opening '('

    while (*p && depth > 0) {
        if (*p == '\'') {
            // Skip string literal
            p++;
            while (*p && *p != '\'') {
                if (*p == '\\') p++; // Skip escaped char
                if (*p) p++;
            }
            if (*p) p++;
        } else if (*p == '"') {
            // Skip quoted identifier
            p++;
            while (*p && *p != '"') {
                if (*p == '\\') p++; // Skip escaped char
                if (*p) p++;
            }
            if (*p) p++;
        } else if (*p == '(') {
            depth++;
            p++;
        } else if (*p == ')') {
            depth--;
            p++;
        } else {
            p++;
        }
    }

    return p - 1; // Return pointer to closing ')'
}

// ============================================================================
// Helper: Extract column name (handles table.column, "quoted", `backtick`)
// ============================================================================

static const char* extract_column_name(const char *p, char *buf, size_t bufsize) {
    const char *start = p;
    size_t idx = 0;

    p = skip_ws(p);

    while (*p && idx < bufsize - 1) {
        if (is_ident_char(*p) || *p == '.') {
            buf[idx++] = *p++;
        } else if (*p == '"') {
            // Quoted identifier
            buf[idx++] = *p++;
            while (*p && *p != '"' && idx < bufsize - 1) {
                buf[idx++] = *p++;
            }
            if (*p == '"') buf[idx++] = *p++;
        } else if (*p == '`') {
            // Backtick identifier (SQLite style)
            buf[idx++] = '"'; // Convert to PostgreSQL style
            p++;
            while (*p && *p != '`' && idx < bufsize - 1) {
                buf[idx++] = *p++;
            }
            if (*p == '`') {
                buf[idx++] = '"';
                p++;
            }
        } else {
            break;
        }
    }

    buf[idx] = '\0';

    // Trim trailing whitespace
    while (idx > 0 && isspace(buf[idx-1])) {
        buf[--idx] = '\0';
    }

    return p;
}

// ============================================================================
// Helper: Check if column already exists in list
// ============================================================================

static int column_exists(column_ref_t *cols, int count, const char *name) {
    for (int i = 0; i < count; i++) {
        if (strcasecmp(cols[i].name, name) == 0) {
            return 1;
        }
    }
    return 0;
}

// ============================================================================
// Helper: Normalize column name (strip quotes, lowercase for comparison)
// ============================================================================

static void normalize_column_name(const char *input, char *output, size_t outsize) {
    size_t i = 0, o = 0;

    while (input[i] && o < outsize - 1) {
        if (input[i] == '"' || input[i] == '`') {
            i++; // Skip quotes
        } else {
            output[o++] = tolower(input[i++]);
        }
    }
    output[o] = '\0';
}

// ============================================================================
// Parse SELECT clause and extract column references
// ============================================================================

static int parse_select_columns(const char *select_start, const char *from_pos,
                                  column_ref_t *cols, int max_cols) {
    int col_count = 0;
    const char *p = select_start;

    // Skip "SELECT" keyword
    while (*p && !isspace(*p)) p++;
    p = skip_ws(p);

    // Skip DISTINCT if present
    if (strncasecmp(p, "distinct", 8) == 0) {
        p += 8;
        p = skip_ws(p);
        // Handle DISTINCT(column) syntax
        if (*p == '(') {
            p++; // Skip '('
        }
    }

    while (p < from_pos && col_count < max_cols) {
        p = skip_ws(p);
        if (p >= from_pos) break;

        // Check for aggregate function
        const char *func_start = p;
        while (*p && is_ident_char(*p)) p++;
        size_t func_len = p - func_start;

        p = skip_ws(p);

        if (*p == '(' && is_aggregate_func(func_start, func_len)) {
            // This is an aggregate function - skip its content
            p = skip_to_matching_paren(p);
            if (*p == ')') p++;

            // Skip to next column or end
            p = skip_ws(p);
            while (*p && *p != ',' && p < from_pos) {
                if (*p == '(') {
                    p = skip_to_matching_paren(p);
                    if (*p == ')') p++;
                } else {
                    p++;
                }
            }
            if (*p == ',') p++;
            continue;
        }

        // Not an aggregate - extract column reference
        p = func_start; // Reset

        char col_name[MAX_COLUMN_LEN];
        const char *col_start = p;

        // Handle CASE expressions
        if (strncasecmp(p, "case", 4) == 0 && !is_ident_char(p[4])) {
            // Skip CASE...END expression
            int case_depth = 1;
            p += 4;
            while (*p && case_depth > 0 && p < from_pos) {
                if (strncasecmp(p, "case", 4) == 0 && !is_ident_char(p[4])) {
                    case_depth++;
                    p += 4;
                } else if (strncasecmp(p, "end", 3) == 0 && !is_ident_char(p[3])) {
                    case_depth--;
                    p += 3;
                } else {
                    p++;
                }
            }
            // CASE expressions don't need to be in GROUP BY
            p = skip_ws(p);
            if (strncasecmp(p, "as", 2) == 0) {
                p += 2;
                p = skip_ws(p);
                while (*p && is_ident_char(*p)) p++;
            }
            if (*p == ',') p++;
            continue;
        }

        // Handle subqueries
        if (*p == '(') {
            p = skip_to_matching_paren(p);
            if (*p == ')') p++;
            p = skip_ws(p);
            if (strncasecmp(p, "as", 2) == 0) {
                p += 2;
                p = skip_ws(p);
                while (*p && is_ident_char(*p)) p++;
            }
            if (*p == ',') p++;
            continue;
        }

        // Extract column name
        p = extract_column_name(p, col_name, sizeof(col_name));

        if (col_name[0]) {
            // Skip if this looks like a constant or expression result
            if (isdigit(col_name[0]) ||
                strcasecmp(col_name, "null") == 0 ||
                strcasecmp(col_name, "true") == 0 ||
                strcasecmp(col_name, "false") == 0) {
                // Skip constant
            } else if (column_exists(cols, col_count, col_name)) {
                // Already have this column
            } else {
                // Add column
                strncpy(cols[col_count].name, col_name, MAX_COLUMN_LEN - 1);
                cols[col_count].name[MAX_COLUMN_LEN - 1] = '\0';
                cols[col_count].is_aggregate = 0;
                cols[col_count].is_alias = 0;
                col_count++;
            }
        }

        // Skip to next column
        p = skip_ws(p);

        // Check for AS alias
        if (strncasecmp(p, "as", 2) == 0 && !is_ident_char(p[2])) {
            p += 2;
            p = skip_ws(p);
            // Skip alias name
            if (*p == '"') {
                p++;
                while (*p && *p != '"') p++;
                if (*p == '"') p++;
            } else {
                while (*p && is_ident_char(*p)) p++;
            }
        }

        p = skip_ws(p);
        if (*p == ',') p++;
    }

    return col_count;
}

// ============================================================================
// Parse existing GROUP BY clause
// ============================================================================

static int parse_group_by_columns(const char *group_by_start, const char *group_by_end,
                                    column_ref_t *cols, int max_cols) {
    int col_count = 0;
    const char *p = group_by_start;

    // Skip "GROUP BY" keywords
    while (*p && !isspace(*p)) p++;
    p = skip_ws(p);
    while (*p && !isspace(*p)) p++;
    p = skip_ws(p);

    while (p < group_by_end && col_count < max_cols) {
        p = skip_ws(p);
        if (p >= group_by_end) break;

        char col_name[MAX_COLUMN_LEN];
        p = extract_column_name(p, col_name, sizeof(col_name));

        if (col_name[0] && !isdigit(col_name[0])) {
            // Add to list if not a number (PostgreSQL allows GROUP BY 1,2,3)
            if (!column_exists(cols, col_count, col_name)) {
                strncpy(cols[col_count].name, col_name, MAX_COLUMN_LEN - 1);
                cols[col_count].name[MAX_COLUMN_LEN - 1] = '\0';
                cols[col_count].is_aggregate = 0;
                cols[col_count].is_alias = 0;
                col_count++;
            }
        }

        p = skip_ws(p);
        if (*p == ',') p++;
    }

    return col_count;
}

// ============================================================================
// Main GROUP BY rewriter function
// ============================================================================

char* fix_group_by_strict_complete(const char *sql) {
    if (!sql) return NULL;

    // Quick check: does query have GROUP BY?
    const char *group_by_pos = strcasestr(sql, "group by");
    if (!group_by_pos) {
        return strdup(sql);
    }

    // Find SELECT
    const char *select_pos = strcasestr(sql, "select");
    if (!select_pos) {
        return strdup(sql);
    }

    // Find FROM
    const char *from_pos = strcasestr(select_pos + 6, " from ");
    if (!from_pos) {
        return strdup(sql);
    }

    // Find end of GROUP BY clause
    const char *group_by_end = group_by_pos + strlen(group_by_pos);
    const char *having_pos = strcasestr(group_by_pos, " having ");
    const char *order_pos = strcasestr(group_by_pos, " order by ");
    const char *limit_pos = strcasestr(group_by_pos, " limit ");
    const char *offset_pos = strcasestr(group_by_pos, " offset ");

    if (having_pos && having_pos < group_by_end) group_by_end = having_pos;
    if (order_pos && order_pos < group_by_end) group_by_end = order_pos;
    if (limit_pos && limit_pos < group_by_end) group_by_end = limit_pos;
    if (offset_pos && offset_pos < group_by_end) group_by_end = offset_pos;

    // Allocate column tracking arrays
    column_ref_t *select_cols = calloc(MAX_COLUMNS, sizeof(column_ref_t));
    column_ref_t *groupby_cols = calloc(MAX_COLUMNS, sizeof(column_ref_t));

    if (!select_cols || !groupby_cols) {
        free(select_cols);
        free(groupby_cols);
        return strdup(sql);
    }

    // Parse SELECT columns
    int select_count = parse_select_columns(select_pos, from_pos, select_cols, MAX_COLUMNS);

    // Parse existing GROUP BY columns
    int groupby_count = parse_group_by_columns(group_by_pos, group_by_end, groupby_cols, MAX_COLUMNS);

    // Find columns missing from GROUP BY
    column_ref_t *missing_cols = calloc(MAX_COLUMNS, sizeof(column_ref_t));
    int missing_count = 0;

    if (!missing_cols) {
        free(select_cols);
        free(groupby_cols);
        return strdup(sql);
    }

    for (int i = 0; i < select_count && missing_count < MAX_COLUMNS; i++) {
        if (select_cols[i].is_aggregate) continue;

        // Check if this column is in GROUP BY
        int found = 0;
        for (int j = 0; j < groupby_count; j++) {
            // Normalize and compare
            char norm_select[MAX_COLUMN_LEN];
            char norm_groupby[MAX_COLUMN_LEN];
            normalize_column_name(select_cols[i].name, norm_select, sizeof(norm_select));
            normalize_column_name(groupby_cols[j].name, norm_groupby, sizeof(norm_groupby));

            if (strcmp(norm_select, norm_groupby) == 0) {
                found = 1;
                break;
            }
        }

        if (!found) {
            // Add to missing list
            memcpy(&missing_cols[missing_count], &select_cols[i], sizeof(column_ref_t));
            missing_count++;
        }
    }

    // If no missing columns, return original
    if (missing_count == 0) {
        free(select_cols);
        free(groupby_cols);
        free(missing_cols);
        return strdup(sql);
    }

    // Reconstruct query with updated GROUP BY
    size_t prefix_len = group_by_end - sql;
    size_t suffix_len = strlen(group_by_end);
    size_t additional_len = missing_count * MAX_COLUMN_LEN; // Generous estimate

    char *result = malloc(prefix_len + suffix_len + additional_len + 1024);
    if (!result) {
        free(select_cols);
        free(groupby_cols);
        free(missing_cols);
        return strdup(sql);
    }

    // Copy prefix (everything up to end of current GROUP BY)
    memcpy(result, sql, prefix_len);
    result[prefix_len] = '\0';

    // Trim any trailing whitespace from GROUP BY clause
    while (prefix_len > 0 && isspace(result[prefix_len - 1])) {
        result[--prefix_len] = '\0';
    }

    // Append missing columns
    for (int i = 0; i < missing_count; i++) {
        strcat(result, ",");
        strcat(result, missing_cols[i].name);
    }

    // Append suffix (rest of query)
    strcat(result, group_by_end);

    LOG_INFO("GROUP_BY_REWRITER: Added %d missing columns to GROUP BY", missing_count);

    free(select_cols);
    free(groupby_cols);
    free(missing_cols);

    return result;
}
