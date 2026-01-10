/*
 * Plex PostgreSQL Interposing Shim - Metadata & Error Functions
 *
 * Handles sqlite3_changes, sqlite3_last_insert_rowid, sqlite3_errmsg, etc.
 * Also handles collation registration.
 */

#include "db_interpose.h"

// ============================================================================
// Changes / Last Insert Rowid
// ============================================================================

int my_sqlite3_changes(sqlite3 *db) {
    // Prevent recursion: if we're already in an interpose call, return 0
    if (in_interpose_call) {
        return 0;
    }
    in_interpose_call = 1;

    pg_connection_t *pg_conn = pg_find_connection(db);
    int result = 0;

    if (pg_conn && pg_conn->is_pg_active) {
        result = pg_conn->last_changes;
    }
    // For non-PostgreSQL databases, return 0 (safe default)
    // We CANNOT call the original SQLite function with DYLD_FORCE_FLAT_NAMESPACE
    // because it will cause infinite recursion

    in_interpose_call = 0;
    return result;
}

sqlite3_int64 my_sqlite3_changes64(sqlite3 *db) {
    // Prevent recursion: if we're already in an interpose call, return 0
    if (in_interpose_call) {
        return 0;
    }
    in_interpose_call = 1;

    pg_connection_t *pg_conn = pg_find_connection(db);
    sqlite3_int64 result = 0;

    if (pg_conn && pg_conn->is_pg_active) {
        result = (sqlite3_int64)pg_conn->last_changes;
    }
    // For non-PostgreSQL databases, return 0 (safe default)

    in_interpose_call = 0;
    return result;
}

sqlite3_int64 my_sqlite3_last_insert_rowid(sqlite3 *db) {
    // Prevent recursion: if we're already in an interpose call, return 0
    if (in_interpose_call) {
        return 0;
    }
    in_interpose_call = 1;

    pg_connection_t *pg_conn = pg_find_connection(db);
    sqlite3_int64 result = 0;

    // Only use PostgreSQL lastval() if we found the EXACT connection for this db
    // Using a fallback connection would return wrong values from different tables
    if (pg_conn && pg_conn->is_pg_active && pg_conn->conn) {
        // CRITICAL FIX: Lock connection mutex to prevent concurrent libpq access
        pthread_mutex_lock(&pg_conn->mutex);
        PGresult *res = PQexec(pg_conn->conn, "SELECT lastval()");
        if (PQresultStatus(res) == PGRES_TUPLES_OK && PQntuples(res) > 0) {
            sqlite3_int64 rowid = atoll(PQgetvalue(res, 0, 0) ?: "0");
            PQclear(res);
            pthread_mutex_unlock(&pg_conn->mutex);
            if (rowid > 0) {
                LOG_DEBUG("last_insert_rowid: lastval() = %lld on conn %p for db %p",
                        rowid, (void*)pg_conn, (void*)db);
                result = rowid;
            }
        } else {
            PQclear(res);
            pthread_mutex_unlock(&pg_conn->mutex);
        }
    }
    // For non-PostgreSQL databases, return 0 (safe default)

    in_interpose_call = 0;
    return result;
}

// ============================================================================
// Error Handling
// ============================================================================

// CRITICAL FIX for SOCI "not an error" exception:
// When we abort prepare_v2 early (e.g., stack protection), we return an error
// code BUT never call the real sqlite3_prepare_v2, so SQLite's internal error
// state remains SQLITE_OK. SOCI then calls sqlite3_errmsg() and gets "not an
// error" instead of our actual error message. We must intercept errmsg/errcode
// to return our tracked error state when we've set it.

