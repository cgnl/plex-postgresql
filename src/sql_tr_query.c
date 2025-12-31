/*
 * SQL Translator - Query Structure Translations
 * Fixes for PostgreSQL query structure requirements
 */

#include "sql_translator_internal.h"
#include "pg_logging.h"

// ============================================================================
// Fix GROUP BY strict mode - add missing columns
// ============================================================================

char* fix_group_by_strict(const char *sql) {
    if (!sql) return NULL;

    // Only process if it has GROUP BY
    if (!strcasestr(sql, "group by")) {
        return strdup(sql);
    }

    // Special case: metadata_item_clusterings query with invalid outer table reference
    // Bug: GROUP BY references clusters.library_section_id which is in outer query, not subquery
    // Fix: Remove the outer table reference from GROUP BY
    if (strcasestr(sql, "metadata_item_clusterings") &&
        strcasestr(sql, "clusters.library_section_id") &&
        strcasestr(sql, "group by")) {

        // Remove ",clusters.library_section_id" from GROUP BY in subquery
        char *result = str_replace_nocase(sql,
            ",clusters.library_section_id HAVING",
            " HAVING");
        if (result && strcmp(result, sql) != 0) {
            LOG_INFO("Fixed clusters subquery: removed outer table reference from GROUP BY");
            return result;
        }
        if (result) free(result);
    }

    // Special case: metadata_item_clusterings query with missing column
    if (strcasestr(sql, "metadata_item_clusterings") &&
        strcasestr(sql, "metadata_item_cluster_id") &&
        strcasestr(sql, "group by") &&
        strcasestr(sql, "metadata_item_id")) {

        char *group_pos = strcasestr(sql, "group by");
        if (group_pos) {
            char *having_pos = strcasestr(group_pos, "having");
            char *end_pos = having_pos ? having_pos : (group_pos + strlen(group_pos));

            size_t group_clause_len = end_pos - group_pos;
            char *group_clause = malloc(group_clause_len + 1);
            if (group_clause) {
                memcpy(group_clause, group_pos, group_clause_len);
                group_clause[group_clause_len] = '\0';

                if (!strcasestr(group_clause, "metadata_item_cluster_id")) {
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

// ============================================================================
// Add alias to subqueries in FROM clause
// PostgreSQL requires: FROM (SELECT ...) AS alias
// ============================================================================

char* add_subquery_alias(const char *sql) {
    if (!sql) return NULL;

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
                    // Check if already has alias
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
            }
        } else {
            *out++ = *p++;
        }
    }

    *out = '\0';
    return result;
}

// ============================================================================
// Translate CASE with mixed boolean/integer types
// ============================================================================

char* translate_case_booleans(const char *sql) {
    if (!sql) return NULL;

    char *current = strdup(sql);
    if (!current) return NULL;

    char *temp;

    temp = str_replace_nocase(current, " else 1 end)", " else true end)");
    free(current);
    if (!temp) return NULL;
    current = temp;

    temp = str_replace_nocase(current, " else 0 end)", " else false end)");
    free(current);
    if (!temp) return NULL;
    current = temp;

    temp = str_replace_nocase(current, "then 0 else true end)", "then false else true end)");
    free(current);
    if (!temp) return NULL;
    current = temp;

    temp = str_replace_nocase(current, "then 1 else false end)", "then true else false end)");
    free(current);
    if (!temp) return NULL;
    current = temp;

    // Fix integer literals in boolean context (SQLite allows 0/1, PostgreSQL requires boolean)
    // (0 or ...) -> (FALSE or ...)
    temp = str_replace_nocase(current, "(0 or ", "(FALSE or ");
    free(current);
    if (!temp) return NULL;
    current = temp;

    temp = str_replace_nocase(current, "(1 or ", "(TRUE or ");
    free(current);
    if (!temp) return NULL;
    current = temp;

    // ... and 0) -> ... and FALSE)
    temp = str_replace_nocase(current, " and 0)", " and FALSE)");
    free(current);
    if (!temp) return NULL;
    current = temp;

    temp = str_replace_nocase(current, " and 1)", " and TRUE)");
    free(current);
    if (!temp) return NULL;
    current = temp;

    // ... or 0) -> ... or FALSE)
    temp = str_replace_nocase(current, " or 0)", " or FALSE)");
    free(current);
    if (!temp) return NULL;
    current = temp;

    temp = str_replace_nocase(current, " or 1)", " or TRUE)");
    free(current);
    if (!temp) return NULL;
    current = temp;

    return current;
}

// ============================================================================
// max(a, b) -> GREATEST(a, b) when used with 2+ arguments
// ============================================================================

char* translate_max_to_greatest(const char *sql) {
    if (!sql) return NULL;

    char *result = malloc(strlen(sql) * 2 + 1);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;

    while (*p) {
        if ((p == sql || !is_ident_char(*(p-1))) &&
            strncasecmp(p, "max(", 4) == 0) {

            const char *start = p;
            p += 4;

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
                memcpy(out, "GREATEST(", 9);
                out += 9;
                size_t content_len = p - content_start;
                memcpy(out, content_start, content_len);
                out += content_len;
                *out++ = ')';
                p++;
            } else {
                size_t len = (p + 1) - start;
                memcpy(out, start, len);
                out += len;
                p++;
            }
        } else {
            *out++ = *p++;
        }
    }

    *out = '\0';
    return result;
}

// ============================================================================
// min(a, b) -> LEAST(a, b) when used with 2+ arguments
// ============================================================================

char* translate_min_to_least(const char *sql) {
    if (!sql) return NULL;

    char *result = malloc(strlen(sql) * 2 + 1);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;

    while (*p) {
        if ((p == sql || !is_ident_char(*(p-1))) &&
            strncasecmp(p, "min(", 4) == 0) {

            const char *start = p;
            p += 4;

            int depth = 1;
            const char *content_start = p;
            while (*p && depth > 0) {
                if (*p == '(') depth++;
                else if (*p == ')') depth--;
                if (depth > 0) p++;
            }

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
                memcpy(out, "LEAST(", 6);
                out += 6;
                size_t content_len = p - content_start;
                memcpy(out, content_start, content_len);
                out += content_len;
                *out++ = ')';
                p++;
            } else {
                size_t len = (p + 1) - start;
                memcpy(out, start, len);
                out += len;
                p++;
            }
        } else {
            *out++ = *p++;
        }
    }

    *out = '\0';
    return result;
}

