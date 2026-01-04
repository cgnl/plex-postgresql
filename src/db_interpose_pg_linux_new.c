/*
 * Plex PostgreSQL Interposing Shim - Linux Version
 *
 * Uses LD_PRELOAD to intercept SQLite calls and redirect to PostgreSQL.
 * Uses shared modules (pg_client.c, pg_statement.c, etc.) for PostgreSQL handling.
 *
 * Build:
 *   gcc -shared -fPIC -o db_interpose_pg.so db_interpose_pg_linux.c $(OBJECTS) \
 *       -Iinclude -Isrc -lpq -lsqlite3 -ldl -lpthread
 *
 * Usage:
 *   export LD_PRELOAD=/path/to/db_interpose_pg.so
 *   export PLEX_PG_HOST=localhost
 *   ... other env vars
 *   /usr/lib/plexmediaserver/Plex\ Media\ Server
 */

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <stdint.h>
#include <stdarg.h>
#include <time.h>
#include <ctype.h>
#include <dlfcn.h>
#include <sqlite3.h>
#include <libpq-fe.h>

// Use modular components (shared with macOS)
#include "pg_types.h"
#include "pg_logging.h"
#include "pg_config.h"
#include "pg_client.h"
#include "pg_statement.h"
#include "sql_translator.h"

// ============================================================================
// Original SQLite Function Pointers (via dlsym RTLD_NEXT)
// ============================================================================

// Core functions
static int (*orig_sqlite3_open)(const char*, sqlite3**) = NULL;
static int (*orig_sqlite3_open_v2)(const char*, sqlite3**, int, const char*) = NULL;
static int (*orig_sqlite3_close)(sqlite3*) = NULL;
static int (*orig_sqlite3_close_v2)(sqlite3*) = NULL;
static int (*orig_sqlite3_exec)(sqlite3*, const char*, int(*)(void*,int,char**,char**), void*, char**) = NULL;
static int (*orig_sqlite3_prepare)(sqlite3*, const char*, int, sqlite3_stmt**, const char**) = NULL;
static int (*orig_sqlite3_prepare_v2)(sqlite3*, const char*, int, sqlite3_stmt**, const char**) = NULL;
static int (*orig_sqlite3_prepare_v3)(sqlite3*, const char*, int, unsigned int, sqlite3_stmt**, const char**) = NULL;
static int (*orig_sqlite3_step)(sqlite3_stmt*) = NULL;
static int (*orig_sqlite3_reset)(sqlite3_stmt*) = NULL;
static int (*orig_sqlite3_finalize)(sqlite3_stmt*) = NULL;
static int (*orig_sqlite3_clear_bindings)(sqlite3_stmt*) = NULL;

// Bind functions
static int (*orig_sqlite3_bind_int)(sqlite3_stmt*, int, int) = NULL;
static int (*orig_sqlite3_bind_int64)(sqlite3_stmt*, int, sqlite3_int64) = NULL;
static int (*orig_sqlite3_bind_double)(sqlite3_stmt*, int, double) = NULL;
static int (*orig_sqlite3_bind_text)(sqlite3_stmt*, int, const char*, int, void(*)(void*)) = NULL;
static int (*orig_sqlite3_bind_blob)(sqlite3_stmt*, int, const void*, int, void(*)(void*)) = NULL;
static int (*orig_sqlite3_bind_null)(sqlite3_stmt*, int) = NULL;
static int (*orig_sqlite3_bind_parameter_count)(sqlite3_stmt*) = NULL;

// Column functions
static int (*orig_sqlite3_column_count)(sqlite3_stmt*) = NULL;
static int (*orig_sqlite3_column_type)(sqlite3_stmt*, int) = NULL;
static int (*orig_sqlite3_column_int)(sqlite3_stmt*, int) = NULL;
static sqlite3_int64 (*orig_sqlite3_column_int64)(sqlite3_stmt*, int) = NULL;
static double (*orig_sqlite3_column_double)(sqlite3_stmt*, int) = NULL;
static const unsigned char* (*orig_sqlite3_column_text)(sqlite3_stmt*, int) = NULL;
static const void* (*orig_sqlite3_column_blob)(sqlite3_stmt*, int) = NULL;
static int (*orig_sqlite3_column_bytes)(sqlite3_stmt*, int) = NULL;
static const char* (*orig_sqlite3_column_name)(sqlite3_stmt*, int) = NULL;
static sqlite3_value* (*orig_sqlite3_column_value)(sqlite3_stmt*, int) = NULL;

