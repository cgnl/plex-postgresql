/*
 * SQL Translator - UPSERT Translation
 * Converts SQLite INSERT OR REPLACE to PostgreSQL ON CONFLICT DO UPDATE
 */

#include "sql_translator_internal.h"
#include "pg_logging.h"
#include <ctype.h>

// ============================================================================
// Table Conflict Resolution Mapping
// Maps table names to their conflict resolution columns (PRIMARY KEY or UNIQUE)
// ============================================================================

typedef struct {
    const char *table_name;
    const char *conflict_columns;  // Comma-separated list
    int update_all_columns;        // 1 = update all columns, 0 = custom logic
} conflict_target_t;

// This mapping is based on the PostgreSQL schema analysis
// Tables with PRIMARY KEY 'id' use (id) as conflict target
// Tables with UNIQUE constraints use those columns
static const conflict_target_t conflict_targets[] = {
    // Tables with simple id PRIMARY KEY
    {"metadata_items", "id", 1},
    {"media_items", "id", 1},
    {"media_parts", "id", 1},
    {"media_streams", "id", 1},
    {"tags", "id", 1},
    {"taggings", "id", 1},
    {"statistics_media", "id", 1},
    {"statistics_bandwidth", "id", 1},
    {"statistics_resources", "id", 1},
    {"play_queue_generators", "id", 1},
    {"play_queue_items", "id", 1},
    {"play_queues", "id", 1},
    {"activities", "id", 1},
    {"accounts", "id", 1},
    {"devices", "id", 1},
    {"directories", "id", 1},
    {"library_sections", "id", 1},
    {"locations", "id", 1},
    {"plugins", "id", 1},
    {"media_grabs", "id", 1},
    {"metadata_relations", "id", 1},
    {"versioned_metadata_items", "id", 1},
    {"external_metadata_sources", "id", 1},
    {"blobs", "id", 1},

    // Tables with UNIQUE constraints
    {"metadata_item_settings", "account_id, guid", 0},  // Custom logic in convert_metadata_settings_insert_to_upsert
    {"locatables", "location_id, locatable_id, locatable_type", 1},
    {"location_places", "location_id, guid", 1},
    {"media_stream_settings", "media_stream_id, account_id", 1},
    {"preferences", "name", 1},

    {NULL, NULL, 0}  // Sentinel
};

// ============================================================================
// Helper: Find conflict target for table
// ============================================================================

static const conflict_target_t* find_conflict_target(const char *table_name) {
    if (!table_name) return NULL;

    // Remove schema prefix if present (e.g., "plex.metadata_items" -> "metadata_items")
    const char *dot = strchr(table_name, '.');
    if (dot) table_name = dot + 1;

    for (int i = 0; conflict_targets[i].table_name; i++) {
        if (strcasecmp(table_name, conflict_targets[i].table_name) == 0) {
            return &conflict_targets[i];
        }
    }

    // Default: assume id PRIMARY KEY for tables not in the list
    static const conflict_target_t default_target = {"*", "id", 1};
    return &default_target;
}

// ============================================================================
// Helper: Extract table name from INSERT statement
// ============================================================================

static char* extract_table_name(const char *sql) {
    // Find "INSERT OR REPLACE INTO table_name"
    const char *into_pos = strcasestr(sql, "INTO");
    if (!into_pos) return NULL;

    const char *p = into_pos + 4;
    while (*p && isspace(*p)) p++;

    // Extract table name (until space, '(' or end)
    const char *start = p;
    while (*p && !isspace(*p) && *p != '(' && *p != ';') p++;

    size_t len = p - start;
    char *table = malloc(len + 1);
    if (table) {
        memcpy(table, start, len);
        table[len] = '\0';
    }

    return table;
}

// ============================================================================
// Helper: Extract column list from INSERT statement
// Returns NULL if no column list (INSERT INTO table VALUES...)
// ============================================================================