// ============================================================================
// Translate SQLite FTS4 queries to PostgreSQL ILIKE queries
// ============================================================================

// Helper to convert SQLite FTS MATCH term to PostgreSQL tsquery
// 'term*' -> 'term:*'
// 'term1 term2' -> 'term1 & term2' (AND logic)
static void convert_fts_term(const char *sqlite_term, char *pg_term, size_t pg_term_size) {
    if (!sqlite_term || !pg_term || pg_term_size == 0) return;

    size_t len = strlen(sqlite_term);
    size_t out_idx = 0;

    for (size_t i = 0; i < len && out_idx < pg_term_size - 2; i++) {
        char c = sqlite_term[i];
        if (c == '*') {
            // Wildcard: term* -> term:*
            pg_term[out_idx++] = ':';
            pg_term[out_idx++] = '*';
        } else if (c == ' ') {
            // Space -> AND operator
            pg_term[out_idx++] = ' ';
            pg_term[out_idx++] = '&';
            pg_term[out_idx++] = ' ';
        } else if (c == '\'' || c == '\\') {
            // Escape special characters
            pg_term[out_idx++] = '\\';
            pg_term[out_idx++] = c;
        } else {
            pg_term[out_idx++] = c;
        }
    }
    pg_term[out_idx] = '\0';
}