const char* my_sqlite3_errmsg(sqlite3 *db) {
    LOG_DEBUG("ERRMSG: db=%p", (void*)db);
    // CRITICAL: Prevent recursion when called from within our shim
    if (in_interpose_call && real_sqlite3_errmsg) {
        return real_sqlite3_errmsg(db);
    }

    // Check if this is a PostgreSQL-managed connection
    pg_connection_t *pg_conn = pg_find_connection(db);
    if (pg_conn) {
        // If we have a tracked error, return it
        if (pg_conn->last_error_code != SQLITE_OK && pg_conn->last_error[0] != '\0') {
            LOG_DEBUG("ERRMSG: returning tracked error='%s'", pg_conn->last_error);
            return pg_conn->last_error;
        }
        // PostgreSQL connection with no error - return "not an error"
        // Don't fall through to real SQLite which would return garbage for our db handle
        LOG_DEBUG("ERRMSG: returning 'not an error'");
        return "not an error";
    }

    // Only fall through to real SQLite for non-PostgreSQL databases
    const char *msg = NULL;
    if (real_sqlite3_errmsg) {
        msg = real_sqlite3_errmsg(db);
    } else if (orig_sqlite3_errmsg) {
        msg = orig_sqlite3_errmsg(db);
    } else {
        msg = "unknown error";
    }
    return msg;
}

int my_sqlite3_errcode(sqlite3 *db) {
    LOG_DEBUG("ERRCODE: db=%p", (void*)db);
    // CRITICAL: Prevent recursion when called from within our shim
    if (in_interpose_call && real_sqlite3_errcode) {
        return real_sqlite3_errcode(db);
    }

    // Check if this is a PostgreSQL-managed connection
    pg_connection_t *pg_conn = pg_find_connection(db);
    if (pg_conn) {
        // Return our tracked error code (SQLITE_OK if no error)
        LOG_DEBUG("ERRCODE: pg_conn found, returning code=%d", pg_conn->last_error_code);
        return pg_conn->last_error_code;
    }

    // Only fall through to real SQLite for non-PostgreSQL databases
    int code;
    if (real_sqlite3_errcode) {
        code = real_sqlite3_errcode(db);
    } else if (orig_sqlite3_errcode) {
        code = orig_sqlite3_errcode(db);
    } else {
        code = SQLITE_ERROR;
    }
    return code;
}

int my_sqlite3_extended_errcode(sqlite3 *db) {
    // For extended error codes, we use the basic error code since we don't track extended codes
    pg_connection_t *pg_conn = pg_find_connection(db);
    if (pg_conn) {
        // Return our tracked error code (SQLITE_OK if no error)
        return pg_conn->last_error_code;
    }
    // Only fall through to real SQLite for non-PostgreSQL databases
    return orig_sqlite3_extended_errcode ? orig_sqlite3_extended_errcode(db) : SQLITE_ERROR;
}

// ============================================================================
// Get Table
// ============================================================================

int my_sqlite3_get_table(sqlite3 *db, const char *sql, char ***pazResult,
                         int *pnRow, int *pnColumn, char **pzErrMsg) {
    // CRITICAL FIX: NULL check to prevent crash
    if (!sql) {
        return orig_sqlite3_get_table ? orig_sqlite3_get_table(db, sql, pazResult, pnRow, pnColumn, pzErrMsg) : SQLITE_ERROR;
    }

    pg_connection_t *pg_conn = pg_find_connection(db);

    if (pg_conn && pg_conn->is_pg_active && pg_conn->conn && is_read_operation(sql)) {
        sql_translation_t trans = sql_translate(sql);
        if (trans.success && trans.sql) {
            // CRITICAL FIX: Lock connection mutex to prevent concurrent libpq access
            pthread_mutex_lock(&pg_conn->mutex);
            PGresult *res = PQexec(pg_conn->conn, trans.sql);
            if (PQresultStatus(res) == PGRES_TUPLES_OK) {
                int nrows = PQntuples(res);
                int ncols = PQnfields(res);
                int total = (nrows + 1) * ncols + 1;
                char **result = malloc(total * sizeof(char*));
                if (result) {
                    for (int c = 0; c < ncols; c++) {
                        result[c] = strdup(PQfname(res, c));
                    }
                    for (int r = 0; r < nrows; r++) {
                        for (int c = 0; c < ncols; c++) {
                            result[(r + 1) * ncols + c] = PQgetisnull(res, r, c) ? NULL : strdup(PQgetvalue(res, r, c));
                        }
                    }
                    result[total - 1] = NULL;
                    *pazResult = result;
                    *pnRow = nrows;
                    *pnColumn = ncols;
                    if (pzErrMsg) *pzErrMsg = NULL;
                    PQclear(res);
                    pthread_mutex_unlock(&pg_conn->mutex);
                    sql_translation_free(&trans);
                    return SQLITE_OK;
                }
            }
            PQclear(res);
            pthread_mutex_unlock(&pg_conn->mutex);
        }
        sql_translation_free(&trans);
    }

    return orig_sqlite3_get_table ? orig_sqlite3_get_table(db, sql, pazResult, pnRow, pnColumn, pzErrMsg) : SQLITE_ERROR;
}

