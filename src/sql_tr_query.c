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

    // Fast path: if no patterns that need boolean translation
    if (!strcasestr(sql, "end)") && !strcasestr(sql, "(0 ") && !strcasestr(sql, "(1 ") &&
        !strcasestr(sql, "where 0") && !strcasestr(sql, "where 1")) {
        return strdup(sql);
    }

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

    // WHERE 0 -> WHERE FALSE, WHERE 1 -> WHERE TRUE
    // SQLite treats integers as booleans, PostgreSQL doesn't
    temp = str_replace_nocase(current, " WHERE 0", " WHERE FALSE");
    free(current);
    if (!temp) return NULL;
    current = temp;

    temp = str_replace_nocase(current, " WHERE 1", " WHERE TRUE");
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
// Supports:
//   'term*'        -> 'term:*'           (prefix wildcard)
//   'term1 term2'  -> 'term1 & term2'    (implicit AND)
//   '-term'        -> '!term'            (negation)
//   'term1 AND term2' -> 'term1 & term2' (explicit AND)
//   'term1 OR term2'  -> 'term1 | term2' (OR)
//   '"exact phrase"'  -> 'exact <-> phrase' (phrase search)
static void convert_fts_term(const char *sqlite_term, char *pg_term, size_t pg_term_size) {
    if (!sqlite_term || !pg_term || pg_term_size == 0) return;

    size_t len = strlen(sqlite_term);
    size_t out_idx = 0;
    int in_phrase = 0;  // Track if we're inside a quoted phrase

    for (size_t i = 0; i < len && out_idx < pg_term_size - 4; i++) {
        char c = sqlite_term[i];

        // Handle phrase quotes
        if (c == '"') {
            in_phrase = !in_phrase;
            // Skip the quote character itself
            continue;
        }

        // Handle negation: -term -> !term
        if (c == '-' && (i == 0 || sqlite_term[i-1] == ' ')) {
            pg_term[out_idx++] = '!';
            continue;
        }

        // Handle wildcard: term* -> term:*
        if (c == '*') {
            pg_term[out_idx++] = ':';
            pg_term[out_idx++] = '*';
            continue;
        }

        // Handle explicit AND/OR keywords (case insensitive)
        if (!in_phrase && i + 3 < len) {
            // Check for " AND " pattern
            if ((c == 'A' || c == 'a') &&
                (sqlite_term[i+1] == 'N' || sqlite_term[i+1] == 'n') &&
                (sqlite_term[i+2] == 'D' || sqlite_term[i+2] == 'd') &&
                (i == 0 || sqlite_term[i-1] == ' ') &&
                sqlite_term[i+3] == ' ') {
                // Already have space before, add & and skip "AND "
                pg_term[out_idx++] = '&';
                pg_term[out_idx++] = ' ';
                i += 3;  // Skip "AND" (loop will skip the space)
                continue;
            }
            // Check for " OR " pattern
            if ((c == 'O' || c == 'o') &&
                (sqlite_term[i+1] == 'R' || sqlite_term[i+1] == 'r') &&
                (i == 0 || sqlite_term[i-1] == ' ') &&
                sqlite_term[i+2] == ' ') {
                // Replace with |
                pg_term[out_idx++] = '|';
                pg_term[out_idx++] = ' ';
                i += 2;  // Skip "OR" (loop will skip the space)
                continue;
            }
        }

        // Handle space
        if (c == ' ') {
            if (in_phrase) {
                // Inside phrase: use <-> for adjacent word matching
                pg_term[out_idx++] = ' ';
                pg_term[out_idx++] = '<';
                pg_term[out_idx++] = '-';
                pg_term[out_idx++] = '>';
                pg_term[out_idx++] = ' ';
            } else {
                // Skip space if next char starts an operator we handle
                if (i + 1 < len) {
                    char next = sqlite_term[i+1];
                    if (next == '-' ||
                        ((next == 'A' || next == 'a') && i + 4 < len &&
                         (sqlite_term[i+2] == 'N' || sqlite_term[i+2] == 'n')) ||
                        ((next == 'O' || next == 'o') && i + 3 < len &&
                         (sqlite_term[i+2] == 'R' || sqlite_term[i+2] == 'r'))) {
                        pg_term[out_idx++] = ' ';
                        continue;
                    }
                }
                // Regular space -> implicit AND
                pg_term[out_idx++] = ' ';
                pg_term[out_idx++] = '&';
                pg_term[out_idx++] = ' ';
            }
            continue;
        }

        // Escape special tsquery characters
        if (c == '\'' || c == '\\') {
            pg_term[out_idx++] = '\\';
            pg_term[out_idx++] = c;
            continue;
        }

        // Regular character
        pg_term[out_idx++] = c;
    }
    pg_term[out_idx] = '\0';
}

