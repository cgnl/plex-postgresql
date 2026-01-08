/*
 * Plex PostgreSQL Interposing Shim - Parameter Binding
 *
 * Handles sqlite3_bind_* function interposition.
 * These functions capture bound parameters for PostgreSQL queries.
 */

#include "db_interpose.h"

// ============================================================================
// Helper Functions
// ============================================================================

// Helper to map SQLite parameter index to PostgreSQL parameter index
// Handles named parameters (:name) by finding the correct position
int pg_map_param_index(pg_stmt_t *pg_stmt, sqlite3_stmt *pStmt, int sqlite_idx) {
    if (!pg_stmt) {
        LOG_DEBUG("pg_map_param_index: no pg_stmt, using direct mapping idx=%d -> %d", sqlite_idx, sqlite_idx - 1);
        return sqlite_idx - 1;
    }

    // If we have named parameters, we need to map them
    if (pg_stmt->param_names && pg_stmt->param_count > 0) {
        // Get the parameter name from SQLite
        const char *param_name = sqlite3_bind_parameter_name(pStmt, sqlite_idx);
        LOG_DEBUG("pg_map_param_index: sqlite_idx=%d, param_name=%s, param_count=%d",
                 sqlite_idx, param_name ? param_name : "NULL", pg_stmt->param_count);

        if (param_name) {
            // Remove the : prefix if present
            const char *clean_name = param_name;
            if (param_name[0] == ':') clean_name = param_name + 1;

            // Debug: show all param names
            for (int i = 0; i < pg_stmt->param_count && i < 5; i++) {
                LOG_DEBUG("  param_names[%d] = %s", i, pg_stmt->param_names[i] ? pg_stmt->param_names[i] : "NULL");
            }

            // Find this name in our param_names array
            for (int i = 0; i < pg_stmt->param_count; i++) {
                if (pg_stmt->param_names[i] && strcmp(pg_stmt->param_names[i], clean_name) == 0) {
                    LOG_DEBUG("  -> Found match at pg_idx=%d", i);
                    return i;  // Found it! Return the PostgreSQL position
                }
            }
            LOG_ERROR("Named parameter '%s' not found in translation (sqlite_idx=%d)", clean_name, sqlite_idx);
        } else {
            LOG_DEBUG("  -> No parameter name, using direct mapping");
        }
    } else {
        LOG_DEBUG("pg_map_param_index: no param_names (count=%d), using direct mapping idx=%d -> %d",
                 pg_stmt->param_count, sqlite_idx, sqlite_idx - 1);
    }

    // For positional parameters (?) or if name not found, use direct mapping
    return sqlite_idx - 1;
}

// Helper: check if data contains binary bytes (non-UTF8 safe)
// Returns 1 if data contains bytes that would be invalid in UTF-8 text
int contains_binary_bytes(const unsigned char *data, size_t len) {
    if (!data || len == 0) return 0;

    for (size_t i = 0; i < len; i++) {
        unsigned char c = data[i];
        // Control characters (except tab, newline, carriage return)
        if (c < 0x20 && c != 0x09 && c != 0x0A && c != 0x0D) {
            return 1;
        }
        // Check for invalid UTF-8 lead bytes or 0x7F (DEL)
        if (c == 0x7F || c == 0xC0 || c == 0xC1 || c >= 0xF5) {
            return 1;
        }
        // Gzip magic bytes (0x1f 0x8b) - common binary data
        if (i == 0 && len >= 2 && c == 0x1f && data[1] == 0x8b) {
            return 1;
        }
    }
    return 0;
}

