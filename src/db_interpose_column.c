/*
 * Plex PostgreSQL Interposing Shim - Column & Value Access
 *
 * Handles sqlite3_column_* and sqlite3_value_* function interposition.
 * These functions read data from PostgreSQL result sets.
 */

#include "db_interpose.h"

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
// Column Functions
// ============================================================================

int my_sqlite3_column_count(sqlite3_stmt *pStmt) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);
        if (!pg_stmt->result) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return orig_sqlite3_column_count ? orig_sqlite3_column_count(pStmt) : 0;
        }
        int count = pg_stmt->num_cols;
        pthread_mutex_unlock(&pg_stmt->mutex);
        return count;
    }
    return orig_sqlite3_column_count ? orig_sqlite3_column_count(pStmt) : 0;
}

int my_sqlite3_column_type(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);
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

const unsigned char* my_sqlite3_column_text(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);
        // CRITICAL FIX: Check result AFTER lock to prevent use-after-free
        if (!pg_stmt->result) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return NULL;
        }

        if (idx < 0 || idx >= pg_stmt->num_cols || idx >= MAX_PARAMS) {
            LOG_ERROR("COL_TEXT_BOUNDS: idx=%d out of bounds (num_cols=%d) sql=%.100s",
                     idx, pg_stmt->num_cols, pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
            pthread_mutex_unlock(&pg_stmt->mutex);
            return NULL;
        }
        int row = pg_stmt->current_row;
        if (row < 0 || row >= pg_stmt->num_rows) {
            LOG_ERROR("COL_TEXT_ROW_BOUNDS: row=%d out of bounds (num_rows=%d)", row, pg_stmt->num_rows);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return NULL;
        }
        if (!PQgetisnull(pg_stmt->result, row, idx)) {
            // Check if we already have this value cached for the current row
            if (pg_stmt->cached_row == row && pg_stmt->cached_text[idx]) {
                const unsigned char *result = (const unsigned char*)pg_stmt->cached_text[idx];
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

            // Get value from PostgreSQL and cache it
            const char* pg_value = PQgetvalue(pg_stmt->result, row, idx);
            if (pg_value) {
                pg_stmt->cached_text[idx] = strdup(pg_value);
                if (!pg_stmt->cached_text[idx]) {
                    LOG_ERROR("COL_TEXT: strdup failed for column %d", idx);
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return NULL;
                }
            }

            const unsigned char *result = (const unsigned char*)pg_stmt->cached_text[idx];
            pthread_mutex_unlock(&pg_stmt->mutex);
            return result;
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
        return NULL;
    }
    return orig_sqlite3_column_text ? orig_sqlite3_column_text(pStmt, idx) : NULL;
}

const void* my_sqlite3_column_blob(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);
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
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);
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
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);
        if (!pg_stmt->result) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return orig_sqlite3_column_name ? orig_sqlite3_column_name(pStmt, idx) : NULL;
        }
        if (idx >= 0 && idx < pg_stmt->num_cols) {
            const char *name = PQfname(pg_stmt->result, idx);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return name;
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
    }
    return orig_sqlite3_column_name ? orig_sqlite3_column_name(pStmt, idx) : NULL;
}

// sqlite3_column_value returns a pointer to a sqlite3_value for a column.
// For PostgreSQL statements, we return a fake sqlite3_value that encodes the pg_stmt and column.
// The sqlite3_value_* functions will decode this to return proper PostgreSQL data.
sqlite3_value* my_sqlite3_column_value(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);
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
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);
        if (!pg_stmt->result) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return orig_sqlite3_data_count ? orig_sqlite3_data_count(pStmt) : 0;
        }
        int count = (pg_stmt->current_row < pg_stmt->num_rows) ? pg_stmt->num_cols : 0;
        pthread_mutex_unlock(&pg_stmt->mutex);
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
const unsigned char* my_sqlite3_value_text(sqlite3_value *pVal) {
    if (!pVal) return NULL;  // CRITICAL FIX: NULL check
    pg_fake_value_t *fake = pg_check_fake_value(pVal);
    if (fake && fake->pg_stmt) {
        pg_stmt_t *pg_stmt = (pg_stmt_t*)fake->pg_stmt;
        if (pg_stmt->result && fake->row_idx < pg_stmt->num_rows && fake->col_idx < pg_stmt->num_cols) {
            if (PQgetisnull(pg_stmt->result, fake->row_idx, fake->col_idx)) {
                return NULL;
            }
            return (const unsigned char*)PQgetvalue(pg_stmt->result, fake->row_idx, fake->col_idx);
        }
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
            return val ? atoi(val) : 0;
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
            return val ? atoll(val) : 0;
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
            return val ? atof(val) : 0.0;
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

// Intercept sqlite3_value_blob to handle our fake values
const void* my_sqlite3_value_blob(sqlite3_value *pVal) {
    if (!pVal) return NULL;  // CRITICAL FIX: NULL check
    pg_fake_value_t *fake = pg_check_fake_value(pVal);
    if (fake && fake->pg_stmt) {
        pg_stmt_t *pg_stmt = (pg_stmt_t*)fake->pg_stmt;
        if (pg_stmt->result && fake->row_idx < pg_stmt->num_rows && fake->col_idx < pg_stmt->num_cols) {
            if (PQgetisnull(pg_stmt->result, fake->row_idx, fake->col_idx)) {
                return NULL;
            }
            return PQgetvalue(pg_stmt->result, fake->row_idx, fake->col_idx);
        }
        return NULL;
    }
    return orig_sqlite3_value_blob ? orig_sqlite3_value_blob(pVal) : NULL;
}
