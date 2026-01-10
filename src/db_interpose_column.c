/*
 * Plex PostgreSQL Interposing Shim - Column & Value Access
 *
 * Handles sqlite3_column_* and sqlite3_value_* function interposition.
 * These functions read data from PostgreSQL result sets.
 */

#include "db_interpose.h"
#include "pg_query_cache.h"
#include <stdatomic.h>

// ============================================================================
// Helper: Decode PostgreSQL hex-encoded BYTEA to binary
// ============================================================================

// PostgreSQL BYTEA hex format: \x followed by hex digits (2 per byte)
// Returns decoded data and sets out_length. Caller must NOT free the result.
const void* pg_decode_bytea(pg_stmt_t *pg_stmt, int row, int col, int *out_length) {
    const char *hex_str = PQgetvalue(pg_stmt->result, row, col);
    if (!hex_str) {
        *out_length = 0;
        return NULL;
    }

    // Check for hex format: starts with \x
    if (hex_str[0] != '\\' || hex_str[1] != 'x') {
        // Not hex format, return raw data (escape format or other)
        *out_length = PQgetlength(pg_stmt->result, row, col);
        return hex_str;
    }

    // Skip \x prefix
    hex_str += 2;
    size_t hex_len = strlen(hex_str);
    size_t bin_len = hex_len / 2;

    // Check if we already have this row cached
    if (pg_stmt->decoded_blob_row == row && pg_stmt->decoded_blobs[col]) {
        *out_length = pg_stmt->decoded_blob_lens[col];
        return pg_stmt->decoded_blobs[col];
    }

    // Clear old cache if row changed
    if (pg_stmt->decoded_blob_row != row) {
        for (int i = 0; i < MAX_PARAMS; i++) {
            if (pg_stmt->decoded_blobs[i]) {
                free(pg_stmt->decoded_blobs[i]);
                pg_stmt->decoded_blobs[i] = NULL;
                pg_stmt->decoded_blob_lens[i] = 0;
            }
        }
        pg_stmt->decoded_blob_row = row;
    }

    // Allocate and decode
    unsigned char *binary = malloc(bin_len + 1);  // +1 for safety
    if (!binary) {
        *out_length = 0;
        return NULL;
    }

    // Inline hex decode - 4-10x faster than sscanf
    // Lookup table for hex digit values (255 = invalid)
    static const unsigned char hex_lut[256] = {
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        0,1,2,3,4,5,6,7,8,9,255,255,255,255,255,255,  // 0-9
        255,10,11,12,13,14,15,255,255,255,255,255,255,255,255,255,  // A-F
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        255,10,11,12,13,14,15,255,255,255,255,255,255,255,255,255,  // a-f
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255
    };

    for (size_t i = 0; i < bin_len; i++) {
        unsigned char hi = hex_lut[(unsigned char)hex_str[i * 2]];
        unsigned char lo = hex_lut[(unsigned char)hex_str[i * 2 + 1]];
        if (hi == 255 || lo == 255) {
            free(binary);
            *out_length = 0;
            return NULL;
        }
        binary[i] = (hi << 4) | lo;
    }

    // Cache the decoded data
    pg_stmt->decoded_blobs[col] = binary;
    pg_stmt->decoded_blob_lens[col] = (int)bin_len;
    *out_length = (int)bin_len;

    return binary;
}

// ============================================================================
// Helper: Execute query on-demand for column metadata access
// ============================================================================
// SQLite allows column_count/column_name to be called before step().
// PostgreSQL requires executing the query to get column metadata.
// This helper executes the query if not yet executed.
static int ensure_pg_result_for_metadata(pg_stmt_t *pg_stmt) {
    // Must be called with pg_stmt->mutex held
    if (pg_stmt->result || pg_stmt->cached_result) {
        return 1;  // Already have result
    }
    if (!pg_stmt->pg_sql || !pg_stmt->conn || !pg_stmt->conn->conn) {
        return 0;  // Can't execute - missing query or connection
    }

    // Get the connection to use (thread-local for library DB)
    pg_connection_t *exec_conn = pg_stmt->conn;
    if (is_library_db_path(pg_stmt->conn->db_path)) {
        pg_connection_t *thread_conn = pg_get_thread_connection(pg_stmt->conn->db_path);
        if (thread_conn && thread_conn->is_pg_active && thread_conn->conn) {
            exec_conn = thread_conn;
        }
    }

    LOG_INFO("METADATA_EXEC: Executing query for column metadata access: %.100s", pg_stmt->pg_sql);

    // Lock the connection mutex
    pthread_mutex_lock(&exec_conn->mutex);

    // Drain any pending results
    PQsetnonblocking(exec_conn->conn, 0);
    while (PQisBusy(exec_conn->conn)) {
        PQconsumeInput(exec_conn->conn);
    }
    PGresult *pending;
    while ((pending = PQgetResult(exec_conn->conn)) != NULL) {
        PQclear(pending);
    }

    // Build parameter values array
    const char *paramValues[MAX_PARAMS] = {NULL};
    for (int i = 0; i < pg_stmt->param_count && i < MAX_PARAMS; i++) {
        paramValues[i] = pg_stmt->param_values[i];
    }

    // Execute the query
    pg_stmt->result = PQexecParams(exec_conn->conn, pg_stmt->pg_sql,
                                    pg_stmt->param_count, NULL,
                                    paramValues, NULL, NULL, 0);
    pthread_mutex_unlock(&exec_conn->mutex);

    if (PQresultStatus(pg_stmt->result) == PGRES_TUPLES_OK) {
        pg_stmt->num_rows = PQntuples(pg_stmt->result);
        pg_stmt->num_cols = PQnfields(pg_stmt->result);
        pg_stmt->current_row = -1;  // Will be 0 after first step()
        pg_stmt->result_conn = exec_conn;
        LOG_INFO("METADATA_EXEC: Success - %d cols, %d rows", pg_stmt->num_cols, pg_stmt->num_rows);
        return 1;
    } else {
        LOG_ERROR("METADATA_EXEC: Query failed: %s", PQerrorMessage(exec_conn->conn));
        PQclear(pg_stmt->result);
        pg_stmt->result = NULL;
        return 0;
    }
}