char* translate_fts(const char *sql) {
    if (!sql) return NULL;

    // Allocate result buffer (generous size)
    char *result = malloc(strlen(sql) * 3 + 1024);
    if (!result) return NULL;
    strcpy(result, sql);

    // List of table.column combinations to handle
    struct fts_map {
        const char *search;      // e.g. "fts4_metadata_titles_icu.title"
        const char *replacement; // e.g. "fts4_metadata_titles_icu.title_fts"
    } maps[] = {
        { "fts4_metadata_titles_icu.title_sort", "fts4_metadata_titles_icu.title_fts" },
        { "fts4_metadata_titles.title_sort", "fts4_metadata_titles.title_fts" },
        { "fts4_metadata_titles_icu.title", "fts4_metadata_titles_icu.title_fts" },
        { "fts4_metadata_titles.title", "fts4_metadata_titles.title_fts" },
        { "fts4_tag_titles_icu.title", "fts4_tag_titles_icu.title_fts" },
        { "fts4_tag_titles.title", "fts4_tag_titles.title_fts" },
        { "fts4_tag_titles_icu.tag", "fts4_tag_titles_icu.title_fts" },
        { "fts4_tag_titles.tag", "fts4_tag_titles.title_fts" },
        // Fallback for table match (implicit column)
        { "fts4_metadata_titles_icu", "fts4_metadata_titles_icu.title_fts" },
        { "fts4_metadata_titles", "fts4_metadata_titles.title_fts" },
        { "fts4_tag_titles_icu", "fts4_tag_titles_icu.title_fts" },
        { "fts4_tag_titles", "fts4_tag_titles.title_fts" },
        { NULL, NULL }
    };

    int changed = 0;

    for (int i = 0; maps[i].search; i++) {
        char *pos = result;
        while ((pos = strcasestr(pos, maps[i].search)) != NULL) {
            // Check what follows: must be whitespace then "match"
            char *scan = pos + strlen(maps[i].search);
            while (*scan && isspace(*scan)) scan++;
            
            LOG_INFO("FTS Scan for %s found: '%.10s'", maps[i].search, scan);

            if (strncasecmp(scan, "match", 5) == 0) {
                LOG_INFO("Found FTS match: %s match...", maps[i].search);
                // Found "...column match"
                char *match_op_pos = scan;
                scan += 5; // skip "match"
                while (*scan && isspace(*scan)) scan++;
                
                if (*scan == '\'') {
                    // Start of term string
                    char *quote_start = scan + 1;
                    char *quote_end = strchr(quote_start, '\'');
                    if (quote_end) {
                        // We have the full term
                        char search_term[256] = {0};
                        size_t term_len = quote_end - quote_start;
                        if (term_len > 254) term_len = 254;
                        strncpy(search_term, quote_start, term_len);

                        char pg_term[512] = {0};
                        convert_fts_term(search_term, pg_term, sizeof(pg_term));

                        // Construct replacement: col_fts @@ to_tsquery(...)
                        char replacement[1024];
                        snprintf(replacement, sizeof(replacement), 
                            "%s @@ to_tsquery('simple', '%s')", 
                            maps[i].replacement, pg_term);

                        // Replacment logic
                        // Original range to remove: from 'pos' to 'quote_end' (inclusive of quote)
                        size_t old_len = (quote_end + 1) - pos;
                        size_t new_len = strlen(replacement);
                        size_t tail_len = strlen(quote_end + 1);

                        // Move tail
                        memmove(pos + new_len, quote_end + 1, tail_len + 1);
                        // Insert replacement
                        memcpy(pos, replacement, new_len);

                        pos += new_len; // advance
                        changed = 1;
                        continue; // look for next occurrence
                    }
                }
            }
            pos++; // advance if no match found
        }
    }

    if (!changed) {
        free(result);
        return strdup(sql);
    }
    return result;
}

// ============================================================================
// Fix forward references in self-joins
// ============================================================================