// Value functions
static int (*orig_sqlite3_value_type)(sqlite3_value*) = NULL;
static int (*orig_sqlite3_value_int)(sqlite3_value*) = NULL;
static sqlite3_int64 (*orig_sqlite3_value_int64)(sqlite3_value*) = NULL;
static double (*orig_sqlite3_value_double)(sqlite3_value*) = NULL;
static const unsigned char* (*orig_sqlite3_value_text)(sqlite3_value*) = NULL;
static int (*orig_sqlite3_value_bytes)(sqlite3_value*) = NULL;
static const void* (*orig_sqlite3_value_blob)(sqlite3_value*) = NULL;

// Other
static sqlite3_int64 (*orig_sqlite3_last_insert_rowid)(sqlite3*) = NULL;
static int (*orig_sqlite3_changes)(sqlite3*) = NULL;
static const char* (*orig_sqlite3_errmsg)(sqlite3*) = NULL;
static int (*orig_sqlite3_errcode)(sqlite3*) = NULL;
static int (*orig_sqlite3_busy_timeout)(sqlite3*, int) = NULL;
static int (*orig_sqlite3_get_autocommit)(sqlite3*) = NULL;
static int (*orig_sqlite3_table_column_metadata)(sqlite3*, const char*, const char*, const char*,
    char const**, char const**, int*, int*, int*) = NULL;
static int (*orig_sqlite3_wal_checkpoint_v2)(sqlite3*, const char*, int, int*, int*) = NULL;
static int (*orig_sqlite3_wal_autocheckpoint)(sqlite3*, int) = NULL;
static int (*orig_sqlite3_data_count)(sqlite3_stmt*) = NULL;
static sqlite3* (*orig_sqlite3_db_handle)(sqlite3_stmt*) = NULL;

// ============================================================================
// Initialization
// ============================================================================

static int shim_initialized = 0;
static pthread_mutex_t init_mutex = PTHREAD_MUTEX_INITIALIZER;