// ============================================================================
// Collation Registration - Pretend ICU collations are registered
// ============================================================================

int my_sqlite3_create_collation(
    sqlite3 *db,
    const char *zName,
    int eTextRep,
    void *pArg,
    int(*xCompare)(void*,int,const void*,int,const void*)
) {
    // For icu_root and similar ICU collations, just pretend we registered it
    // Our SQL translator strips COLLATE clauses from queries
    if (zName && (strcasestr(zName, "icu") || strcasestr(zName, "ICU"))) {
        LOG_DEBUG("Faking registration of collation: %s", zName);
        return SQLITE_OK;
    }
    // For other collations, pass through to real SQLite
    return orig_sqlite3_create_collation ? orig_sqlite3_create_collation(db, zName, eTextRep, pArg, xCompare) : SQLITE_ERROR;
}

int my_sqlite3_create_collation_v2(
    sqlite3 *db,
    const char *zName,
    int eTextRep,
    void *pArg,
    int(*xCompare)(void*,int,const void*,int,const void*),
    void(*xDestroy)(void*)
) {
    // For icu_root and similar ICU collations, just pretend we registered it
    if (zName && (strcasestr(zName, "icu") || strcasestr(zName, "ICU"))) {
        LOG_DEBUG("Faking registration of collation v2: %s", zName);
        return SQLITE_OK;
    }
    // For other collations, pass through to real SQLite
    return orig_sqlite3_create_collation_v2 ? orig_sqlite3_create_collation_v2(db, zName, eTextRep, pArg, xCompare, xDestroy) : SQLITE_ERROR;
}

// ============================================================================
// Memory Management
// ============================================================================

void my_sqlite3_free(void *ptr) {
    // Just pass through to real SQLite - we don't allocate SQLite memory
    if (orig_sqlite3_free) {
        orig_sqlite3_free(ptr);
    } else {
        // Fallback to standard free if SQLite free not available
        free(ptr);
    }
}

void* my_sqlite3_malloc(int n) {
    // Pass through to real SQLite
    if (orig_sqlite3_malloc) {
        return orig_sqlite3_malloc(n);
    }
    // Fallback to standard malloc
    return malloc(n);
}

// ============================================================================
// Statement Info Functions
// ============================================================================

sqlite3* my_sqlite3_db_handle(sqlite3_stmt *pStmt) {
    LOG_DEBUG("DB_HANDLE: pStmt=%p", (void*)pStmt);
    if (!pStmt) return NULL;

    // Check if this is one of our PostgreSQL statements
    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        // For PostgreSQL statements, we need to return the ORIGINAL SQLite db handle
        // Not the pool connection's shadow_db (which is NULL for pool connections)
        // The shadow_stmt was set during prepare with the real SQLite statement
        // We can get the db handle from that
        if (pg_stmt->shadow_stmt && orig_sqlite3_db_handle) {
            sqlite3 *db = orig_sqlite3_db_handle(pg_stmt->shadow_stmt);
            LOG_DEBUG("DB_HANDLE: returning from shadow_stmt=%p", (void*)db);
            return db;
        }
        // Fallback to conn->shadow_db if available (non-pool connections)
        if (pg_stmt->conn && pg_stmt->conn->shadow_db) {
            LOG_DEBUG("DB_HANDLE: returning shadow_db=%p", (void*)pg_stmt->conn->shadow_db);
            return pg_stmt->conn->shadow_db;
        }
        LOG_DEBUG("DB_HANDLE: pg_stmt has no valid db handle");
        return NULL;
    }

    // Pass through to real SQLite for non-PG statements
    if (orig_sqlite3_db_handle) {
        sqlite3 *db = orig_sqlite3_db_handle(pStmt);
        LOG_DEBUG("DB_HANDLE: returning orig=%p", (void*)db);
        return db;
    }
    return NULL;
}