// ============================================================================
// Column Functions
// ============================================================================

int my_sqlite3_column_count(sqlite3_stmt *pStmt) {
    LOG_DEBUG("COLUMN_COUNT: stmt=%p", (void*)pStmt);
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);
        // QUERY CACHE: Check for cached result first
        if (pg_stmt->cached_result) {
            int count = pg_stmt->cached_result->num_cols;
            pthread_mutex_unlock(&pg_stmt->mutex);
            return count;
        }
        // If num_cols is 0 and we have a query but no result yet,
        // execute the query to get column metadata (SQLite allows this before step)
        if (pg_stmt->num_cols == 0 && pg_stmt->pg_sql && !pg_stmt->result) {
            ensure_pg_result_for_metadata(pg_stmt);
        }
        // For PostgreSQL statements, return our stored num_cols
        // Don't fall through to orig_sqlite3_column_count which would fail
        // because pStmt is not a valid SQLite statement
        int count = pg_stmt->num_cols;
        pthread_mutex_unlock(&pg_stmt->mutex);
        return count;
    }
    return orig_sqlite3_column_count ? orig_sqlite3_column_count(pStmt) : 0;
}

int my_sqlite3_column_type(sqlite3_stmt *pStmt, int idx) {
    LOG_DEBUG("COLUMN_TYPE: stmt=%p idx=%d", (void*)pStmt, idx);
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);

        // QUERY CACHE: Check for cached result first
        if (pg_stmt->cached_result) {
            cached_result_t *cached = pg_stmt->cached_result;
            int row = pg_stmt->current_row;
            if (idx >= 0 && idx < cached->num_cols && row >= 0 && row < cached->num_rows) {
                cached_row_t *crow = &cached->rows[row];
                if (crow->is_null[idx]) {
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return SQLITE_NULL;
                }
                // Use cached column type OID to determine SQLite type
                int result = pg_oid_to_sqlite_type(cached->col_types[idx]);
                pthread_mutex_unlock(&pg_stmt->mutex);
                return result;
            }
            pthread_mutex_unlock(&pg_stmt->mutex);
            return SQLITE_NULL;
        }

        if (!pg_stmt->result) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return SQLITE_NULL;
        }
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            LOG_ERROR("COL_TYPE_BOUNDS: idx=%d out of bounds (num_cols=%d) sql=%.100s",
                     idx, pg_stmt->num_cols, pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
            pthread_mutex_unlock(&pg_stmt->mutex);
            return SQLITE_NULL;
        }
        int row = pg_stmt->current_row;
        if (row < 0 || row >= pg_stmt->num_rows) {
            LOG_ERROR("COL_TYPE_ROW_BOUNDS: row=%d out of bounds (num_rows=%d)", row, pg_stmt->num_rows);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return SQLITE_NULL;
        }
        int is_null = PQgetisnull(pg_stmt->result, row, idx);
        int result = is_null ? SQLITE_NULL : pg_oid_to_sqlite_type(PQftype(pg_stmt->result, idx));
        pthread_mutex_unlock(&pg_stmt->mutex);
        return result;
    }
    return orig_sqlite3_column_type ? orig_sqlite3_column_type(pStmt, idx) : SQLITE_NULL;
}

int my_sqlite3_column_int(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);

        // QUERY CACHE: Check for cached result first
        if (pg_stmt->cached_result) {
            cached_result_t *cached = pg_stmt->cached_result;
            int row = pg_stmt->current_row;
            if (idx >= 0 && idx < cached->num_cols && row >= 0 && row < cached->num_rows) {
                cached_row_t *crow = &cached->rows[row];
                if (!crow->is_null[idx] && crow->values[idx]) {
                    const char *val = crow->values[idx];
                    int result_val = 0;
                    if (val[0] == 't' && val[1] == '\0') result_val = 1;
                    else if (val[0] == 'f' && val[1] == '\0') result_val = 0;
                    else result_val = atoi(val);
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return result_val;
                }
            }
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }

        if (!pg_stmt->result) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            LOG_ERROR("COL_INT_BOUNDS: idx=%d out of bounds (num_cols=%d)", idx, pg_stmt->num_cols);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }
        int row = pg_stmt->current_row;
        if (row < 0 || row >= pg_stmt->num_rows) {
            LOG_ERROR("COL_INT_ROW_BOUNDS: row=%d out of bounds (num_rows=%d)", row, pg_stmt->num_rows);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }

        int result_val = 0;
        if (!PQgetisnull(pg_stmt->result, row, idx)) {
            const char *val = PQgetvalue(pg_stmt->result, row, idx);
            if (val[0] == 't' && val[1] == '\0') result_val = 1;
            else if (val[0] == 'f' && val[1] == '\0') result_val = 0;
            else result_val = atoi(val);
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
        return result_val;
    }
    return orig_sqlite3_column_int ? orig_sqlite3_column_int(pStmt, idx) : 0;
}