static void load_original_functions(void) {
    // Core
    orig_sqlite3_open = dlsym(RTLD_NEXT, "sqlite3_open");
    orig_sqlite3_open_v2 = dlsym(RTLD_NEXT, "sqlite3_open_v2");
    orig_sqlite3_close = dlsym(RTLD_NEXT, "sqlite3_close");
    orig_sqlite3_close_v2 = dlsym(RTLD_NEXT, "sqlite3_close_v2");
    orig_sqlite3_exec = dlsym(RTLD_NEXT, "sqlite3_exec");
    orig_sqlite3_prepare = dlsym(RTLD_NEXT, "sqlite3_prepare");
    orig_sqlite3_prepare_v2 = dlsym(RTLD_NEXT, "sqlite3_prepare_v2");
    orig_sqlite3_prepare_v3 = dlsym(RTLD_NEXT, "sqlite3_prepare_v3");
    orig_sqlite3_step = dlsym(RTLD_NEXT, "sqlite3_step");
    orig_sqlite3_reset = dlsym(RTLD_NEXT, "sqlite3_reset");
    orig_sqlite3_finalize = dlsym(RTLD_NEXT, "sqlite3_finalize");
    orig_sqlite3_clear_bindings = dlsym(RTLD_NEXT, "sqlite3_clear_bindings");

    // Bind
    orig_sqlite3_bind_int = dlsym(RTLD_NEXT, "sqlite3_bind_int");
    orig_sqlite3_bind_int64 = dlsym(RTLD_NEXT, "sqlite3_bind_int64");
    orig_sqlite3_bind_double = dlsym(RTLD_NEXT, "sqlite3_bind_double");
    orig_sqlite3_bind_text = dlsym(RTLD_NEXT, "sqlite3_bind_text");
    orig_sqlite3_bind_blob = dlsym(RTLD_NEXT, "sqlite3_bind_blob");
    orig_sqlite3_bind_null = dlsym(RTLD_NEXT, "sqlite3_bind_null");
    orig_sqlite3_bind_parameter_count = dlsym(RTLD_NEXT, "sqlite3_bind_parameter_count");

    // Column
    orig_sqlite3_column_count = dlsym(RTLD_NEXT, "sqlite3_column_count");
    orig_sqlite3_column_type = dlsym(RTLD_NEXT, "sqlite3_column_type");
    orig_sqlite3_column_int = dlsym(RTLD_NEXT, "sqlite3_column_int");
    orig_sqlite3_column_int64 = dlsym(RTLD_NEXT, "sqlite3_column_int64");
    orig_sqlite3_column_double = dlsym(RTLD_NEXT, "sqlite3_column_double");
    orig_sqlite3_column_text = dlsym(RTLD_NEXT, "sqlite3_column_text");
    orig_sqlite3_column_blob = dlsym(RTLD_NEXT, "sqlite3_column_blob");
    orig_sqlite3_column_bytes = dlsym(RTLD_NEXT, "sqlite3_column_bytes");
    orig_sqlite3_column_name = dlsym(RTLD_NEXT, "sqlite3_column_name");
    orig_sqlite3_column_value = dlsym(RTLD_NEXT, "sqlite3_column_value");

    // Value
    orig_sqlite3_value_type = dlsym(RTLD_NEXT, "sqlite3_value_type");
    orig_sqlite3_value_int = dlsym(RTLD_NEXT, "sqlite3_value_int");
    orig_sqlite3_value_int64 = dlsym(RTLD_NEXT, "sqlite3_value_int64");
    orig_sqlite3_value_double = dlsym(RTLD_NEXT, "sqlite3_value_double");
    orig_sqlite3_value_text = dlsym(RTLD_NEXT, "sqlite3_value_text");
    orig_sqlite3_value_bytes = dlsym(RTLD_NEXT, "sqlite3_value_bytes");
    orig_sqlite3_value_blob = dlsym(RTLD_NEXT, "sqlite3_value_blob");

    // Other
    orig_sqlite3_last_insert_rowid = dlsym(RTLD_NEXT, "sqlite3_last_insert_rowid");
    orig_sqlite3_changes = dlsym(RTLD_NEXT, "sqlite3_changes");
    orig_sqlite3_errmsg = dlsym(RTLD_NEXT, "sqlite3_errmsg");
    orig_sqlite3_errcode = dlsym(RTLD_NEXT, "sqlite3_errcode");
    orig_sqlite3_busy_timeout = dlsym(RTLD_NEXT, "sqlite3_busy_timeout");
    orig_sqlite3_get_autocommit = dlsym(RTLD_NEXT, "sqlite3_get_autocommit");
    orig_sqlite3_table_column_metadata = dlsym(RTLD_NEXT, "sqlite3_table_column_metadata");
    orig_sqlite3_wal_checkpoint_v2 = dlsym(RTLD_NEXT, "sqlite3_wal_checkpoint_v2");
    orig_sqlite3_wal_autocheckpoint = dlsym(RTLD_NEXT, "sqlite3_wal_autocheckpoint");
    orig_sqlite3_data_count = dlsym(RTLD_NEXT, "sqlite3_data_count");
    orig_sqlite3_db_handle = dlsym(RTLD_NEXT, "sqlite3_db_handle");
}

__attribute__((constructor))
static void shim_init(void) {
    pthread_mutex_lock(&init_mutex);
    if (!shim_initialized) {
        load_original_functions();
        init_logging();
        load_config();
        shim_initialized = 1;
        LOG_INFO("Linux PostgreSQL shim initialized");
    }
    pthread_mutex_unlock(&init_mutex);
}

// ============================================================================
// Helper: Check if database should use PostgreSQL
// ============================================================================

static int should_redirect_db(const char *path) {
    if (!path) return 0;

    // Check for Plex library databases
    if (strstr(path, "com.plexapp.plugins.library.db") ||
        strstr(path, "com.plexapp.plugins.library.blobs.db")) {
        return 1;
    }
    return 0;
}