static char* extract_column_list(const char *sql) {
    // Find table name first
    const char *into_pos = strcasestr(sql, "INTO");
    if (!into_pos) return NULL;

    const char *p = into_pos + 4;
    while (*p && isspace(*p)) p++;

    // Skip table name
    while (*p && !isspace(*p) && *p != '(') p++;
    while (*p && isspace(*p)) p++;

    // Check for column list (starts with '(')
    if (*p != '(') return NULL;

    p++;  // Skip opening '('
    const char *col_start = p;

    // Find matching closing ')'
    int depth = 1;
    while (*p && depth > 0) {
        if (*p == '(') depth++;
        else if (*p == ')') depth--;
        if (depth > 0) p++;
    }

    if (depth != 0) return NULL;  // Unbalanced parentheses

    size_t len = p - col_start;
    char *columns = malloc(len + 1);
    if (columns) {
        memcpy(columns, col_start, len);
        columns[len] = '\0';
    }

    return columns;
}

// ============================================================================
// Helper: Parse column names from column list string
// ============================================================================

typedef struct {
    char **names;
    int count;
} column_list_t;

static column_list_t parse_columns(const char *col_list) {
    column_list_t result = {NULL, 0};
    if (!col_list) return result;

    // Count columns (by counting commas + 1)
    int count = 1;
    for (const char *p = col_list; *p; p++) {
        if (*p == ',') count++;
    }

    result.names = malloc(count * sizeof(char*));
    if (!result.names) return result;

    // Parse individual column names
    const char *p = col_list;
    int idx = 0;

    while (*p && idx < count) {
        // Skip whitespace
        while (*p && isspace(*p)) p++;

        // Extract column name (may be quoted)
        const char *start = p;
        if (*p == '"') {
            // Quoted column name
            p++;
            start = p;
            while (*p && *p != '"') p++;
            size_t len = p - start;
            result.names[idx] = malloc(len + 1);
            if (result.names[idx]) {
                memcpy(result.names[idx], start, len);
                result.names[idx][len] = '\0';
            }
            if (*p == '"') p++;
        } else {
            // Unquoted column name
            while (*p && !isspace(*p) && *p != ',' && *p != ')') p++;
            size_t len = p - start;
            result.names[idx] = malloc(len + 1);
            if (result.names[idx]) {
                memcpy(result.names[idx], start, len);
                result.names[idx][len] = '\0';
            }
        }

        idx++;

        // Skip to next column
        while (*p && isspace(*p)) p++;
        if (*p == ',') {
            p++;
            while (*p && isspace(*p)) p++;
        }
    }

    result.count = idx;
    return result;
}

static void free_column_list(column_list_t *list) {
    if (!list || !list->names) return;
    for (int i = 0; i < list->count; i++) {
        if (list->names[i]) free(list->names[i]);
    }
    free(list->names);
    list->names = NULL;
    list->count = 0;
}

// ============================================================================
// Helper: Generate ON CONFLICT DO UPDATE clause
// ============================================================================

static char* generate_on_conflict_clause(const conflict_target_t *target, column_list_t *columns) {
    if (!target) return NULL;

    // Estimate size (generous)
    size_t size = 1024;
    if (columns && columns->count > 0) {
        size += columns->count * 100;  // ~100 bytes per column
    }

    char *result = malloc(size);
    if (!result) return NULL;

    char *p = result;

    // ON CONFLICT (conflict_columns)
    p += sprintf(p, " ON CONFLICT (%s) DO UPDATE SET ", target->conflict_columns);

    // Generate SET clause
    // Skip conflict columns themselves (they can't be updated)
    int first = 1;

    if (columns && columns->count > 0) {
        for (int i = 0; i < columns->count; i++) {
            const char *col = columns->names[i];
            if (!col) continue;

            // Skip conflict columns (id, account_id, guid, etc.)
            if (strcasecmp(col, "id") == 0) continue;
            if (strcasestr(target->conflict_columns, col) != NULL) continue;

            if (!first) p += sprintf(p, ", ");
            first = 0;

            // Special handling for certain columns
            if (strcasecmp(col, "updated_at") == 0 ||
                strcasecmp(col, "changed_at") == 0) {
                // Use COALESCE to default to current timestamp if NULL
                p += sprintf(p, "%s = COALESCE(EXCLUDED.%s, EXTRACT(EPOCH FROM NOW())::bigint)", col, col);
            } else if (strcasecmp(col, "view_count") == 0) {
                // Take maximum value (like in metadata_item_settings)
                p += sprintf(p, "%s = GREATEST(EXCLUDED.%s, plex.%s.%s, 0)",
                            col, col, target->table_name, col);
            } else {
                // Standard update: use EXCLUDED value
                p += sprintf(p, "%s = EXCLUDED.%s", col, col);
            }
        }
    } else {
        // No column list provided - we can't generate a complete SET clause
        // This happens with "INSERT INTO table VALUES (...)" without column names
        // Return NULL to indicate we can't handle this case
        free(result);
        return NULL;
    }

    // Add RETURNING id if table has id column
    if (strcasestr(target->conflict_columns, "id") != NULL) {
        p += sprintf(p, " RETURNING id");
    }

    return result;
}