sqlite3_int64 my_sqlite3_column_int64(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);

        // QUERY CACHE: Check for cached result first
        if (pg_stmt->cached_result) {
            cached_result_t *cached = pg_stmt->cached_result;
            int row = pg_stmt->current_row;
            if (idx >= 0 && idx < cached->num_cols && row >= 0 && row < cached->num_rows) {
                cached_row_t *crow = &cached->rows[row];
                if (!crow->is_null[idx] && crow->values[idx]) {
                    const char *val = crow->values[idx];
                    sqlite3_int64 result_val = 0;
                    if (val[0] == 't' && val[1] == '\0') result_val = 1;
                    else if (val[0] == 'f' && val[1] == '\0') result_val = 0;
                    else result_val = atoll(val);
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return result_val;
                }
            }
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }

        if (!pg_stmt->result) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            LOG_ERROR("COL_INT64_BOUNDS: idx=%d out of bounds (num_cols=%d)", idx, pg_stmt->num_cols);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }
        int row = pg_stmt->current_row;
        if (row < 0 || row >= pg_stmt->num_rows) {
            LOG_ERROR("COL_INT64_ROW_BOUNDS: row=%d out of bounds (num_rows=%d)", row, pg_stmt->num_rows);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }

        sqlite3_int64 result_val = 0;
        if (!PQgetisnull(pg_stmt->result, row, idx)) {
            const char *val = PQgetvalue(pg_stmt->result, row, idx);
            if (val[0] == 't' && val[1] == '\0') result_val = 1;
            else if (val[0] == 'f' && val[1] == '\0') result_val = 0;
            else result_val = atoll(val);
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
        return result_val;
    }
    return orig_sqlite3_column_int64 ? orig_sqlite3_column_int64(pStmt, idx) : 0;
}

double my_sqlite3_column_double(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);

        // QUERY CACHE: Check for cached result first
        if (pg_stmt->cached_result) {
            cached_result_t *cached = pg_stmt->cached_result;
            int row = pg_stmt->current_row;
            if (idx >= 0 && idx < cached->num_cols && row >= 0 && row < cached->num_rows) {
                cached_row_t *crow = &cached->rows[row];
                if (!crow->is_null[idx] && crow->values[idx]) {
                    const char *val = crow->values[idx];
                    double result_val = 0.0;
                    if (val[0] == 't' && val[1] == '\0') result_val = 1.0;
                    else if (val[0] == 'f' && val[1] == '\0') result_val = 0.0;
                    else result_val = atof(val);
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return result_val;
                }
            }
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0.0;
        }

        if (!pg_stmt->result) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0.0;
        }
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0.0;
        }
        int row = pg_stmt->current_row;
        if (row < 0 || row >= pg_stmt->num_rows) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0.0;
        }
        double result_val = 0.0;
        if (!PQgetisnull(pg_stmt->result, row, idx)) {
            const char *val = PQgetvalue(pg_stmt->result, row, idx);
            if (val[0] == 't' && val[1] == '\0') result_val = 1.0;
            else if (val[0] == 'f' && val[1] == '\0') result_val = 0.0;
            else result_val = atof(val);
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
        return result_val;
    }
    return orig_sqlite3_column_double ? orig_sqlite3_column_double(pStmt, idx) : 0.0;
}

// Static buffers for column_text - LARGE pool to prevent race condition wrap-around
// 256 buffers x 16KB = 4MB total - allows 256 concurrent column_text calls before wrap
// CRITICAL: Small pool (8 buffers) caused SIGILL crashes due to buffer overwrite race
static char column_text_buffers[256][16384];
static atomic_int column_text_idx = 0;  // Atomic for thread-safe increment

