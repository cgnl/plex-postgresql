/*
 * SQL Translator - Placeholder Translation
 * Converts SQLite placeholders (? and :name) to PostgreSQL ($1, $2, ...)
 */

#include "sql_translator.h"
#include "sql_translator_internal.h"

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
            // CRITICAL FIX: Ensure param_names array is allocated and initialized
            // even for ? placeholders, to prevent uninitialized memory access
            // when mixed with :name placeholders
            if (param_count >= param_capacity) {
                param_capacity = param_capacity ? param_capacity * 2 : 16;
                char **new_names = realloc(param_names, param_capacity * sizeof(char*));
                if (new_names) {
                    // Initialize new slots to NULL
                    for (int i = param_names ? (param_capacity / 2) : 0; i < param_capacity; i++) {
                        new_names[i] = NULL;
                    }
                    param_names = new_names;
                }
            }
            // Set this slot to NULL explicitly (? placeholder has no name)
            if (param_names) {
                param_names[param_count] = NULL;
            }
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
                        int old_capacity = param_capacity;
                        param_capacity = param_capacity ? param_capacity * 2 : 16;
                        char **new_names = realloc(param_names, param_capacity * sizeof(char*));
                        if (new_names) {
                            // Initialize new slots to NULL
                            for (int i = old_capacity; i < param_capacity; i++) {
                                new_names[i] = NULL;
                            }
                            param_names = new_names;
                        }
                    }

                    if (param_names) {
                        param_names[param_count] = malloc(name_len + 1);
                        if (param_names[param_count]) {
                            memcpy(param_names[param_count], name_start, name_len);
                            param_names[param_count][name_len] = '\0';
                        }
                    }

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