const char* my_sqlite3_sql(sqlite3_stmt *pStmt) {
    if (!pStmt) return NULL;

    // Check if this is one of our PostgreSQL statements
    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        // Return the original SQL (before translation)
        return pg_stmt->sql;
    }

    // Pass through to real SQLite for non-PG statements
    if (orig_sqlite3_sql) {
        return orig_sqlite3_sql(pStmt);
    }
    return NULL;
}

int my_sqlite3_bind_parameter_count(sqlite3_stmt *pStmt) {
    if (!pStmt) return 0;

    // Check if this is one of our PostgreSQL statements
    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        return pg_stmt->param_count;
    }

    // Pass through to real SQLite for non-PG statements
    if (orig_sqlite3_bind_parameter_count) {
        return orig_sqlite3_bind_parameter_count(pStmt);
    }
    return 0;
}

int my_sqlite3_stmt_readonly(sqlite3_stmt *pStmt) {
    if (!pStmt) return 1;  // NULL treated as readonly (safe default)

    // Check if this is one of our PostgreSQL statements
    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        // Use our is_read_operation check on the original SQL
        if (pg_stmt->sql) {
            return is_read_operation(pg_stmt->sql) ? 1 : 0;
        }
        return 1;  // Default to readonly if no SQL
    }

    // Pass through to real SQLite for non-PG statements
    if (orig_sqlite3_stmt_readonly) {
        return orig_sqlite3_stmt_readonly(pStmt);
    }
    return 1;  // Default to readonly
}

int my_sqlite3_stmt_busy(sqlite3_stmt *pStmt) {
    LOG_DEBUG("STMT_BUSY: stmt=%p", (void*)pStmt);
    if (!pStmt) return 0;

    // Check if this is one of our PostgreSQL statements
    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        // Statement is "busy" if we have returned SQLITE_ROW and have more rows
        // This matches SQLite semantics: busy = step() was called, results pending
        int busy = (pg_stmt->result != NULL && pg_stmt->current_row < pg_stmt->num_rows);
        LOG_DEBUG("STMT_BUSY: pg_stmt, result=%p current_row=%d num_rows=%d -> busy=%d",
                  (void*)pg_stmt->result, pg_stmt->current_row, pg_stmt->num_rows, busy);
        return busy;
    }

    // Pass through to real SQLite for non-PG statements
    if (orig_sqlite3_stmt_busy) {
        return orig_sqlite3_stmt_busy(pStmt);
    }
    return 0;
}

int my_sqlite3_stmt_status(sqlite3_stmt *pStmt, int op, int resetFlg) {
    LOG_DEBUG("STMT_STATUS: stmt=%p op=%d reset=%d", (void*)pStmt, op, resetFlg);
    if (!pStmt) return 0;

    // Check if this is one of our PostgreSQL statements
    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        // For PostgreSQL statements, return 0 for all status counters
        // SQLite ops: SQLITE_STMTSTATUS_FULLSCAN_STEP (1), VM_STEP (4), SORT (2), etc.
        LOG_DEBUG("STMT_STATUS: pg_stmt returning 0");
        return 0;
    }

    // Pass through to real SQLite for non-PG statements
    if (orig_sqlite3_stmt_status) {
        return orig_sqlite3_stmt_status(pStmt, op, resetFlg);
    }
    return 0;
}

const char* my_sqlite3_bind_parameter_name(sqlite3_stmt *pStmt, int idx) {
    LOG_DEBUG("BIND_PARAM_NAME: stmt=%p idx=%d", (void*)pStmt, idx);
    if (!pStmt) return NULL;

    // Check if this is one of our PostgreSQL statements
    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        // Return the parameter name if we have it stored
        // Note: SQLite uses 1-based indexing, our param_names array is 0-based
        if (idx > 0 && idx <= pg_stmt->param_count && pg_stmt->param_names) {
            const char *name = pg_stmt->param_names[idx - 1];
            LOG_DEBUG("BIND_PARAM_NAME: pg_stmt returning '%s'", name ? name : "NULL");
            return name;
        }
        LOG_DEBUG("BIND_PARAM_NAME: pg_stmt idx out of range, returning NULL");
        return NULL;
    }

    // Pass through to real SQLite for non-PG statements
    if (orig_sqlite3_bind_parameter_name) {
        return orig_sqlite3_bind_parameter_name(pStmt, idx);
    }
    return NULL;
}