// ============================================================================
// Interposed Functions: Database Open/Close
// ============================================================================

int sqlite3_open(const char *filename, sqlite3 **ppDb) {
    if (!orig_sqlite3_open) shim_init();

    int rc = orig_sqlite3_open(filename, ppDb);
    if (rc != SQLITE_OK || !should_redirect_db(filename)) {
        return rc;
    }

    // Get or create PostgreSQL connection via pool
    pg_connection_t *conn = pg_get_thread_connection(filename);
    if (conn) {
        LOG_INFO("Opened PG connection for %s", filename);
    }

    return rc;
}

int sqlite3_open_v2(const char *filename, sqlite3 **ppDb, int flags, const char *zVfs) {
    if (!orig_sqlite3_open_v2) shim_init();

    int rc = orig_sqlite3_open_v2(filename, ppDb, flags, zVfs);
    if (rc != SQLITE_OK || !should_redirect_db(filename)) {
        return rc;
    }

    pg_connection_t *conn = pg_get_thread_connection(filename);
    if (conn) {
        LOG_INFO("Opened PG connection (v2) for %s", filename);
    }

    return rc;
}

int sqlite3_close(sqlite3 *db) {
    if (!orig_sqlite3_close) shim_init();

    // Release pool connection if any
    pg_release_thread_connection(db);

    return orig_sqlite3_close(db);
}

int sqlite3_close_v2(sqlite3 *db) {
    if (!orig_sqlite3_close_v2) shim_init();

    pg_release_thread_connection(db);

    return orig_sqlite3_close_v2(db);
}

// ============================================================================
// Interposed Functions: Prepare/Step/Finalize
// ============================================================================

int sqlite3_prepare_v2(sqlite3 *db, const char *zSql, int nByte,
                       sqlite3_stmt **ppStmt, const char **pzTail) {
    if (!orig_sqlite3_prepare_v2) shim_init();

    // Always prepare via SQLite first (for fallback and handle)
    int rc = orig_sqlite3_prepare_v2(db, zSql, nByte, ppStmt, pzTail);
    if (rc != SQLITE_OK) return rc;

    // Check if we should use PostgreSQL
    pg_connection_t *conn = pg_get_thread_connection(NULL);
    if (!conn || !conn->is_pg_active) {
        return rc;
    }

    // Translate and prepare for PostgreSQL
    pg_stmt_t *stmt = pg_prepare_statement(conn, zSql, *ppStmt);
    if (stmt) {
        LOG_DEBUG("Prepared PG statement: %s", zSql);
    }

    return rc;
}

int sqlite3_prepare_v3(sqlite3 *db, const char *zSql, int nByte,
                       unsigned int prepFlags, sqlite3_stmt **ppStmt, const char **pzTail) {
    if (!orig_sqlite3_prepare_v3) shim_init();

    int rc = orig_sqlite3_prepare_v3(db, zSql, nByte, prepFlags, ppStmt, pzTail);
    if (rc != SQLITE_OK) return rc;

    pg_connection_t *conn = pg_get_thread_connection(NULL);
    if (!conn || !conn->is_pg_active) {
        return rc;
    }

    pg_stmt_t *stmt = pg_prepare_statement(conn, zSql, *ppStmt);
    if (stmt) {
        LOG_DEBUG("Prepared PG statement (v3): %s", zSql);
    }

    return rc;
}

int sqlite3_step(sqlite3_stmt *pStmt) {
    if (!orig_sqlite3_step) shim_init();

    // Find our PostgreSQL statement
    pg_stmt_t *stmt = pg_find_statement(pStmt);
    if (stmt && stmt->is_pg) {
        return pg_step(stmt);
    }

    // Fallback to SQLite
    return orig_sqlite3_step(pStmt);
}

int sqlite3_reset(sqlite3_stmt *pStmt) {
    if (!orig_sqlite3_reset) shim_init();

    pg_stmt_t *stmt = pg_find_statement(pStmt);
    if (stmt) {
        pg_reset(stmt);
    }

    return orig_sqlite3_reset(pStmt);
}