// Helper: convert binary data to PostgreSQL hex format (\xHEXHEX...)
// Caller must free the returned string
char* bytes_to_pg_hex(const unsigned char *data, size_t len) {
    if (!data || len == 0) return strdup("");

    // Format: \x followed by 2 hex chars per byte
    size_t hex_len = 2 + (len * 2) + 1;  // \x + hex + null
    char *hex = malloc(hex_len);
    if (!hex) return NULL;

    hex[0] = '\\';
    hex[1] = 'x';

    static const char hex_chars[] = "0123456789abcdef";
    for (size_t i = 0; i < len; i++) {
        hex[2 + i*2] = hex_chars[(data[i] >> 4) & 0x0F];
        hex[2 + i*2 + 1] = hex_chars[data[i] & 0x0F];
    }
    hex[hex_len - 1] = '\0';

    return hex;
}

// ============================================================================
// Bind Functions
// ============================================================================

int my_sqlite3_bind_int(sqlite3_stmt *pStmt, int idx, int val) {
    int rc = orig_sqlite3_bind_int ? orig_sqlite3_bind_int(pStmt, idx, val) : SQLITE_ERROR;

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS) {
        pthread_mutex_lock(&pg_stmt->mutex);  // CRITICAL FIX: Protect bind operations
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            // Use pre-allocated buffer instead of strdup
            snprintf(pg_stmt->param_buffers[pg_idx], 32, "%d", val);
            pg_stmt->param_values[pg_idx] = pg_stmt->param_buffers[pg_idx];
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
    }

    return rc;
}

int my_sqlite3_bind_int64(sqlite3_stmt *pStmt, int idx, sqlite3_int64 val) {
    int rc = orig_sqlite3_bind_int64 ? orig_sqlite3_bind_int64(pStmt, idx, val) : SQLITE_ERROR;

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS) {
        pthread_mutex_lock(&pg_stmt->mutex);  // CRITICAL FIX: Protect bind operations
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            // Use pre-allocated buffer instead of strdup
            snprintf(pg_stmt->param_buffers[pg_idx], 32, "%lld", val);
            pg_stmt->param_values[pg_idx] = pg_stmt->param_buffers[pg_idx];
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
    }

    return rc;
}

int my_sqlite3_bind_double(sqlite3_stmt *pStmt, int idx, double val) {
    int rc = orig_sqlite3_bind_double ? orig_sqlite3_bind_double(pStmt, idx, val) : SQLITE_ERROR;

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS) {
        pthread_mutex_lock(&pg_stmt->mutex);  // CRITICAL FIX: Protect bind operations
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            // Use pre-allocated buffer instead of strdup
            snprintf(pg_stmt->param_buffers[pg_idx], 32, "%.17g", val);
            pg_stmt->param_values[pg_idx] = pg_stmt->param_buffers[pg_idx];
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
    }

    return rc;
}

int my_sqlite3_bind_text(sqlite3_stmt *pStmt, int idx, const char *val,
                         int nBytes, void (*destructor)(void*)) {
    int rc = orig_sqlite3_bind_text ? orig_sqlite3_bind_text(pStmt, idx, val, nBytes, destructor) : SQLITE_ERROR;

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS && val) {
        pthread_mutex_lock(&pg_stmt->mutex);  // CRITICAL FIX: Protect bind operations
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);

        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            // Free old value only if it was dynamically allocated
            if (pg_stmt->param_values[pg_idx] && !is_preallocated_buffer(pg_stmt, pg_idx)) {
                free(pg_stmt->param_values[pg_idx]);
                pg_stmt->param_values[pg_idx] = NULL;  // Prevent dangling pointer
            }

            size_t actual_len = (nBytes < 0) ? strlen(val) : (size_t)nBytes;

            // Check if data contains binary bytes (non-UTF8)
            // If so, convert to PostgreSQL hex format for BYTEA columns
            if (contains_binary_bytes((const unsigned char*)val, actual_len)) {
                LOG_DEBUG("bind_text: detected binary data at idx=%d, len=%zu, converting to hex", idx, actual_len);
                pg_stmt->param_values[pg_idx] = bytes_to_pg_hex((const unsigned char*)val, actual_len);
            } else if (nBytes < 0) {
                pg_stmt->param_values[pg_idx] = strdup(val);
            } else {
                pg_stmt->param_values[pg_idx] = malloc(nBytes + 1);
                if (pg_stmt->param_values[pg_idx]) {
                    memcpy(pg_stmt->param_values[pg_idx], val, nBytes);
                    pg_stmt->param_values[pg_idx][nBytes] = '\0';
                }
            }
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
    }

    return rc;
}