// ============================================================================
// Main Function: Translate INSERT OR REPLACE to ON CONFLICT DO UPDATE
// ============================================================================

char* translate_insert_or_replace(const char *sql) {
    if (!sql) return NULL;

    // Only handle INSERT OR REPLACE statements
    if (!strcasestr(sql, "INSERT OR REPLACE")) {
        return strdup(sql);
    }

    LOG_INFO("Translating INSERT OR REPLACE: %.200s", sql);

    // Extract table name
    char *table_name = extract_table_name(sql);
    if (!table_name) {
        LOG_ERROR("Failed to extract table name from: %s", sql);
        return strdup(sql);
    }

    // Find conflict target for this table
    const conflict_target_t *target = find_conflict_target(table_name);
    if (!target) {
        LOG_ERROR("No conflict target found for table: %s", table_name);
        free(table_name);
        return strdup(sql);
    }

    LOG_INFO("Table: %s, Conflict columns: %s", table_name, target->conflict_columns);

    // Special case: metadata_item_settings uses custom upsert logic
    if (strcasecmp(table_name, "metadata_item_settings") == 0) {
        free(table_name);
        // This will be handled by convert_metadata_settings_insert_to_upsert
        // Just remove OR REPLACE for now
        char *result = str_replace_nocase(sql, "INSERT OR REPLACE INTO", "INSERT INTO");
        return result;
    }

    // Extract column list
    char *col_list_str = extract_column_list(sql);
    column_list_t columns = {NULL, 0};

    if (col_list_str) {
        columns = parse_columns(col_list_str);
        free(col_list_str);
    }

    // Generate ON CONFLICT clause
    char *on_conflict = generate_on_conflict_clause(target, &columns);

    free_column_list(&columns);

    if (!on_conflict) {
        LOG_ERROR("Failed to generate ON CONFLICT clause for table: %s", table_name);
        free(table_name);
        // Fallback: just remove OR REPLACE (will likely fail but at least tries)
        char *result = str_replace_nocase(sql, "INSERT OR REPLACE INTO", "INSERT INTO");
        return result;
    }

    // Build final SQL:
    // 1. Replace "INSERT OR REPLACE INTO" with "INSERT INTO"
    // 2. Append ON CONFLICT clause before semicolon or at end

    char *step1 = str_replace_nocase(sql, "INSERT OR REPLACE INTO", "INSERT INTO");
    if (!step1) {
        free(table_name);
        free(on_conflict);
        return strdup(sql);
    }

    // Find end of statement (semicolon or end of string)
    size_t sql_len = strlen(step1);
    const char *end = step1 + sql_len;

    // Trim trailing whitespace and semicolon
    while (end > step1 && (isspace(*(end-1)) || *(end-1) == ';')) end--;

    size_t prefix_len = end - step1;
    size_t on_conflict_len = strlen(on_conflict);

    char *result = malloc(prefix_len + on_conflict_len + 2);  // +2 for possible ';' and '\0'
    if (!result) {
        free(step1);
        free(table_name);
        free(on_conflict);
        return strdup(sql);
    }

    memcpy(result, step1, prefix_len);
    memcpy(result + prefix_len, on_conflict, on_conflict_len);
    result[prefix_len + on_conflict_len] = '\0';

    free(step1);
    free(table_name);
    free(on_conflict);

    LOG_INFO("Translated to: %.200s", result);

    return result;
}