const unsigned char* my_sqlite3_column_text(sqlite3_stmt *pStmt, int idx) {
    LOG_DEBUG("COLUMN_TEXT: stmt=%p idx=%d", (void*)pStmt, idx);
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    LOG_DEBUG("COLUMN_TEXT: pg_stmt=%p is_pg=%d", (void*)pg_stmt, pg_stmt ? pg_stmt->is_pg : -1);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);
        LOG_DEBUG("COLUMN_TEXT: locked mutex, result=%p row=%d cols=%d",
                 (void*)pg_stmt->result, pg_stmt->current_row, pg_stmt->num_cols);

        const char *source_value = NULL;

        // QUERY CACHE: Check for cached result first
        if (pg_stmt->cached_result) {
            cached_result_t *cached = pg_stmt->cached_result;
            int row = pg_stmt->current_row;
            LOG_DEBUG("COLUMN_TEXT_CACHE: idx=%d row=%d num_cols=%d num_rows=%d",
                     idx, row, cached->num_cols, cached->num_rows);
            if (idx >= 0 && idx < cached->num_cols && row >= 0 && row < cached->num_rows) {
                cached_row_t *crow = &cached->rows[row];
                if (!crow->is_null[idx] && crow->values[idx]) {
                    source_value = crow->values[idx];
                    LOG_DEBUG("COLUMN_TEXT_CACHE_HIT: found cached value len=%zu", strlen(source_value));
                }
            }
            if (!source_value) {
                LOG_DEBUG("COLUMN_TEXT_CACHE_NULL: returning NULL (SQLite behavior)");
                pthread_mutex_unlock(&pg_stmt->mutex);
                return NULL;  // SQLite returns NULL for NULL columns
            }
        } else {
            // Non-cached path - get from PGresult
            if (!pg_stmt->result) {
                LOG_DEBUG("COLUMN_TEXT: no result, returning empty buffer");
                int buf = atomic_fetch_add(&column_text_idx, 1) & 0xFF;
                column_text_buffers[buf][0] = '\0';
                pthread_mutex_unlock(&pg_stmt->mutex);
                return (const unsigned char*)column_text_buffers[buf];
            }

            if (idx < 0 || idx >= pg_stmt->num_cols) {
                LOG_DEBUG("COLUMN_TEXT: idx=%d out of bounds (num_cols=%d)", idx, pg_stmt->num_cols);
                int buf = atomic_fetch_add(&column_text_idx, 1) & 0xFF;
                column_text_buffers[buf][0] = '\0';
                pthread_mutex_unlock(&pg_stmt->mutex);
                return (const unsigned char*)column_text_buffers[buf];
            }

            int row = pg_stmt->current_row;
            if (row < 0 || row >= pg_stmt->num_rows) {
                LOG_DEBUG("COLUMN_TEXT: row=%d out of bounds (num_rows=%d)", row, pg_stmt->num_rows);
                int buf = atomic_fetch_add(&column_text_idx, 1) & 0xFF;
                column_text_buffers[buf][0] = '\0';
                pthread_mutex_unlock(&pg_stmt->mutex);
                return (const unsigned char*)column_text_buffers[buf];
            }

            if (PQgetisnull(pg_stmt->result, row, idx)) {
                LOG_DEBUG("COLUMN_TEXT: value is NULL, returning NULL (SQLite behavior)");
                pthread_mutex_unlock(&pg_stmt->mutex);
                return NULL;  // SQLite returns NULL for NULL columns
            }

            source_value = PQgetvalue(pg_stmt->result, row, idx);
            if (!source_value) {
                LOG_DEBUG("COLUMN_TEXT: PQgetvalue returned NULL, returning empty buffer");
                int buf = atomic_fetch_add(&column_text_idx, 1) & 0xFF;
                column_text_buffers[buf][0] = '\0';
                pthread_mutex_unlock(&pg_stmt->mutex);
                return (const unsigned char*)column_text_buffers[buf];
            }
        }

        // CRITICAL: Always copy to static buffer to prevent use-after-free
        // Both cache and PGresult can be freed while caller still uses the pointer
        size_t len = strlen(source_value);
        if (len >= 16383) len = 16383;

        // Thread-safe buffer allocation using atomic increment
        // Use bitmask for fast modulo (256 = 0xFF + 1)
        int buf = atomic_fetch_add(&column_text_idx, 1) & 0xFF;
        memcpy(column_text_buffers[buf], source_value, len);
        column_text_buffers[buf][len] = '\0';

        LOG_DEBUG("COLUMN_TEXT: copied to buffer[%d] len=%zu", buf, len);
        pthread_mutex_unlock(&pg_stmt->mutex);

        // TEST: Return the actual buffer content
        // If crashes: issue is with buffer content
        // If works: buffer content is fine
        return (const unsigned char*)column_text_buffers[buf];
    }
    LOG_DEBUG("COLUMN_TEXT: falling through to orig");
    return orig_sqlite3_column_text ? orig_sqlite3_column_text(pStmt, idx) : NULL;
}