char* fix_forward_reference_joins(const char *sql) {
    if (!sql) return NULL;

    const char *first_alias_join = strcasestr(sql, "join metadata_items as ");
    if (!first_alias_join) return strdup(sql);

    const char *unaliased_join = strcasestr(sql, " join metadata_items on ");
    if (!unaliased_join) return strdup(sql);

    if (unaliased_join < first_alias_join) return strdup(sql);

    // Check for forward reference
    const char *check = first_alias_join;
    int has_forward_ref = 0;
    while (check < unaliased_join) {
        if (strncasecmp(check, "metadata_items.", 15) == 0) {
            has_forward_ref = 1;
            break;
        }
        check++;
    }

    if (!has_forward_ref) return strdup(sql);

    // Move the unaliased join before the aliased joins
    const char *move_start = unaliased_join + 1;

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

    size_t prefix_len = first_alias_join - sql;
    size_t move_len = move_end - move_start;
    size_t middle_len = move_start - first_alias_join - 1;
    size_t suffix_len = strlen(move_end);

    size_t result_len = prefix_len + move_len + 1 + middle_len + suffix_len + 1;
    char *result = malloc(result_len);
    if (!result) return strdup(sql);

    char *out = result;

    memcpy(out, sql, prefix_len);
    out += prefix_len;

    memcpy(out, move_start, move_len);
    out += move_len;

    *out++ = ' ';

    memcpy(out, first_alias_join, middle_len);
    out += middle_len;

    memcpy(out, move_end, suffix_len);
    out += suffix_len;

    *out = '\0';

    return result;
}

// ============================================================================
// Remove DISTINCT when ORDER BY uses aggregate functions
// PostgreSQL requires: for SELECT DISTINCT, ORDER BY expressions must appear in select list
// This is commonly an issue with queries like: SELECT DISTINCT(id) ... GROUP BY id ORDER BY count(*)
// ============================================================================

// Helper to check if a column appears in the SELECT clause
static int column_in_select(const char *sql, const char *column) {
    // Find FROM to delimit SELECT clause
    const char *from_pos = strcasestr(sql, " from ");
    if (!from_pos) return 0;

    // Search for column in the SELECT clause (before FROM)
    size_t select_len = from_pos - sql;
    char *select_clause = malloc(select_len + 1);
    if (!select_clause) return 0;

    memcpy(select_clause, sql, select_len);
    select_clause[select_len] = '\0';

    int found = (strcasestr(select_clause, column) != NULL);
    free(select_clause);
    return found;
}

// ============================================================================
// Convert SQLite NULL sorting to PostgreSQL NULLS LAST
// SQLite pattern: ORDER BY col IS NULL, col ASC  -> puts NULLs last
// PostgreSQL:     ORDER BY col ASC NULLS LAST
// This is required because with DISTINCT, "col IS NULL" is not in SELECT list
// ============================================================================