int sqlite3_finalize(sqlite3_stmt *pStmt) {
    if (!orig_sqlite3_finalize) shim_init();

    pg_stmt_t *stmt = pg_find_statement(pStmt);
    if (stmt) {
        pg_finalize(stmt);
    }

    return orig_sqlite3_finalize(pStmt);
}

// ============================================================================
// Interposed Functions: Bind Parameters
// ============================================================================

int sqlite3_bind_int(sqlite3_stmt *pStmt, int i, int iValue) {
    if (!orig_sqlite3_bind_int) shim_init();

    pg_stmt_t *stmt = pg_find_statement(pStmt);
    if (stmt && stmt->is_pg) {
        pg_bind_int(stmt, i, iValue);
    }

    return orig_sqlite3_bind_int(pStmt, i, iValue);
}

int sqlite3_bind_int64(sqlite3_stmt *pStmt, int i, sqlite3_int64 iValue) {
    if (!orig_sqlite3_bind_int64) shim_init();

    pg_stmt_t *stmt = pg_find_statement(pStmt);
    if (stmt && stmt->is_pg) {
        pg_bind_int64(stmt, i, iValue);
    }

    return orig_sqlite3_bind_int64(pStmt, i, iValue);
}

int sqlite3_bind_double(sqlite3_stmt *pStmt, int i, double rValue) {
    if (!orig_sqlite3_bind_double) shim_init();

    pg_stmt_t *stmt = pg_find_statement(pStmt);
    if (stmt && stmt->is_pg) {
        pg_bind_double(stmt, i, rValue);
    }

    return orig_sqlite3_bind_double(pStmt, i, rValue);
}

int sqlite3_bind_text(sqlite3_stmt *pStmt, int i, const char *zData,
                      int nData, void (*xDel)(void*)) {
    if (!orig_sqlite3_bind_text) shim_init();

    pg_stmt_t *stmt = pg_find_statement(pStmt);
    if (stmt && stmt->is_pg) {
        pg_bind_text(stmt, i, zData, nData, xDel);
    }

    return orig_sqlite3_bind_text(pStmt, i, zData, nData, xDel);
}

int sqlite3_bind_blob(sqlite3_stmt *pStmt, int i, const void *zData,
                      int nData, void (*xDel)(void*)) {
    if (!orig_sqlite3_bind_blob) shim_init();

    pg_stmt_t *stmt = pg_find_statement(pStmt);
    if (stmt && stmt->is_pg) {
        pg_bind_blob(stmt, i, zData, nData, xDel);
    }

    return orig_sqlite3_bind_blob(pStmt, i, zData, nData, xDel);
}

int sqlite3_bind_null(sqlite3_stmt *pStmt, int i) {
    if (!orig_sqlite3_bind_null) shim_init();

    pg_stmt_t *stmt = pg_find_statement(pStmt);
    if (stmt && stmt->is_pg) {
        pg_bind_null(stmt, i);
    }

    return orig_sqlite3_bind_null(pStmt, i);
}

// ============================================================================
// Interposed Functions: Column Access
// ============================================================================

int sqlite3_column_count(sqlite3_stmt *pStmt) {
    if (!orig_sqlite3_column_count) shim_init();

    pg_stmt_t *stmt = pg_find_statement(pStmt);
    if (stmt && stmt->is_pg && stmt->result) {
        return stmt->num_cols;
    }

    return orig_sqlite3_column_count(pStmt);
}

int sqlite3_column_type(sqlite3_stmt *pStmt, int iCol) {
    if (!orig_sqlite3_column_type) shim_init();

    pg_stmt_t *stmt = pg_find_statement(pStmt);
    if (stmt && stmt->is_pg) {
        return pg_column_type(stmt, iCol);
    }

    return orig_sqlite3_column_type(pStmt, iCol);
}

int sqlite3_column_int(sqlite3_stmt *pStmt, int iCol) {
    if (!orig_sqlite3_column_int) shim_init();

    pg_stmt_t *stmt = pg_find_statement(pStmt);
    if (stmt && stmt->is_pg) {
        return pg_column_int(stmt, iCol);
    }

    return orig_sqlite3_column_int(pStmt, iCol);
}

