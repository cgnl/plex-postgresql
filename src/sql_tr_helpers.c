/*
 * SQL Translator - Helper Functions
 * String manipulation utilities for SQL translation
 */

#include "sql_translator_internal.h"

// ============================================================================
// String Replace (case-sensitive)
// ============================================================================

char* str_replace(const char *str, const char *old, const char *new_str) {
    if (!str || !old || !new_str) return NULL;

    size_t old_len = strlen(old);
    if (old_len == 0) return strdup(str);

    // Fast path: check if first char exists before expensive strstr
    char first = old[0];
    int found_first = 0;
    for (const char *scan = str; *scan; scan++) {
        if (*scan == first) {
            found_first = 1;
            break;
        }
    }
    if (!found_first) return strdup(str);

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

// ============================================================================
// String Replace (case-insensitive)
// ============================================================================

char* str_replace_nocase(const char *str, const char *old, const char *new_str) {
    if (!str || !old || !new_str) return NULL;

    size_t old_len = strlen(old);
    if (old_len == 0) return strdup(str);

    // Fast path: check if first char of pattern exists (case insensitive)
    // This avoids expensive strcasestr scan for patterns that can't match
    char first = old[0];
    char first_lower = (first >= 'A' && first <= 'Z') ? (first | 0x20) : first;
    char first_upper = (first >= 'a' && first <= 'z') ? (first & ~0x20) : first;
    int found_first = 0;
    for (const char *scan = str; *scan; scan++) {
        char c = *scan;
        if (c == first_lower || c == first_upper) {
            found_first = 1;
            break;
        }
    }
    if (!found_first) return strdup(str);

    size_t new_len = strlen(new_str);

    // Count occurrences first (case insensitive)
    int count = 0;
    const char *p = str;
    while ((p = strcasestr(p, old)) != NULL) {
        count++;
        p += old_len;
    }

    if (count == 0) return strdup(str);

    // Allocate exact size needed
    size_t str_len = strlen(str);
    size_t result_len = str_len + count * ((ssize_t)new_len - (ssize_t)old_len) + 1;
    char *result = malloc(result_len);
    if (!result) return NULL;

    char *out = result;
    p = str;

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

// Note: skip_ws() and is_ident_char() are now inline in sql_translator_internal.h

// ============================================================================
// Extract Function Argument (handles nested parentheses)
// ============================================================================

const char* extract_arg(const char *start, char *buf, size_t bufsize) {
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