char* translate_null_sorting(const char *sql) {
    if (!sql) return NULL;

    // Quick check if there's "IS NULL" in ORDER BY
    const char *order_by = strcasestr(sql, "order by");
    if (!order_by) return strdup(sql);

    if (!strcasestr(order_by, " is null") && !strcasestr(order_by, "IS NULL")) {
        return strdup(sql);
    }

    char *current = strdup(sql);
    if (!current) return NULL;

    // Use simple string replacements for common Plex patterns
    // Pattern: "col IS NULL,col asc" -> "col ASC NULLS LAST"
    // Pattern: "col IS NULL, col asc" -> "col ASC NULLS LAST"

    // List of known column patterns used by Plex
    const char *columns[] = {
        "parents.`index`",
        "parents.\"index\"",
        "metadata_items.`index`",
        "metadata_items.\"index\"",
        "metadata_items.originally_available_at",
        "grandparents.title_sort",
        NULL
    };

    for (int i = 0; columns[i]; i++) {
        char pattern1[256], pattern2[256], pattern3[256], pattern4[256];
        char replacement[256];

        // Pattern with no space after comma, lowercase asc
        snprintf(pattern1, sizeof(pattern1), "%s IS NULL,%s asc", columns[i], columns[i]);
        // Pattern with space after comma, lowercase asc
        snprintf(pattern2, sizeof(pattern2), "%s IS NULL, %s asc", columns[i], columns[i]);
        // Pattern with no space after comma, uppercase ASC
        snprintf(pattern3, sizeof(pattern3), "%s IS NULL,%s ASC", columns[i], columns[i]);
        // Pattern with space after comma, uppercase ASC
        snprintf(pattern4, sizeof(pattern4), "%s IS NULL, %s ASC", columns[i], columns[i]);

        snprintf(replacement, sizeof(replacement), "%s ASC NULLS LAST", columns[i]);

        char *temp;

        // Try all pattern variations (case insensitive)
        temp = str_replace_nocase(current, pattern1, replacement);
        if (temp && strcmp(temp, current) != 0) {
            free(current);
            current = temp;
            continue;
        }
        free(temp);

        temp = str_replace_nocase(current, pattern2, replacement);
        if (temp && strcmp(temp, current) != 0) {
            free(current);
            current = temp;
            continue;
        }
        free(temp);

        temp = str_replace_nocase(current, pattern3, replacement);
        if (temp && strcmp(temp, current) != 0) {
            free(current);
            current = temp;
            continue;
        }
        free(temp);

        temp = str_replace_nocase(current, pattern4, replacement);
        if (temp && strcmp(temp, current) != 0) {
            free(current);
            current = temp;
            continue;
        }
        free(temp);
    }

    return current;
}

char* translate_distinct_orderby(const char *sql) {
    if (!sql) return NULL;

    if (!strcasestr(sql, "distinct")) {
        return strdup(sql);
    }

    // Check if ORDER BY uses aggregate functions (count, sum, avg, max, min)
    const char *order_by_pos = strcasestr(sql, "order by");
    if (order_by_pos) {
        // Look for aggregate functions in ORDER BY clause
        const char *agg_funcs[] = {"count(", "sum(", "avg(", "max(", "min(", NULL};
        for (int i = 0; agg_funcs[i]; i++) {
            if (strcasestr(order_by_pos, agg_funcs[i])) {
                // Found aggregate in ORDER BY - remove DISTINCT
                char *result = str_replace_nocase(sql, "select distinct", "select");
                return result ? result : strdup(sql);
            }
        }

        // Check for common Plex ORDER BY patterns that use table aliases not in SELECT
        // These patterns cause "ORDER BY expressions must appear in select list" errors
        const char *problem_patterns[] = {
            "grandparents.",   // Aliased parent's parent
            "parents.",        // Aliased parent
            "metadata_items.", // When DISTINCT on media_items but ORDER BY metadata_items
            NULL
        };

        for (int i = 0; problem_patterns[i]; i++) {
            const char *pattern_pos = strcasestr(order_by_pos, problem_patterns[i]);
            if (pattern_pos) {
                // Extract the full column reference (e.g., "grandparents.title_sort")
                char col_ref[256];
                const char *start = pattern_pos;
                const char *end = start;

                // Find end of column reference
                while (*end && (is_ident_char(*end) || *end == '.' || *end == '"')) {
                    end++;
                }

                size_t col_len = end - start;
                if (col_len < sizeof(col_ref)) {
                    memcpy(col_ref, start, col_len);
                    col_ref[col_len] = '\0';

                    // Check if this column is in the SELECT clause
                    if (!column_in_select(sql, col_ref)) {
                        LOG_INFO("Removing DISTINCT due to ORDER BY column not in SELECT: %s", col_ref);
                        char *result = str_replace_nocase(sql, "select distinct", "select");
                        return result ? result : strdup(sql);
                    }
                }
            }
        }
    }

    // Also remove DISTINCT when GROUP BY is present (GROUP BY already ensures uniqueness)
    if (strcasestr(sql, "group by")) {
        char *result = str_replace_nocase(sql, "select distinct", "select");
        return result ? result : strdup(sql);
    }

    return strdup(sql);
}