const void* my_sqlite3_column_blob(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);

        // QUERY CACHE: Check for cached result first
        if (pg_stmt->cached_result) {
            cached_result_t *cached = pg_stmt->cached_result;
            int row = pg_stmt->current_row;
            if (idx >= 0 && idx < cached->num_cols && row >= 0 && row < cached->num_rows) {
                cached_row_t *crow = &cached->rows[row];
                if (!crow->is_null[idx] && crow->values[idx]) {
                    // Return cached blob data directly
                    // Note: For BYTEA, the cached value is already decoded
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return crow->values[idx];
                }
            }
            pthread_mutex_unlock(&pg_stmt->mutex);
            return NULL;
        }

        if (!pg_stmt->result) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return NULL;
        }

        if (idx < 0 || idx >= pg_stmt->num_cols || idx >= MAX_PARAMS) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return NULL;
        }
        int row = pg_stmt->current_row;
        if (row < 0 || row >= pg_stmt->num_rows) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return NULL;
        }
        if (!PQgetisnull(pg_stmt->result, row, idx)) {
            // Check if this is a BYTEA column (OID 17)
            Oid col_type = PQftype(pg_stmt->result, idx);
            const char *col_name = PQfname(pg_stmt->result, idx);
            LOG_DEBUG("column_blob called: col=%d name=%s type=%d row=%d", idx, col_name ? col_name : "?", col_type, row);
            if (col_type == 17) {  // BYTEA
                int blob_len;
                const void *result = pg_decode_bytea(pg_stmt, row, idx, &blob_len);
                pthread_mutex_unlock(&pg_stmt->mutex);
                return result;
            }

            // For non-BYTEA, cache the raw blob data to ensure pointer validity
            // Check if we already have this value cached for the current row
            if (pg_stmt->cached_row == row && pg_stmt->cached_blob[idx]) {
                const void *result = pg_stmt->cached_blob[idx];
                pthread_mutex_unlock(&pg_stmt->mutex);
                return result;
            }

            // Clear cache if row changed
            if (pg_stmt->cached_row != row) {
                for (int i = 0; i < MAX_PARAMS; i++) {
                    if (pg_stmt->cached_text[i]) {
                        free(pg_stmt->cached_text[i]);
                        pg_stmt->cached_text[i] = NULL;
                    }
                    if (pg_stmt->cached_blob[i]) {
                        free(pg_stmt->cached_blob[i]);
                        pg_stmt->cached_blob[i] = NULL;
                        pg_stmt->cached_blob_len[i] = 0;
                    }
                }
                pg_stmt->cached_row = row;
            }

            // Cache the blob data
            int blob_len = PQgetlength(pg_stmt->result, row, idx);
            const char *pg_value = PQgetvalue(pg_stmt->result, row, idx);
            if (pg_value && blob_len > 0) {
                pg_stmt->cached_blob[idx] = malloc(blob_len);
                if (pg_stmt->cached_blob[idx]) {
                    memcpy(pg_stmt->cached_blob[idx], pg_value, blob_len);
                    pg_stmt->cached_blob_len[idx] = blob_len;
                } else {
                    LOG_ERROR("COL_BLOB: malloc failed for column %d, len %d", idx, blob_len);
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return NULL;
                }
            }
            const void *result = pg_stmt->cached_blob[idx];
            pthread_mutex_unlock(&pg_stmt->mutex);
            return result;
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
        return NULL;
    }
    return orig_sqlite3_column_blob ? orig_sqlite3_column_blob(pStmt, idx) : NULL;
}

int my_sqlite3_column_bytes(sqlite3_stmt *pStmt, int idx) {
    LOG_DEBUG("COLUMN_BYTES: stmt=%p idx=%d", (void*)pStmt, idx);
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);

        // QUERY CACHE: Check for cached result first
        if (pg_stmt->cached_result) {
            cached_result_t *cached = pg_stmt->cached_result;
            int row = pg_stmt->current_row;
            if (idx >= 0 && idx < cached->num_cols && row >= 0 && row < cached->num_rows) {
                cached_row_t *crow = &cached->rows[row];
                if (!crow->is_null[idx]) {
                    int len = crow->lengths[idx];
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return len;
                }
            }
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }

        if (!pg_stmt->result) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }

        if (idx < 0 || idx >= pg_stmt->num_cols) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }
        int row = pg_stmt->current_row;
        if (row < 0 || row >= pg_stmt->num_rows) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }
        if (!PQgetisnull(pg_stmt->result, row, idx)) {
            // Check if this is a BYTEA column (OID 17)
            Oid col_type = PQftype(pg_stmt->result, idx);
            if (col_type == 17) {  // BYTEA
                // Decode the blob (caches it) and return the decoded length
                int blob_len;
                pg_decode_bytea(pg_stmt, row, idx, &blob_len);
                pthread_mutex_unlock(&pg_stmt->mutex);
                return blob_len;
            }
            int len = PQgetlength(pg_stmt->result, row, idx);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return len;
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
        return 0;
    }
    return orig_sqlite3_column_bytes ? orig_sqlite3_column_bytes(pStmt, idx) : 0;
}

const char* my_sqlite3_column_name(sqlite3_stmt *pStmt, int idx) {
    LOG_DEBUG("COLUMN_NAME: stmt=%p idx=%d", (void*)pStmt, idx);
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);
        // If no result yet but we have a query, execute it to get column metadata
        // SQLite allows column_name to be called before step()
        if (!pg_stmt->result && !pg_stmt->cached_result && pg_stmt->pg_sql) {
            if (!ensure_pg_result_for_metadata(pg_stmt)) {
                LOG_DEBUG("COLUMN_NAME: failed to execute query for metadata");
                pthread_mutex_unlock(&pg_stmt->mutex);
                return orig_sqlite3_column_name ? orig_sqlite3_column_name(pStmt, idx) : NULL;
            }
        }
        if (!pg_stmt->result) {
            LOG_DEBUG("COLUMN_NAME: pg_stmt has no result, falling back to orig");
            pthread_mutex_unlock(&pg_stmt->mutex);
            return orig_sqlite3_column_name ? orig_sqlite3_column_name(pStmt, idx) : NULL;
        }
        if (idx >= 0 && idx < pg_stmt->num_cols) {
            const char *name = PQfname(pg_stmt->result, idx);
            LOG_DEBUG("COLUMN_NAME: returning '%s' for idx=%d", name ? name : "NULL", idx);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return name;
        }
        LOG_DEBUG("COLUMN_NAME: idx out of bounds (num_cols=%d)", pg_stmt->num_cols);
        pthread_mutex_unlock(&pg_stmt->mutex);
    } else {
        LOG_DEBUG("COLUMN_NAME: not a PG stmt (pg_stmt=%p is_pg=%d), using orig",
                 (void*)pg_stmt, pg_stmt ? pg_stmt->is_pg : -1);
    }
    const char *orig_name = orig_sqlite3_column_name ? orig_sqlite3_column_name(pStmt, idx) : NULL;
    LOG_DEBUG("COLUMN_NAME: orig returned '%s'", orig_name ? orig_name : "NULL");
    return orig_name;
}

