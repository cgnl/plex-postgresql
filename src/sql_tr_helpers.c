/*
 * SQL Translator - Helper Functions
 * String manipulation utilities for SQL translation
 */

#include "sql_translator_internal.h"

// ============================================================================
// Safe strcasestr implementation
// musl's strcasestr has issues with certain inputs, so we implement our own
// ============================================================================

char* safe_strcasestr(const char *haystack, const char *needle) {
    if (!haystack || !needle) return NULL;
    if (!*needle) return (char*)haystack;

    size_t needle_len = strlen(needle);

    for (const char *p = haystack; *p; p++) {
        if (strncasecmp(p, needle, needle_len) == 0) {
            return (char*)p;
        }
    }
    return NULL;
}

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
// String Replace (case-insensitive) - Single-pass optimized
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
    size_t str_len = strlen(str);

    // Estimate buffer size (assume max 8 replacements, expand if needed)
    size_t diff = (new_len > old_len) ? (new_len - old_len) : 0;
    size_t buf_size = str_len + 8 * diff + 64;
    char *result = malloc(buf_size);
    if (!result) return NULL;

    char *out = result;
    const char *p = str;
    const char *match;

    // Single pass: find matches with strcasestr, copy segments with memcpy
    while ((match = strcasestr(p, old)) != NULL) {
        // Copy everything before the match
        size_t prefix_len = match - p;
        if (prefix_len > 0) {
            // Check if we need to expand buffer
            size_t used = out - result;
            size_t needed = used + prefix_len + new_len + (str_len - (match - str)) + 1;
            if (needed > buf_size) {
                buf_size = needed + 64;
                char *new_buf = realloc(result, buf_size);
                if (!new_buf) { free(result); return NULL; }
                out = new_buf + used;
                result = new_buf;
            }
            memcpy(out, p, prefix_len);
            out += prefix_len;
        }

        // Copy replacement
        memcpy(out, new_str, new_len);
        out += new_len;

        // Move past the match
        p = match + old_len;
    }

    // Copy remainder (after last match)
    size_t remainder = strlen(p);
    if (remainder > 0) {
        size_t used = out - result;
        if (used + remainder + 1 > buf_size) {
            buf_size = used + remainder + 1;
            char *new_buf = realloc(result, buf_size);
            if (!new_buf) { free(result); return NULL; }
            out = new_buf + used;
            result = new_buf;
        }
        memcpy(out, p, remainder);
        out += remainder;
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