// ============================================================================
// Fix integer vs text type mismatch
// Issue: metadata_items.id IN (SELECT taggings.metadata_item_id ...) throws integer = text
// Fix: Cast the subquery column to integer explicitly.
// ============================================================================

// Strip "collate icu_root" from SQL (PG doesn't support it)
char* strip_icu_collation(const char *sql) {
    if (!sql) return NULL;
    if (strcasestr(sql, "collate icu_root")) {
        // Use a simple loop to remove all occurrences
        char *result = strdup(sql);
        char *pos;
        while ((pos = strcasestr(result, " collate icu_root"))) {
            // Remove " collate icu_root" (17 chars)
            memmove(pos, pos + 17, strlen(pos + 17) + 1);
        }
        // Also check without leading space just in case
        while ((pos = strcasestr(result, "collate icu_root"))) {
            memmove(pos, pos + 16, strlen(pos + 16) + 1);
        }
        return result;
    }
    return strdup(sql); // Return copy if no change
}

char* fix_integer_text_mismatch(const char *sql) {
    if (!sql) return NULL;

    // Pattern: metadata_items.id IN (SELECT taggings.metadata_item_id
    // Fix: Cast BOTH sides to text to ensure type equality regardless of underlying types.
    // metadata_items.id::text IN (SELECT taggings.metadata_item_id::text

    // We match the specific join pattern to be safe, then replace the IN clause structure.
    if (strcasestr(sql, "metadata_items.id in (select taggings.metadata_item_id")) {
        LOG_INFO("Fixing integer/text mismatch in SQL: %.100s...", sql);
        char *temp = str_replace_nocase(sql,
            "metadata_items.id in (select taggings.metadata_item_id",
            "metadata_items.id::text in (select taggings.metadata_item_id::text");
        return temp ? temp : strdup(sql);
    }

    return strdup(sql);
}

// ============================================================================
// Fix JSON operator (->>) on TEXT columns
// SQLite: column ->> '$.path' works on TEXT with JSON
// PostgreSQL: Convert to LIKE pattern since data may be malformed JSON
// Example: extra_data ->> '$.pv:version' < '1'
//       -> (extra_data LIKE '%"pv:version":"0"%' OR extra_data NOT LIKE '%"pv:version"%')
// ============================================================================