// sqlite3_column_decltype returns the declared type of a column from CREATE TABLE.
// For PostgreSQL, we convert the OID to a SQLite-compatible type string.
// This function can be called before step(), so we may need to execute the query first.
const char* my_sqlite3_column_decltype(sqlite3_stmt *pStmt, int idx) {
    LOG_DEBUG("COLUMN_DECLTYPE: stmt=%p idx=%d", (void*)pStmt, idx);
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);
        // If no result yet but we have a query, execute it to get column metadata
        // SQLite allows column_decltype to be called before step()
        if (!pg_stmt->result && !pg_stmt->cached_result && pg_stmt->pg_sql) {
            if (!ensure_pg_result_for_metadata(pg_stmt)) {
                LOG_DEBUG("COLUMN_DECLTYPE: failed to execute query for metadata");
                pthread_mutex_unlock(&pg_stmt->mutex);
                return orig_sqlite3_column_decltype ? orig_sqlite3_column_decltype(pStmt, idx) : NULL;
            }
        }
        if (!pg_stmt->result) {
            LOG_DEBUG("COLUMN_DECLTYPE: pg_stmt has no result, falling back to orig");
            pthread_mutex_unlock(&pg_stmt->mutex);
            return orig_sqlite3_column_decltype ? orig_sqlite3_column_decltype(pStmt, idx) : NULL;
        }
        if (idx >= 0 && idx < pg_stmt->num_cols) {
            Oid pg_type = PQftype(pg_stmt->result, idx);
            const char *decltype = pg_oid_to_sqlite_decltype(pg_type);
            LOG_DEBUG("COLUMN_DECLTYPE: returning '%s' for idx=%d (oid=%d)",
                     decltype ? decltype : "NULL", idx, (int)pg_type);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return decltype;
        }
        LOG_DEBUG("COLUMN_DECLTYPE: idx out of bounds (num_cols=%d)", pg_stmt->num_cols);
        pthread_mutex_unlock(&pg_stmt->mutex);
    } else {
        LOG_DEBUG("COLUMN_DECLTYPE: not a PG stmt (pg_stmt=%p is_pg=%d), using orig",
                 (void*)pg_stmt, pg_stmt ? pg_stmt->is_pg : -1);
    }
    const char *orig_type = orig_sqlite3_column_decltype ? orig_sqlite3_column_decltype(pStmt, idx) : NULL;
    LOG_DEBUG("COLUMN_DECLTYPE: orig returned '%s'", orig_type ? orig_type : "NULL");
    return orig_type;
}

// sqlite3_column_value returns a pointer to a sqlite3_value for a column.
// For PostgreSQL statements, we return a fake sqlite3_value that encodes the pg_stmt and column.
// The sqlite3_value_* functions will decode this to return proper PostgreSQL data.
sqlite3_value* my_sqlite3_column_value(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);
        // column_value is typically called after step(), but just in case...
        if (!pg_stmt->result && !pg_stmt->cached_result && pg_stmt->pg_sql) {
            if (!ensure_pg_result_for_metadata(pg_stmt)) {
                LOG_DEBUG("COLUMN_VALUE: failed to execute query for metadata");
                pthread_mutex_unlock(&pg_stmt->mutex);
                return orig_sqlite3_column_value ? orig_sqlite3_column_value(pStmt, idx) : NULL;
            }
        }
        if (!pg_stmt->result) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return orig_sqlite3_column_value ? orig_sqlite3_column_value(pStmt, idx) : NULL;
        }
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            LOG_ERROR("COLUMN_VALUE_BOUNDS: idx=%d out of bounds (num_cols=%d) sql=%.100s",
                     idx, pg_stmt->num_cols, pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
            pthread_mutex_unlock(&pg_stmt->mutex);
            return NULL;
        }
        int row = pg_stmt->current_row;
        pthread_mutex_unlock(&pg_stmt->mutex);

        // Return a fake value from our pool (thread-safe)
        pthread_mutex_lock(&fake_value_mutex);
        // Use bitmask instead of modulo - always produces 0-255 even after overflow
        unsigned int slot = (fake_value_next++) & 0xFF;
        pg_fake_value_t *fake = &fake_value_pool[slot];
        fake->magic = PG_FAKE_VALUE_MAGIC;
        fake->pg_stmt = pg_stmt;
        fake->col_idx = idx;
        fake->row_idx = row;
        pthread_mutex_unlock(&fake_value_mutex);

        return (sqlite3_value*)fake;
    }
    return orig_sqlite3_column_value ? orig_sqlite3_column_value(pStmt, idx) : NULL;
}