sqlite3_int64 sqlite3_column_int64(sqlite3_stmt *pStmt, int iCol) {
    if (!orig_sqlite3_column_int64) shim_init();

    pg_stmt_t *stmt = pg_find_statement(pStmt);
    if (stmt && stmt->is_pg) {
        return pg_column_int64(stmt, iCol);
    }

    return orig_sqlite3_column_int64(pStmt, iCol);
}

double sqlite3_column_double(sqlite3_stmt *pStmt, int iCol) {
    if (!orig_sqlite3_column_double) shim_init();

    pg_stmt_t *stmt = pg_find_statement(pStmt);
    if (stmt && stmt->is_pg) {
        return pg_column_double(stmt, iCol);
    }

    return orig_sqlite3_column_double(pStmt, iCol);
}

const unsigned char *sqlite3_column_text(sqlite3_stmt *pStmt, int iCol) {
    if (!orig_sqlite3_column_text) shim_init();

    pg_stmt_t *stmt = pg_find_statement(pStmt);
    if (stmt && stmt->is_pg) {
        return pg_column_text(stmt, iCol);
    }

    return orig_sqlite3_column_text(pStmt, iCol);
}

const void *sqlite3_column_blob(sqlite3_stmt *pStmt, int iCol) {
    if (!orig_sqlite3_column_blob) shim_init();

    pg_stmt_t *stmt = pg_find_statement(pStmt);
    if (stmt && stmt->is_pg) {
        return pg_column_blob(stmt, iCol);
    }

    return orig_sqlite3_column_blob(pStmt, iCol);
}

int sqlite3_column_bytes(sqlite3_stmt *pStmt, int iCol) {
    if (!orig_sqlite3_column_bytes) shim_init();

    pg_stmt_t *stmt = pg_find_statement(pStmt);
    if (stmt && stmt->is_pg) {
        return pg_column_bytes(stmt, iCol);
    }

    return orig_sqlite3_column_bytes(pStmt, iCol);
}

const char *sqlite3_column_name(sqlite3_stmt *pStmt, int iCol) {
    if (!orig_sqlite3_column_name) shim_init();

    pg_stmt_t *stmt = pg_find_statement(pStmt);
    if (stmt && stmt->is_pg && stmt->result) {
        return PQfname(stmt->result, iCol);
    }

    return orig_sqlite3_column_name(pStmt, iCol);
}

// ============================================================================
// Interposed Functions: sqlite3_value (fake value mechanism)
// ============================================================================

sqlite3_value *sqlite3_column_value(sqlite3_stmt *pStmt, int iCol) {
    if (!orig_sqlite3_column_value) shim_init();

    pg_stmt_t *stmt = pg_find_statement(pStmt);
    if (stmt && stmt->is_pg) {
        return pg_column_value(stmt, iCol);
    }

    return orig_sqlite3_column_value(pStmt, iCol);
}

int sqlite3_value_type(sqlite3_value *pVal) {
    if (!orig_sqlite3_value_type) shim_init();

    // Check if this is our fake value
    pg_value_t *pgval = (pg_value_t *)pVal;
    if (pgval && pgval->magic == PG_VALUE_MAGIC) {
        return pgval->type;
    }

    return orig_sqlite3_value_type(pVal);
}

int sqlite3_value_int(sqlite3_value *pVal) {
    if (!orig_sqlite3_value_int) shim_init();

    pg_value_t *pgval = (pg_value_t *)pVal;
    if (pgval && pgval->magic == PG_VALUE_MAGIC) {
        return pg_column_int(pgval->stmt, pgval->col_idx);
    }

    return orig_sqlite3_value_int(pVal);
}

sqlite3_int64 sqlite3_value_int64(sqlite3_value *pVal) {
    if (!orig_sqlite3_value_int64) shim_init();

    pg_value_t *pgval = (pg_value_t *)pVal;
    if (pgval && pgval->magic == PG_VALUE_MAGIC) {
        return pg_column_int64(pgval->stmt, pgval->col_idx);
    }

    return orig_sqlite3_value_int64(pVal);
}