char* fix_json_operator_on_text(const char *sql) {
    if (!sql) return NULL;

    // Check if the query contains ->> operator with $.
    if (!strstr(sql, "->>") || !strstr(sql, "'$.")) {
        return strdup(sql);
    }

    LOG_INFO("Fixing JSON ->> operator on TEXT columns");

    char *result = malloc(strlen(sql) * 4 + 2048);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;

    while (*p) {
        // Look for pattern: column ->> '$.key' IS NULL
        // or: column ->> '$.key' < 'value'
        const char *scan = p;

        // Try to match: column_name ->> '$.key'
        if (is_ident_char(*scan) || *scan == '.') {
            const char *col_start = scan;

            // Find end of column name
            while (*scan && (is_ident_char(*scan) || *scan == '.')) {
                scan++;
            }
            const char *col_end = scan;

            // Skip whitespace
            while (*scan && isspace(*scan)) scan++;

            // Check for ->>
            if (strncmp(scan, "->>", 3) == 0) {
                scan += 3;
                while (*scan && isspace(*scan)) scan++;

                // Check for '$.
                if (*scan == '\'' && scan[1] == '$' && scan[2] == '.') {
                    // Extract the JSON key
                    const char *key_start = scan + 3; // skip '$.
                    const char *key_end = strchr(key_start, '\'');

                    if (key_end) {
                        char json_key[256];
                        size_t key_len = key_end - key_start;
                        if (key_len < sizeof(json_key)) {
                            memcpy(json_key, key_start, key_len);
                            json_key[key_len] = '\0';

                            // Copy column name
                            size_t col_len = col_end - col_start;
                            memcpy(out, col_start, col_len);
                            out += col_len;

                            // Check what comes after the JSON operator
                            const char *after = key_end + 1;
                            while (*after && isspace(*after)) after++;

                            // Convert to LIKE pattern based on the comparison
                            if (strncasecmp(after, "is null", 7) == 0) {
                                // column ->> '$.key' IS NULL
                                // -> (column IS NULL OR column NOT LIKE '%"key"%')
                                out += sprintf(out, " NOT LIKE '%%\"%s\"%%'", json_key);
                                p = after + 7;
                                continue;
                            } else if (strncmp(after, "<", 1) == 0) {
                                // column ->> '$.key' < 'value'
                                // -> column LIKE '%"key":"0"%' (simplified for version checking)
                                out += sprintf(out, " LIKE '%%\"%s\":\"0\"%%'", json_key);
                                // Skip the < and the value
                                p = strchr(after, '\'');
                                if (p) {
                                    p = strchr(p + 1, '\'');
                                    if (p) p++;
                                }
                                continue;
                            }
                        }
                    }
                }
            }
        }

        // No match - copy character and continue
        *out++ = *p++;
    }

    *out = '\0';
    return result;
}

// ============================================================================
// Fix collections query: Filter out metadata_type=18 from queries
// Plex can't serialize collections properly - skip them to avoid 500 errors
// ============================================================================

char* fix_collections_query(const char *sql) {
    if (!sql) return NULL;

    char *result = strdup(sql);
    if (!result) return NULL;

    // Check if query has type=1 (movies) specifically, not just as part of type=18
    // Need to check for "metadata_type=1 " or "metadata_type=1)" pattern
    int has_type1 = (strcasestr(result, "metadata_type=1 ") != NULL ||
                     strcasestr(result, "metadata_type=1)") != NULL ||
                     strcasestr(result, "metadata_type=1\n") != NULL ||
                     strcasestr(result, "metadata_type=1\t") != NULL);
    int has_type18 = (strcasestr(result, "metadata_type=18") != NULL);

    // Filter out collections (metadata_type=18) from queries that include both movies and collections
    if (has_type1 && has_type18) {
        LOG_INFO("COLLECTIONS_FIX: Found query with both type=1 and type=18");
        char *temp = str_replace_nocase(result,
            "(metadata_items.metadata_type=1 or metadata_items.metadata_type=18)",
            "metadata_items.metadata_type=1");
        if (temp) {
            LOG_INFO("COLLECTIONS_FIX: Replaced combined pattern");
            free(result);
            result = temp;
        }

        // Also try alternative pattern
        temp = str_replace_nocase(result,
            "((metadata_items.metadata_type=1 or metadata_items.metadata_type=18)",
            "(metadata_items.metadata_type=1");
        if (temp) {
            free(result);
            result = temp;
        }
    }

    // For the pure collections query (only type=18, no type=1), return empty result
    // NOTE: Disabled - sqlite3_value fix should handle collections now
    // if (has_type18 && !has_type1) {
    //     LOG_INFO("COLLECTIONS_FIX: Found pure collections query, adding FALSE");
    //     // Add 1=0 condition to make it return 0 rows
    //     char *temp = str_replace_nocase(result,
    //         "metadata_type=18",
    //         "metadata_type=18 AND 1=0");
    //     if (temp) {
    //         LOG_INFO("COLLECTIONS_FIX: Result: %.100s", temp);
    //         free(result);
    //         result = temp;
    //     } else {
    //         LOG_ERROR("COLLECTIONS_FIX: str_replace_nocase failed!");
    //     }
    // }

    return result;
}