int my_sqlite3_data_count(sqlite3_stmt *pStmt) {
    LOG_DEBUG("DATA_COUNT: stmt=%p", (void*)pStmt);
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);
        // For PostgreSQL statements, return our stored num_cols if we have a valid row
        // Don't fall through to orig_sqlite3_data_count which would fail
        int count = (pg_stmt->current_row < pg_stmt->num_rows) ? pg_stmt->num_cols : 0;
        pthread_mutex_unlock(&pg_stmt->mutex);
        LOG_DEBUG("DATA_COUNT: returning %d (row=%d rows=%d cols=%d)",
                 count, pg_stmt->current_row, pg_stmt->num_rows, pg_stmt->num_cols);
        return count;
    }
    return orig_sqlite3_data_count ? orig_sqlite3_data_count(pStmt) : 0;
}

// ============================================================================
// Value Functions (for sqlite3_column_value returned values)
// ============================================================================

// Intercept sqlite3_value_type to handle our fake values
int my_sqlite3_value_type(sqlite3_value *pVal) {
    if (!pVal) return SQLITE_NULL;  // CRITICAL FIX: NULL check to prevent crash
    pg_fake_value_t *fake = pg_check_fake_value(pVal);
    if (fake && fake->pg_stmt) {
        pg_stmt_t *pg_stmt = (pg_stmt_t*)fake->pg_stmt;
        if (pg_stmt->result && fake->row_idx < pg_stmt->num_rows && fake->col_idx < pg_stmt->num_cols) {
            if (PQgetisnull(pg_stmt->result, fake->row_idx, fake->col_idx)) {
                return SQLITE_NULL;
            }
            // Use the same type logic as my_sqlite3_column_type
            Oid oid = PQftype(pg_stmt->result, fake->col_idx);
            switch (oid) {
                case 20: case 21: case 23: case 26: case 16:  // int8, int2, int4, oid, bool
                    return SQLITE_INTEGER;
                case 700: case 701: case 1700:  // float4, float8, numeric
                    return SQLITE_FLOAT;
                case 17:  // bytea
                    return SQLITE_BLOB;
                default:
                    return SQLITE_TEXT;
            }
        }
        return SQLITE_NULL;
    }
    return orig_sqlite3_value_type ? orig_sqlite3_value_type(pVal) : SQLITE_NULL;
}

// Intercept sqlite3_value_text to handle our fake values
// Static buffers for value_text - LARGE pool, separate from column_text
// 256 buffers x 16KB = 4MB total - prevents race condition wrap-around
static char value_text_buffers[256][16384];
static atomic_int value_text_idx = 0;  // Atomic for thread-safe increment