double sqlite3_value_double(sqlite3_value *pVal) {
    if (!orig_sqlite3_value_double) shim_init();

    pg_value_t *pgval = (pg_value_t *)pVal;
    if (pgval && pgval->magic == PG_VALUE_MAGIC) {
        return pg_column_double(pgval->stmt, pgval->col_idx);
    }

    return orig_sqlite3_value_double(pVal);
}

const unsigned char *sqlite3_value_text(sqlite3_value *pVal) {
    if (!orig_sqlite3_value_text) shim_init();

    pg_value_t *pgval = (pg_value_t *)pVal;
    if (pgval && pgval->magic == PG_VALUE_MAGIC) {
        return pg_column_text(pgval->stmt, pgval->col_idx);
    }

    return orig_sqlite3_value_text(pVal);
}

int sqlite3_value_bytes(sqlite3_value *pVal) {
    if (!orig_sqlite3_value_bytes) shim_init();

    pg_value_t *pgval = (pg_value_t *)pVal;
    if (pgval && pgval->magic == PG_VALUE_MAGIC) {
        return pg_column_bytes(pgval->stmt, pgval->col_idx);
    }

    return orig_sqlite3_value_bytes(pVal);
}

const void *sqlite3_value_blob(sqlite3_value *pVal) {
    if (!orig_sqlite3_value_blob) shim_init();

    pg_value_t *pgval = (pg_value_t *)pVal;
    if (pgval && pgval->magic == PG_VALUE_MAGIC) {
        return pg_column_blob(pgval->stmt, pgval->col_idx);
    }

    return orig_sqlite3_value_blob(pVal);
}

// ============================================================================
// Interposed Functions: Other
// ============================================================================

sqlite3_int64 sqlite3_last_insert_rowid(sqlite3 *db) {
    if (!orig_sqlite3_last_insert_rowid) shim_init();

    pg_connection_t *conn = pg_get_thread_connection(NULL);
    if (conn && conn->is_pg_active) {
        return conn->last_insert_rowid;
    }

    return orig_sqlite3_last_insert_rowid(db);
}

int sqlite3_changes(sqlite3 *db) {
    if (!orig_sqlite3_changes) shim_init();

    pg_connection_t *conn = pg_get_thread_connection(NULL);
    if (conn && conn->is_pg_active) {
        return conn->last_changes;
    }

    return orig_sqlite3_changes(db);
}

const char *sqlite3_errmsg(sqlite3 *db) {
    if (!orig_sqlite3_errmsg) shim_init();

    pg_connection_t *conn = pg_get_thread_connection(NULL);
    if (conn && conn->is_pg_active && conn->last_error[0]) {
        return conn->last_error;
    }

    return orig_sqlite3_errmsg(db);
}

int sqlite3_errcode(sqlite3 *db) {
    if (!orig_sqlite3_errcode) shim_init();

    pg_connection_t *conn = pg_get_thread_connection(NULL);
    if (conn && conn->is_pg_active && conn->last_error_code) {
        return conn->last_error_code;
    }

    return orig_sqlite3_errcode(db);
}

// WAL functions - no-op for PostgreSQL
int sqlite3_wal_checkpoint_v2(sqlite3 *db, const char *zDb, int eMode,
                              int *pnLog, int *pnCkpt) {
    if (!orig_sqlite3_wal_checkpoint_v2) shim_init();

    pg_connection_t *conn = pg_get_thread_connection(NULL);
    if (conn && conn->is_pg_active) {
        if (pnLog) *pnLog = 0;
        if (pnCkpt) *pnCkpt = 0;
        return SQLITE_OK;
    }

    return orig_sqlite3_wal_checkpoint_v2(db, zDb, eMode, pnLog, pnCkpt);
}

int sqlite3_wal_autocheckpoint(sqlite3 *db, int N) {
    if (!orig_sqlite3_wal_autocheckpoint) shim_init();

    pg_connection_t *conn = pg_get_thread_connection(NULL);
    if (conn && conn->is_pg_active) {
        return SQLITE_OK;
    }

    return orig_sqlite3_wal_autocheckpoint(db, N);
}