int my_sqlite3_bind_blob(sqlite3_stmt *pStmt, int idx, const void *val,
                         int nBytes, void (*destructor)(void*)) {
    int rc = orig_sqlite3_bind_blob ? orig_sqlite3_bind_blob(pStmt, idx, val, nBytes, destructor) : SQLITE_ERROR;

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS && val && nBytes > 0) {
        pthread_mutex_lock(&pg_stmt->mutex);  // CRITICAL FIX: Protect bind operations
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            if (pg_stmt->param_values[pg_idx] && !is_preallocated_buffer(pg_stmt, pg_idx)) {
                free(pg_stmt->param_values[pg_idx]);
                pg_stmt->param_values[pg_idx] = NULL;  // Prevent dangling pointer
            }
            // Convert binary data to PostgreSQL hex format for BYTEA columns
            // This works in text mode (paramFormats=NULL means text mode)
            LOG_DEBUG("bind_blob: converting %d bytes to hex at idx=%d", nBytes, idx);
            pg_stmt->param_values[pg_idx] = bytes_to_pg_hex((const unsigned char*)val, (size_t)nBytes);
            pg_stmt->param_lengths[pg_idx] = 0;  // Use strlen for text mode
            pg_stmt->param_formats[pg_idx] = 0;  // text mode (hex string)
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
    }

    return rc;
}

// sqlite3_bind_blob64 - 64-bit version for large blobs
int my_sqlite3_bind_blob64(sqlite3_stmt *pStmt, int idx, const void *val,
                           sqlite3_uint64 nBytes, void (*destructor)(void*)) {
    int rc = orig_sqlite3_bind_blob64 ? orig_sqlite3_bind_blob64(pStmt, idx, val, nBytes, destructor) : SQLITE_ERROR;

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS && val && nBytes > 0) {
        pthread_mutex_lock(&pg_stmt->mutex);  // CRITICAL FIX: Protect bind operations
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            if (pg_stmt->param_values[pg_idx] && !is_preallocated_buffer(pg_stmt, pg_idx)) {
                free(pg_stmt->param_values[pg_idx]);
                pg_stmt->param_values[pg_idx] = NULL;  // Prevent dangling pointer
            }
            // Convert binary data to PostgreSQL hex format for BYTEA columns
            LOG_DEBUG("bind_blob64: converting %llu bytes to hex at idx=%d", (unsigned long long)nBytes, idx);
            pg_stmt->param_values[pg_idx] = bytes_to_pg_hex((const unsigned char*)val, (size_t)nBytes);
            pg_stmt->param_lengths[pg_idx] = 0;  // Use strlen for text mode
            pg_stmt->param_formats[pg_idx] = 0;  // text mode (hex string)
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
    }

    return rc;
}

