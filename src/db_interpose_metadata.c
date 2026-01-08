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
    // CRITICAL: Prevent recursion when called from within our shim
    if (in_interpose_call && real_sqlite3_errmsg) {
        return real_sqlite3_errmsg(db);
    }

    // Check if we have a tracked error for this db
    pg_connection_t *pg_conn = pg_find_connection(db);
    if (pg_conn && pg_conn->last_error_code != SQLITE_OK && pg_conn->last_error[0] != '\0') {
        // Return our tracked error message
        return pg_conn->last_error;
    }
    // Otherwise, fall through to real SQLite
    // CRITICAL: Use real function pointer if available to avoid recursion
    if (real_sqlite3_errmsg) {
        return real_sqlite3_errmsg(db);
    }
    return orig_sqlite3_errmsg ? orig_sqlite3_errmsg(db) : "unknown error";
}

int my_sqlite3_errcode(sqlite3 *db) {
    // CRITICAL: Prevent recursion when called from within our shim
    if (in_interpose_call && real_sqlite3_errcode) {
        return real_sqlite3_errcode(db);
    }

    // Check if we have a tracked error for this db
    pg_connection_t *pg_conn = pg_find_connection(db);
    if (pg_conn && pg_conn->last_error_code != SQLITE_OK) {
        // Return our tracked error code
        return pg_conn->last_error_code;
    }
    // Otherwise, fall through to real SQLite
    // CRITICAL: Use real function pointer if available to avoid recursion
    if (real_sqlite3_errcode) {
        return real_sqlite3_errcode(db);
    }
    return orig_sqlite3_errcode ? orig_sqlite3_errcode(db) : SQLITE_ERROR;
}

int my_sqlite3_extended_errcode(sqlite3 *db) {
    // For extended error codes, we just use the basic error code
    // since we don't track extended codes
    pg_connection_t *pg_conn = pg_find_connection(db);
    if (pg_conn && pg_conn->last_error_code != SQLITE_OK) {
        return pg_conn->last_error_code;
    }
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