char* translate_fts(const char *sql) {
    if (!sql) return NULL;

    // Fast path: if no "fts4" in query, no FTS translation needed
    if (!strcasestr(sql, "fts4")) {
        return strdup(sql);
    }

    // Allocate result buffer (generous size)
    char *result = malloc(strlen(sql) * 3 + 1024);
    if (!result) return NULL;
    strcpy(result, sql);

    // List of table.column combinations to handle
    struct fts_map {
        const char *search;      // e.g. "fts4_metadata_titles_icu.title"
        const char *replacement; // e.g. "fts4_metadata_titles_icu.title_fts"
        const char *table;       // table name for unqualified column matching
    } maps[] = {
        { "fts4_metadata_titles_icu.title_sort", "fts4_metadata_titles_icu.title_fts", "fts4_metadata_titles_icu" },
        { "fts4_metadata_titles.title_sort", "fts4_metadata_titles.title_fts", "fts4_metadata_titles" },
        { "fts4_metadata_titles_icu.title", "fts4_metadata_titles_icu.title_fts", "fts4_metadata_titles_icu" },
        { "fts4_metadata_titles.title", "fts4_metadata_titles.title_fts", "fts4_metadata_titles" },
        { "fts4_tag_titles_icu.title", "fts4_tag_titles_icu.title_fts", "fts4_tag_titles_icu" },
        { "fts4_tag_titles.title", "fts4_tag_titles.title_fts", "fts4_tag_titles" },
        { "fts4_tag_titles_icu.tag", "fts4_tag_titles_icu.title_fts", "fts4_tag_titles_icu" },
        { "fts4_tag_titles.tag", "fts4_tag_titles.title_fts", "fts4_tag_titles" },
        // Unqualified column names (just "title" or "tag")
        { "title", "fts4_metadata_titles.title_fts", "fts4_metadata_titles" },
        { "tag", "fts4_tag_titles.title_fts", "fts4_tag_titles" },
        // Fallback for table match (implicit column)
        { "fts4_metadata_titles_icu", "fts4_metadata_titles_icu.title_fts", "fts4_metadata_titles_icu" },
        { "fts4_metadata_titles", "fts4_metadata_titles.title_fts", "fts4_metadata_titles" },
        { "fts4_tag_titles_icu", "fts4_tag_titles_icu.title_fts", "fts4_tag_titles_icu" },
        { "fts4_tag_titles", "fts4_tag_titles.title_fts", "fts4_tag_titles" },
        { NULL, NULL, NULL }
    };

    int changed = 0;

    for (int i = 0; maps[i].search; i++) {
        // For unqualified column names (no "."), verify the FTS table is in FROM clause
        int is_unqualified = (strchr(maps[i].search, '.') == NULL);
        if (is_unqualified && maps[i].table) {
            if (!strcasestr(result, maps[i].table)) {
                continue;  // Skip - table not in query
            }
        }

        char *pos = result;
        while ((pos = strcasestr(pos, maps[i].search)) != NULL) {
            // For unqualified names, ensure we're matching a standalone word
            if (is_unqualified) {
                // Check char before - must not be alphanumeric or '.'
                if (pos > result) {
                    char before = *(pos - 1);
                    if (is_ident_char(before) || before == '.') {
                        pos++;
                        continue;
                    }
                }
                // Check char after - must not be alphanumeric
                char after = *(pos + strlen(maps[i].search));
                if (is_ident_char(after)) {
                    pos++;
                    continue;
                }
            }

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
                        // HEAP allocation to prevent stack overflow (Plex uses ~388KB of stack)
                        char *search_term = calloc(256, 1);
                        char *pg_term = calloc(512, 1);
                        char *replacement = malloc(1024);

                        if (!search_term || !pg_term || !replacement) {
                            free(search_term);
                            free(pg_term);
                            free(replacement);
                            pos++;
                            continue;
                        }

                        // We have the full term
                        size_t term_len = quote_end - quote_start;
                        if (term_len > 254) term_len = 254;
                        strncpy(search_term, quote_start, term_len);

                        convert_fts_term(search_term, pg_term, 512);

                        // Construct replacement: col_fts @@ to_tsquery(...)
                        snprintf(replacement, 1024,
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

                        free(search_term);
                        free(pg_term);
                        free(replacement);
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

    // Debug: log if this is the OnDeck query
    if (strcasestr(sql, "metadata_item_settings") && strcasestr(sql, "grandparents")) {
        LOG_INFO("FIX_FORWARD_REF: Processing OnDeck query");
    }

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

    LOG_INFO("FIX_FORWARD_REF: Found forward reference, reordering JOINs");

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
        // HEAP allocation to prevent stack overflow (Plex uses ~388KB of stack)
        char *pattern1 = malloc(256);
        char *pattern2 = malloc(256);
        char *pattern3 = malloc(256);
        char *pattern4 = malloc(256);
        char *replacement = malloc(256);

        if (!pattern1 || !pattern2 || !pattern3 || !pattern4 || !replacement) {
            free(pattern1);
            free(pattern2);
            free(pattern3);
            free(pattern4);
            free(replacement);
            continue;
        }

        // Pattern with no space after comma, lowercase asc
        snprintf(pattern1, 256, "%s IS NULL,%s asc", columns[i], columns[i]);
        // Pattern with space after comma, lowercase asc
        snprintf(pattern2, 256, "%s IS NULL, %s asc", columns[i], columns[i]);
        // Pattern with no space after comma, uppercase ASC
        snprintf(pattern3, 256, "%s IS NULL,%s ASC", columns[i], columns[i]);
        // Pattern with space after comma, uppercase ASC
        snprintf(pattern4, 256, "%s IS NULL, %s ASC", columns[i], columns[i]);

        snprintf(replacement, 256, "%s ASC NULLS LAST", columns[i]);

        char *temp;

        // Try all pattern variations (case insensitive)
        temp = str_replace_nocase(current, pattern1, replacement);
        if (temp && strcmp(temp, current) != 0) {
            free(current);
            current = temp;
            free(pattern1);
            free(pattern2);
            free(pattern3);
            free(pattern4);
            free(replacement);
            continue;
        }
        free(temp);

        temp = str_replace_nocase(current, pattern2, replacement);
        if (temp && strcmp(temp, current) != 0) {
            free(current);
            current = temp;
            free(pattern1);
            free(pattern2);
            free(pattern3);
            free(pattern4);
            free(replacement);
            continue;
        }
        free(temp);

        temp = str_replace_nocase(current, pattern3, replacement);
        if (temp && strcmp(temp, current) != 0) {
            free(current);
            current = temp;
            free(pattern1);
            free(pattern2);
            free(pattern3);
            free(pattern4);
            free(replacement);
            continue;
        }
        free(temp);

        temp = str_replace_nocase(current, pattern4, replacement);
        if (temp && strcmp(temp, current) != 0) {
            free(current);
            current = temp;
            free(pattern1);
            free(pattern2);
            free(pattern3);
            free(pattern4);
            free(replacement);
            continue;
        }
        free(temp);

        // Free buffers if no match
        free(pattern1);
        free(pattern2);
        free(pattern3);
        free(pattern4);
        free(replacement);
    }

    return current;
}

char* translate_distinct_orderby(const char *sql) {
    if (!sql) return NULL;

    if (!strcasestr(sql, "distinct")) {
        return strdup(sql);
    }

    // Check if ORDER BY uses aggregate functions or random()
    const char *order_by_pos = strcasestr(sql, "order by");
    if (order_by_pos) {
        // Look for aggregate functions or random() in ORDER BY clause
        // random() is incompatible with DISTINCT because it's not in SELECT list
        const char *incompatible_funcs[] = {"count(", "sum(", "avg(", "max(", "min(", "random()", NULL};
        for (int i = 0; incompatible_funcs[i]; i++) {
            if (strcasestr(order_by_pos, incompatible_funcs[i])) {
                // Found incompatible function in ORDER BY - remove DISTINCT
                LOG_INFO("Removing DISTINCT due to ORDER BY %s", incompatible_funcs[i]);
                char *result = str_replace_nocase(sql, "select distinct", "select");
                return result ? result : strdup(sql);
            }
        }

        // Special case: decade query - ORDER BY metadata_items.year but SELECT has year/10*10 AS year
        // Fix: Replace ORDER BY metadata_items.year with ORDER BY year (the alias)
        if (strcasestr(sql, "year/10*10") && strcasestr(sql, "as year")) {
            const char *order_col = strcasestr(order_by_pos, "metadata_items.year");
            if (order_col) {
                LOG_INFO("Fixing decade query: ORDER BY metadata_items.year -> ORDER BY year");
                char *result = str_replace_nocase(sql, "order by metadata_items.year", "order by year");
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
                // HEAP allocation to prevent stack overflow (Plex uses ~388KB of stack)
                char *col_ref = malloc(256);
                if (!col_ref) continue;

                const char *start = pattern_pos;
                const char *end = start;

                // Find end of column reference
                while (*end && (is_ident_char(*end) || *end == '.' || *end == '"')) {
                    end++;
                }

                size_t col_len = end - start;
                if (col_len < 256) {
                    memcpy(col_ref, start, col_len);
                    col_ref[col_len] = '\0';

                    // Check if this column is in the SELECT clause
                    if (!column_in_select(sql, col_ref)) {
                        LOG_INFO("Removing DISTINCT due to ORDER BY column not in SELECT: %s", col_ref);
                        free(col_ref);
                        char *result = str_replace_nocase(sql, "select distinct", "select");
                        return result ? result : strdup(sql);
                    }
                }
                free(col_ref);
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

// ============================================================================
// Translate COLLATE NOCASE to PostgreSQL LOWER() or ILIKE
// SQLite: col COLLATE NOCASE = 'val'  -> LOWER(col) = LOWER('val')
// SQLite: col LIKE '%x%' COLLATE NOCASE -> col ILIKE '%x%'
// SQLite: ORDER BY col COLLATE NOCASE -> ORDER BY LOWER(col)
// ============================================================================

char* translate_collate_nocase(const char *sql) {
    if (!sql) return NULL;

    // Fast path: no COLLATE NOCASE
    if (!strcasestr(sql, "collate nocase")) {
        return strdup(sql);
    }

    char *result = malloc(strlen(sql) * 2 + 256);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;

    while (*p) {
        // Look for "COLLATE NOCASE" pattern
        const char *collate_pos = strcasestr(p, "collate nocase");
        if (!collate_pos) {
            // No more occurrences, copy rest
            strcpy(out, p);
            out += strlen(p);
            break;
        }

        // Find what precedes COLLATE NOCASE
        // Could be: col COLLATE NOCASE, 'val' COLLATE NOCASE, or LIKE 'x' COLLATE NOCASE

        // Check if this is a LIKE ... COLLATE NOCASE pattern
        // Search backwards for LIKE
        const char *scan_back = collate_pos - 1;
        while (scan_back > p && isspace(*scan_back)) scan_back--;

        // Check if there's a quoted string before COLLATE
        int is_like_pattern = 0;
        const char *like_pos = NULL;

        if (*scan_back == '\'') {
            // There's a string literal before COLLATE NOCASE
            // Look further back for LIKE keyword
            const char *before_str = scan_back - 1;
            while (before_str > p && *before_str != '\'') before_str--;
            if (*before_str == '\'') {
                before_str--;
                while (before_str > p && isspace(*before_str)) before_str--;
                // Check for LIKE/GLOB keyword
                if (before_str - 3 >= p && strncasecmp(before_str - 3, "like", 4) == 0) {
                    is_like_pattern = 1;
                    like_pos = before_str - 3;
                } else if (before_str - 3 >= p && strncasecmp(before_str - 3, "glob", 4) == 0) {
                    is_like_pattern = 1;
                    like_pos = before_str - 3;
                }
            }
        }

        if (is_like_pattern && like_pos) {
            // Pattern: col LIKE 'x' COLLATE NOCASE -> col ILIKE 'x'
            // Copy up to LIKE
            size_t prefix_len = like_pos - p;
            memcpy(out, p, prefix_len);
            out += prefix_len;

            // Write ILIKE instead
            memcpy(out, "ILIKE", 5);
            out += 5;

            // Skip "LIKE" or "GLOB"
            p = like_pos + 4;

            // Copy until COLLATE NOCASE
            while (p < collate_pos) {
                *out++ = *p++;
            }

            // Skip "COLLATE NOCASE"
            p = collate_pos + 14;
        } else {
            // Pattern: col COLLATE NOCASE = 'val' or ORDER BY col COLLATE NOCASE
            // Need to wrap the preceding identifier in LOWER()

            // Find the start of the identifier before COLLATE
            const char *id_end = collate_pos - 1;
            while (id_end > p && isspace(*id_end)) id_end--;

            const char *id_start = id_end;
            // Handle quoted identifiers and table.column patterns
            while (id_start > p) {
                char c = *(id_start - 1);
                if (is_ident_char(c) || c == '.' || c == '"' || c == '`') {
                    id_start--;
                } else {
                    break;
                }
            }

            // Copy everything before the identifier
            size_t prefix_len = id_start - p;
            memcpy(out, p, prefix_len);
            out += prefix_len;

            // Write LOWER( identifier )
            memcpy(out, "LOWER(", 6);
            out += 6;

            size_t id_len = (id_end + 1) - id_start;
            memcpy(out, id_start, id_len);
            out += id_len;

            *out++ = ')';

            // Skip past "COLLATE NOCASE"
            p = collate_pos + 14;

            // Check if there's a comparison operator and value after
            const char *after = skip_ws(p);
            if (*after == '=' || *after == '!' || strncasecmp(after, "like", 4) == 0) {
                // Copy the operator
                if (*after == '=') {
                    *out++ = ' ';
                    *out++ = '=';
                    *out++ = ' ';
                    p = after + 1;
                } else if (*after == '!' && *(after+1) == '=') {
                    *out++ = ' ';
                    *out++ = '!';
                    *out++ = '=';
                    *out++ = ' ';
                    p = after + 2;
                } else if (strncasecmp(after, "like", 4) == 0) {
                    // Convert to ILIKE
                    memcpy(out, " ILIKE ", 7);
                    out += 7;
                    p = after + 4;
                }

                // Skip whitespace
                while (*p && isspace(*p)) p++;

                // Check if next is a string literal - wrap in LOWER()
                if (*p == '\'') {
                    // Find end of string
                    const char *str_start = p;
                    p++;
                    while (*p && (*p != '\'' || *(p-1) == '\\')) p++;
                    if (*p == '\'') p++;

                    // Write LOWER('value')
                    memcpy(out, "LOWER(", 6);
                    out += 6;
                    size_t str_len = p - str_start;
                    memcpy(out, str_start, str_len);
                    out += str_len;
                    *out++ = ')';
                }
            }
        }
    }

    *out = '\0';
    return result;
}

char* fix_integer_text_mismatch(const char *sql) {
    if (!sql) return NULL;

    char *current = strdup(sql);
    if (!current) return NULL;
    char *temp;

    // Debug: log what we're checking
    if (strcasestr(current, "taggings") && strcasestr(current, "json_array_elements")) {
        LOG_INFO("fix_integer_text_mismatch checking taggings query: %.300s", current);
    }

    // Pattern 1: metadata_items.id IN (SELECT taggings.metadata_item_id
    if (strcasestr(current, "metadata_items.id in (select taggings.metadata_item_id")) {
        LOG_INFO("Fixing integer/text mismatch pattern 1");
        temp = str_replace_nocase(current,
            "metadata_items.id in (select taggings.metadata_item_id",
            "metadata_items.id::text in (select taggings.metadata_item_id::text");
        if (temp) { free(current); current = temp; }
    }

    // Pattern 2: metadata_item_id IN (SELECT ... FROM json_array_elements
    // metadata_item_id is INTEGER, value::text is TEXT - cast column to text
    // Match both backticks (SQLite style) and double quotes (translated style)
    if (strcasestr(current, "`metadata_item_id` in") && strcasestr(current, "json_array_elements")) {
        LOG_INFO("Fixing integer/text mismatch pattern 2a (metadata_item_id backtick)");
        temp = str_replace_nocase(current,
            "`metadata_item_id` in",
            "`metadata_item_id`::text in");
        if (temp) { free(current); current = temp; }
    }
    if (strcasestr(current, "\"metadata_item_id\" in") && strcasestr(current, "json_array_elements")) {
        LOG_INFO("Fixing integer/text mismatch pattern 2b (metadata_item_id quote)");
        temp = str_replace_nocase(current,
            "\"metadata_item_id\" in",
            "\"metadata_item_id\"::text in");
        if (temp) { free(current); current = temp; }
    }

    // Note: Pattern 3 (tag_id) removed - tag_id compares with tg.id (both INTEGER)
    // Only metadata_item_id directly compares with json_array_elements values

    // Pattern 4: status IN (SELECT ... FROM json_array_elements
    // di."status" is INTEGER in download_queue_items, value::text is TEXT
    // Direct pattern match for the download_queue_items query
    if (strcasestr(current, "download_queue_items") && strcasestr(current, "json_array_elements")) {
        LOG_INFO("Pattern 4 matched: download_queue_items with json_array_elements");
        LOG_INFO("Query before fix: %.200s", current);
        // Match: di."status" IN (SELECT value::text
        // Fix:   di."status"::text IN (SELECT value::text
        temp = str_replace_nocase(current,
            "di.\"status\" IN",
            "di.\"status\"::text IN");
        if (temp && temp != current) {
            LOG_INFO("Pattern 4 replacement succeeded");
            free(current);
            current = temp;
        } else {
            LOG_INFO("Pattern 4 replacement did NOT change anything");
        }
    }

    // Generic pattern for any "status" column with json_array_elements
    if (strcasestr(current, "\"status\" IN") && strcasestr(current, "json_array_elements")) {
        temp = str_replace_nocase(current,
            "\"status\" IN",
            "\"status\"::text IN");
        if (temp) { free(current); current = temp; }
    }

    return current;
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

    // Check if the query contains ->> operator
    if (!strstr(sql, "->>")) {
        return strdup(sql);
    }

    // Check for ->> with parameter ($N) - needs column::json cast
    // Pattern: "column"->>$N or column->>$N
    // Fix: insert ::json before ->>$N
    const char *param_pattern = strstr(sql, "->>$");
    if (param_pattern) {
        LOG_INFO("Fixing JSON ->> operator with parameter on TEXT columns");
        char *result = malloc(strlen(sql) * 2 + 256);
        if (!result) return NULL;

        char *out = result;
        const char *p = sql;

        while (*p) {
            // Look for pattern: ->>$N and insert ::json before it
            if (strncmp(p, "->>$", 4) == 0) {
                // Insert ::json cast before the ->> operator
                out += sprintf(out, "::json");

                // Copy ->>$N
                while (*p && (*p == '-' || *p == '>' || *p == '$' || isdigit(*p))) {
                    *out++ = *p++;
                }
                continue;
            }
            *out++ = *p++;
        }
        *out = '\0';
        return result;
    }

    // Check for ->> with '$.key' pattern
    if (!strstr(sql, "'$.")) {
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
                        // HEAP allocation to prevent stack overflow (Plex uses ~388KB of stack)
                        char *json_key = malloc(256);
                        if (!json_key) {
                            *out++ = *p++;
                            continue;
                        }

                        size_t key_len = key_end - key_start;
                        if (key_len < 256) {
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
                                free(json_key);
                                p = after + 7;
                                continue;
                            } else if (strncmp(after, "<", 1) == 0) {
                                // column ->> '$.key' < 'value'
                                // -> column LIKE '%"key":"0"%' (simplified for version checking)
                                out += sprintf(out, " LIKE '%%\"%s\":\"0\"%%'", json_key);
                                free(json_key);
                                // Skip the < and the value
                                const char *quote1 = strchr(after, '\'');
                                if (quote1) {
                                    const char *quote2 = strchr(quote1 + 1, '\'');
                                    if (quote2) {
                                        p = quote2 + 1;
                                        continue;
                                    }
                                }
                                // Fallback: skip to after the JSON operator
                                p = key_end + 1;
                                continue;
                            }
                        }
                        free(json_key);
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

// ============================================================================
// Fix JOIN order for PostgreSQL (tables must be defined before referenced)
// ============================================================================
//
// SQLite allows: JOIN table AS alias ON alias.col = table.col ... JOIN table ON ...
// PostgreSQL requires: JOIN table ON ... JOIN table AS alias ON alias.col = table.col
//
// This fixes the OnDeck query pattern where metadata_items is referenced before defined.

char* fix_join_order(const char *sql) {
    if (!sql) return NULL;

    // Only process queries with metadata_item_settings and multiple metadata_items joins
    if (!strcasestr(sql, "metadata_item_settings") ||
        !strcasestr(sql, "join metadata_items as parents") ||
        !strcasestr(sql, "join metadata_items on")) {
        return strdup(sql);
    }

    // Find the positions of the key JOIN clauses
    char *parents_join = strcasestr(sql, "join metadata_items as parents");
    char *base_join = strcasestr(sql, "join metadata_items on");

    if (!parents_join || !base_join) {
        return strdup(sql);
    }

    // If parents_join comes BEFORE base_join, we need to reorder
    if (parents_join < base_join) {
        LOG_INFO("FIX_JOIN_ORDER: Reordering metadata_items joins for PostgreSQL compatibility");

        // Find the end of base_join clause (up to WHERE or next JOIN that isn't part of this pattern)
        char *where_pos = strcasestr(base_join, " where ");

        if (!where_pos) {
            return strdup(sql);  // Can't find WHERE, don't modify
        }

        // Calculate the base_join clause text
        // It starts at base_join and goes up to (but not including) " where "
        size_t base_join_len = where_pos - base_join;
        char *base_join_text = malloc(base_join_len + 1);
        if (!base_join_text) return strdup(sql);
        strncpy(base_join_text, base_join, base_join_len);
        base_join_text[base_join_len] = '\0';

        // Build the new SQL:
        // 1. Everything before parents_join
        // 2. base_join clause (moved here)
        // 3. parents_join clause
        // 4. grandparents_join clause (if exists)
        // 5. WHERE clause and rest

        size_t result_size = strlen(sql) + 100;
        char *result = malloc(result_size);
        if (!result) {
            free(base_join_text);
            return strdup(sql);
        }

        // Copy everything before parents_join
        size_t prefix_len = parents_join - sql;
        strncpy(result, sql, prefix_len);
        result[prefix_len] = '\0';

        // Add base_join (without the WHERE part)
        strcat(result, base_join_text);
        strcat(result, " ");

        // Add parents_join up to (but not including) base_join
        size_t parents_clause_len = base_join - parents_join;
        strncat(result, parents_join, parents_clause_len);

        // Add WHERE and everything after
        strcat(result, where_pos);

        free(base_join_text);

        LOG_INFO("FIX_JOIN_ORDER: Result: %.200s", result);
        return result;
    }

    return strdup(sql);
}