// sqlite3_bind_text64 - critical for Plex which uses this for text values!
int my_sqlite3_bind_text64(sqlite3_stmt *pStmt, int idx, const char *val,
                           sqlite3_uint64 nBytes, void (*destructor)(void*),
                           unsigned char encoding) {
    int rc = orig_sqlite3_bind_text64 ? orig_sqlite3_bind_text64(pStmt, idx, val, nBytes, destructor, encoding) : SQLITE_ERROR;

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS && val) {
        pthread_mutex_lock(&pg_stmt->mutex);  // CRITICAL FIX: Protect bind operations
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            if (pg_stmt->param_values[pg_idx] && !is_preallocated_buffer(pg_stmt, pg_idx)) {
                free(pg_stmt->param_values[pg_idx]);
                pg_stmt->param_values[pg_idx] = NULL;  // Prevent dangling pointer
            }

            size_t actual_len = (nBytes == (sqlite3_uint64)-1) ? strlen(val) : (size_t)nBytes;

            // Check if data contains binary bytes (non-UTF8)
            // If so, convert to PostgreSQL hex format for BYTEA columns
            if (contains_binary_bytes((const unsigned char*)val, actual_len)) {
                LOG_DEBUG("bind_text64: detected binary data at idx=%d, len=%zu, converting to hex", idx, actual_len);
                pg_stmt->param_values[pg_idx] = bytes_to_pg_hex((const unsigned char*)val, actual_len);
            } else if (nBytes == (sqlite3_uint64)-1) {
                pg_stmt->param_values[pg_idx] = strdup(val);
            } else {
                pg_stmt->param_values[pg_idx] = malloc((size_t)nBytes + 1);
                if (pg_stmt->param_values[pg_idx]) {
                    memcpy(pg_stmt->param_values[pg_idx], val, (size_t)nBytes);
                    pg_stmt->param_values[pg_idx][(size_t)nBytes] = '\0';
                }
            }
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
    }

    return rc;
}

// sqlite3_bind_value - copies value from another sqlite3_value
int my_sqlite3_bind_value(sqlite3_stmt *pStmt, int idx, const sqlite3_value *pValue) {
    int rc = orig_sqlite3_bind_value ? orig_sqlite3_bind_value(pStmt, idx, pValue) : SQLITE_ERROR;

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS && pValue) {
        pthread_mutex_lock(&pg_stmt->mutex);  // CRITICAL FIX: Protect bind operations
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            // Get value type and extract appropriately
            int vtype = sqlite3_value_type(pValue);
            if (pg_stmt->param_values[pg_idx] && !is_preallocated_buffer(pg_stmt, pg_idx)) {
                free(pg_stmt->param_values[pg_idx]);
                pg_stmt->param_values[pg_idx] = NULL;
            }

            switch (vtype) {
                case SQLITE_INTEGER: {
                    sqlite3_int64 v = sqlite3_value_int64(pValue);
                    char buf[32];
                    snprintf(buf, sizeof(buf), "%lld", v);
                    pg_stmt->param_values[pg_idx] = strdup(buf);
                    break;
                }
                case SQLITE_FLOAT: {
                    double v = sqlite3_value_double(pValue);
                    char buf[64];
                    snprintf(buf, sizeof(buf), "%.17g", v);
                    pg_stmt->param_values[pg_idx] = strdup(buf);
                    break;
                }
                case SQLITE_TEXT: {
                    const char *v = (const char *)sqlite3_value_text(pValue);
                    if (v) pg_stmt->param_values[pg_idx] = strdup(v);
                    break;
                }
                case SQLITE_BLOB: {
                    int len = sqlite3_value_bytes(pValue);
                    const void *v = sqlite3_value_blob(pValue);
                    if (v && len > 0) {
                        pg_stmt->param_values[pg_idx] = malloc(len);
                        if (pg_stmt->param_values[pg_idx]) {
                            memcpy(pg_stmt->param_values[pg_idx], v, len);
                        }
                        pg_stmt->param_lengths[pg_idx] = len;
                        pg_stmt->param_formats[pg_idx] = 1;  // binary
                    }
                    break;
                }
                case SQLITE_NULL:
                default:
                    // Leave as NULL
                    break;
            }
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
    }

    return rc;
}

int my_sqlite3_bind_null(sqlite3_stmt *pStmt, int idx) {
    int rc = orig_sqlite3_bind_null ? orig_sqlite3_bind_null(pStmt, idx) : SQLITE_ERROR;

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS) {
        pthread_mutex_lock(&pg_stmt->mutex);  // CRITICAL FIX: Protect bind operations
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            if (pg_stmt->param_values[pg_idx] && !is_preallocated_buffer(pg_stmt, pg_idx)) {
                free(pg_stmt->param_values[pg_idx]);
                pg_stmt->param_values[pg_idx] = NULL;
            }
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
    }

    return rc;
}