// sqlite3_bind_parameter_index returns the index of a named parameter.
// Returns 0 if the parameter is not found.
int my_sqlite3_bind_parameter_index(sqlite3_stmt *pStmt, const char *zName) {
    LOG_DEBUG("BIND_PARAM_INDEX: stmt=%p name='%s'", (void*)pStmt, zName ? zName : "NULL");
    if (!pStmt || !zName) return 0;

    // Check if this is one of our PostgreSQL statements
    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        // Search for the parameter name in our param_names array
        if (pg_stmt->param_names) {
            for (int i = 0; i < pg_stmt->param_count; i++) {
                if (pg_stmt->param_names[i] && strcmp(pg_stmt->param_names[i], zName) == 0) {
                    LOG_DEBUG("BIND_PARAM_INDEX: found '%s' at index %d", zName, i + 1);
                    return i + 1;  // SQLite uses 1-based indexing
                }
            }
        }
        LOG_DEBUG("BIND_PARAM_INDEX: '%s' not found in pg_stmt", zName);
        return 0;
    }

    // Pass through to real SQLite for non-PG statements
    if (orig_sqlite3_bind_parameter_index) {
        return orig_sqlite3_bind_parameter_index(pStmt, zName);
    }
    return 0;
}

// ============================================================================
// Expanded SQL - Returns SQL with parameters substituted
// ============================================================================

char* my_sqlite3_expanded_sql(sqlite3_stmt *pStmt) {
    if (!pStmt) return NULL;

    // Check if this is one of our PostgreSQL statements
    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        // For PostgreSQL statements, build expanded SQL from pg_sql + bound params
        const char *base_sql = pg_stmt->pg_sql ? pg_stmt->pg_sql : pg_stmt->sql;
        if (!base_sql) return NULL;

        // Simple case: no parameters, just return a copy
        if (pg_stmt->param_count == 0) {
            size_t len = strlen(base_sql);
            char *result = my_sqlite3_malloc((int)(len + 1));
            if (result) {
                memcpy(result, base_sql, len + 1);
            }
            return result;
        }

        // Complex case: substitute $1, $2, ... with actual values
        // First, estimate the size needed
        size_t estimated_size = strlen(base_sql) + 1;
        for (int i = 0; i < pg_stmt->param_count && i < MAX_PARAMS; i++) {
            if (pg_stmt->param_values[i]) {
                estimated_size += strlen(pg_stmt->param_values[i]) + 3;  // quotes + safety
            } else {
                estimated_size += 4;  // "NULL"
            }
        }
        estimated_size *= 2;  // Extra safety margin

        char *result = my_sqlite3_malloc((int)estimated_size);
        if (!result) return NULL;

        // Simple substitution: replace $1, $2, etc with values
        const char *src = base_sql;
        char *dst = result;
        char *end = result + estimated_size - 1;

        while (*src && dst < end) {
            if (*src == '$' && src[1] >= '1' && src[1] <= '9') {
                // Parse parameter number
                int param_num = 0;
                const char *p = src + 1;
                while (*p >= '0' && *p <= '9') {
                    param_num = param_num * 10 + (*p - '0');
                    p++;
                }
                // Substitute with value (1-indexed)
                int idx = param_num - 1;
                if (idx >= 0 && idx < pg_stmt->param_count && idx < MAX_PARAMS) {
                    const char *val = pg_stmt->param_values[idx];
                    if (val) {
                        // Quote text values
                        *dst++ = '\'';
                        while (*val && dst < end - 1) {
                            if (*val == '\'') {
                                *dst++ = '\'';  // Escape quote
                            }
                            *dst++ = *val++;
                        }
                        *dst++ = '\'';
                    } else {
                        // NULL value
                        if (dst + 4 < end) {
                            memcpy(dst, "NULL", 4);
                            dst += 4;
                        }
                    }
                } else {
                    // Unknown parameter, copy as-is
                    while (src < p && dst < end) {
                        *dst++ = *src++;
                    }
                    continue;
                }
                src = p;
            } else {
                *dst++ = *src++;
            }
        }
        *dst = '\0';
        return result;
    }

    // Pass through to real SQLite for non-PG statements
    if (orig_sqlite3_expanded_sql) {
        return orig_sqlite3_expanded_sql(pStmt);
    }
    return NULL;
}