const unsigned char* my_sqlite3_value_text(sqlite3_value *pVal) {
    if (!pVal) return NULL;  // CRITICAL FIX: NULL check
    pg_fake_value_t *fake = pg_check_fake_value(pVal);
    if (fake && fake->pg_stmt) {
        pg_stmt_t *pg_stmt = (pg_stmt_t*)fake->pg_stmt;
        pthread_mutex_lock(&pg_stmt->mutex);
        if (pg_stmt->result && fake->row_idx < pg_stmt->num_rows && fake->col_idx < pg_stmt->num_cols) {
            if (PQgetisnull(pg_stmt->result, fake->row_idx, fake->col_idx)) {
                pthread_mutex_unlock(&pg_stmt->mutex);
                return NULL;
            }
            // CRITICAL FIX: Copy to static buffer instead of returning PGresult pointer directly
            // This prevents use-after-free when PGresult is cleared
            const char* pg_value = PQgetvalue(pg_stmt->result, fake->row_idx, fake->col_idx);
            if (!pg_value) {
                pthread_mutex_unlock(&pg_stmt->mutex);
                return NULL;
            }

            size_t len = strlen(pg_value);
            if (len >= 16383) len = 16383;

            // Thread-safe buffer allocation using atomic increment
            int buf = atomic_fetch_add(&value_text_idx, 1) & 0xFF;
            memcpy(value_text_buffers[buf], pg_value, len);
            value_text_buffers[buf][len] = '\0';

            pthread_mutex_unlock(&pg_stmt->mutex);
            return (const unsigned char*)value_text_buffers[buf];
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
        return NULL;
    }
    return orig_sqlite3_value_text ? orig_sqlite3_value_text(pVal) : NULL;
}

// Intercept sqlite3_value_int to handle our fake values
int my_sqlite3_value_int(sqlite3_value *pVal) {
    if (!pVal) return 0;  // CRITICAL FIX: NULL check
    pg_fake_value_t *fake = pg_check_fake_value(pVal);
    if (fake && fake->pg_stmt) {
        pg_stmt_t *pg_stmt = (pg_stmt_t*)fake->pg_stmt;
        if (pg_stmt->result && fake->row_idx < pg_stmt->num_rows && fake->col_idx < pg_stmt->num_cols) {
            if (PQgetisnull(pg_stmt->result, fake->row_idx, fake->col_idx)) {
                return 0;
            }
            const char *val = PQgetvalue(pg_stmt->result, fake->row_idx, fake->col_idx);
            if (!val) return 0;
            // Handle PostgreSQL boolean 't'/'f' format
            if (val[0] == 't' && val[1] == '\0') return 1;
            if (val[0] == 'f' && val[1] == '\0') return 0;
            return atoi(val);
        }
        return 0;
    }
    return orig_sqlite3_value_int ? orig_sqlite3_value_int(pVal) : 0;
}

// Intercept sqlite3_value_int64 to handle our fake values
sqlite3_int64 my_sqlite3_value_int64(sqlite3_value *pVal) {
    if (!pVal) return 0;  // CRITICAL FIX: NULL check
    pg_fake_value_t *fake = pg_check_fake_value(pVal);
    if (fake && fake->pg_stmt) {
        pg_stmt_t *pg_stmt = (pg_stmt_t*)fake->pg_stmt;
        if (pg_stmt->result && fake->row_idx < pg_stmt->num_rows && fake->col_idx < pg_stmt->num_cols) {
            if (PQgetisnull(pg_stmt->result, fake->row_idx, fake->col_idx)) {
                return 0;
            }
            const char *val = PQgetvalue(pg_stmt->result, fake->row_idx, fake->col_idx);
            if (!val) return 0;
            // Handle PostgreSQL boolean 't'/'f' format
            if (val[0] == 't' && val[1] == '\0') return 1;
            if (val[0] == 'f' && val[1] == '\0') return 0;
            return atoll(val);
        }
        return 0;
    }
    return orig_sqlite3_value_int64 ? orig_sqlite3_value_int64(pVal) : 0;
}

// Intercept sqlite3_value_double to handle our fake values
double my_sqlite3_value_double(sqlite3_value *pVal) {
    if (!pVal) return 0.0;  // CRITICAL FIX: NULL check
    pg_fake_value_t *fake = pg_check_fake_value(pVal);
    if (fake && fake->pg_stmt) {
        pg_stmt_t *pg_stmt = (pg_stmt_t*)fake->pg_stmt;
        if (pg_stmt->result && fake->row_idx < pg_stmt->num_rows && fake->col_idx < pg_stmt->num_cols) {
            if (PQgetisnull(pg_stmt->result, fake->row_idx, fake->col_idx)) {
                return 0.0;
            }
            const char *val = PQgetvalue(pg_stmt->result, fake->row_idx, fake->col_idx);
            if (!val) return 0.0;
            // Handle PostgreSQL boolean 't'/'f' format
            if (val[0] == 't' && val[1] == '\0') return 1.0;
            if (val[0] == 'f' && val[1] == '\0') return 0.0;
            return atof(val);
        }
        return 0.0;
    }
    return orig_sqlite3_value_double ? orig_sqlite3_value_double(pVal) : 0.0;
}

// Intercept sqlite3_value_bytes to handle our fake values
int my_sqlite3_value_bytes(sqlite3_value *pVal) {
    if (!pVal) return 0;  // CRITICAL FIX: NULL check
    pg_fake_value_t *fake = pg_check_fake_value(pVal);
    if (fake && fake->pg_stmt) {
        pg_stmt_t *pg_stmt = (pg_stmt_t*)fake->pg_stmt;
        if (pg_stmt->result && fake->row_idx < pg_stmt->num_rows && fake->col_idx < pg_stmt->num_cols) {
            if (PQgetisnull(pg_stmt->result, fake->row_idx, fake->col_idx)) {
                return 0;
            }
            return PQgetlength(pg_stmt->result, fake->row_idx, fake->col_idx);
        }
        return 0;
    }
    return orig_sqlite3_value_bytes ? orig_sqlite3_value_bytes(pVal) : 0;
}

// Static buffers for value_blob - LARGE pool, separate from text buffers
// 64 buffers x 64KB = 4MB total - prevents race condition wrap-around
static char value_blob_buffers[64][65536];
static atomic_int value_blob_idx = 0;  // Atomic for thread-safe increment

// Intercept sqlite3_value_blob to handle our fake values
const void* my_sqlite3_value_blob(sqlite3_value *pVal) {
    if (!pVal) return NULL;  // CRITICAL FIX: NULL check
    pg_fake_value_t *fake = pg_check_fake_value(pVal);
    if (fake && fake->pg_stmt) {
        pg_stmt_t *pg_stmt = (pg_stmt_t*)fake->pg_stmt;
        pthread_mutex_lock(&pg_stmt->mutex);
        if (pg_stmt->result && fake->row_idx < pg_stmt->num_rows && fake->col_idx < pg_stmt->num_cols) {
            if (PQgetisnull(pg_stmt->result, fake->row_idx, fake->col_idx)) {
                pthread_mutex_unlock(&pg_stmt->mutex);
                return NULL;
            }
            // CRITICAL FIX: Copy to static buffer to prevent use-after-free
            const char *pg_value = PQgetvalue(pg_stmt->result, fake->row_idx, fake->col_idx);
            int len = PQgetlength(pg_stmt->result, fake->row_idx, fake->col_idx);
            if (!pg_value || len <= 0) {
                pthread_mutex_unlock(&pg_stmt->mutex);
                return NULL;
            }
            if (len > 65535) len = 65535;  // Truncate if too large

            // Thread-safe buffer allocation using atomic increment
            int buf = atomic_fetch_add(&value_blob_idx, 1) & 0x3F;  // % 64 via bitmask
            memcpy(value_blob_buffers[buf], pg_value, len);

            pthread_mutex_unlock(&pg_stmt->mutex);
            return value_blob_buffers[buf];
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
        return NULL;
    }
    return orig_sqlite3_value_blob ? orig_sqlite3_value_blob(pVal) : NULL;
}
