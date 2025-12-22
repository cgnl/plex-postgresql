#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <sqlite3.h>
#include <dlfcn.h>
#include <stdarg.h>
#include <mach-o/dyld.h>

#include "pg_types.h"
#include "pg_config.h"
#include "pg_logging.h"
#include "pg_client.h"
#include "pg_statement.h"
#include "fishhook.h"
#include "sql_translator.h"

// ============================================================================
// DYLD Interpose Definitions
// ============================================================================

#define DYLD_INTERPOSE(_replacement, _original) \
    __attribute__((used)) static struct { \
        const void* replacement; \
        const void* original; \
    } _interpose_##_original __attribute__((section("__DATA,__interpose"))) = { \
        (const void*)(unsigned long)&_replacement, \
        (const void*)(unsigned long)&_original \
    };

// ============================================================================
// Helpers & Original Function Pointers
// ============================================================================

static int (*orig_sqlite3_open)(const char*, sqlite3**) = NULL;
static int (*orig_sqlite3_open_v2)(const char*, sqlite3**, int, const char*) = NULL;
static int (*orig_sqlite3_close)(sqlite3*) = NULL;
static int (*orig_sqlite3_close_v2)(sqlite3*) = NULL;
static int (*orig_sqlite3_prepare)(sqlite3*, const char*, int, sqlite3_stmt**, const char**) = NULL;
static int (*orig_sqlite3_prepare_v2)(sqlite3*, const char*, int, sqlite3_stmt**, const char**) = NULL;
static int (*orig_sqlite3_prepare_v3)(sqlite3*, const char*, int, unsigned int, sqlite3_stmt**, const char**) = NULL;
static int (*orig_sqlite3_step)(sqlite3_stmt*) = NULL;
static int (*orig_sqlite3_reset)(sqlite3_stmt*) = NULL;
static int (*orig_sqlite3_finalize)(sqlite3_stmt*) = NULL;
static int (*orig_sqlite3_exec)(sqlite3*, const char*, int(*)(void*,int,char**,char**), void*, char**) = NULL;
static sqlite3_int64 (*orig_sqlite3_last_insert_rowid)(sqlite3*) = NULL;
static int (*orig_sqlite3_changes)(sqlite3*) = NULL;
static const char* (*orig_sqlite3_errmsg)(sqlite3*) = NULL;
static int (*orig_sqlite3_errcode)(sqlite3*) = NULL;
static int (*orig_sqlite3_column_count)(sqlite3_stmt*) = NULL;
static const char* (*orig_sqlite3_column_name)(sqlite3_stmt*, int) = NULL;
static int (*orig_sqlite3_column_type)(sqlite3_stmt*, int) = NULL;
static int (*orig_sqlite3_column_int)(sqlite3_stmt*, int) = NULL;
static sqlite3_int64 (*orig_sqlite3_column_int64)(sqlite3_stmt*, int) = NULL;
static const unsigned char* (*orig_sqlite3_column_text)(sqlite3_stmt*, int) = NULL;
static const void* (*orig_sqlite3_column_blob)(sqlite3_stmt*, int) = NULL;
static double (*orig_sqlite3_column_double)(sqlite3_stmt*, int) = NULL;
static int (*orig_sqlite3_column_bytes)(sqlite3_stmt*, int) = NULL;
static int (*orig_sqlite3_bind_int)(sqlite3_stmt*, int, int) = NULL;
static int (*orig_sqlite3_bind_int64)(sqlite3_stmt*, int, sqlite3_int64) = NULL;
static int (*orig_sqlite3_bind_text)(sqlite3_stmt*, int, const char*, int, void(*)(void*)) = NULL;
static int (*orig_sqlite3_bind_double)(sqlite3_stmt*, int, double) = NULL;
static int (*orig_sqlite3_bind_blob)(sqlite3_stmt*, int, const void*, int, void(*)(void*)) = NULL;
static int (*orig_sqlite3_bind_null)(sqlite3_stmt*, int) = NULL;
static int (*orig_sqlite3_bind_parameter_count)(sqlite3_stmt*) = NULL;
static int (*orig_sqlite3_stmt_readonly)(sqlite3_stmt*) = NULL;
static const char* (*orig_sqlite3_column_decltype)(sqlite3_stmt*, int) = NULL;
static int (*orig_sqlite3_clear_bindings)(sqlite3_stmt*) = NULL;
static int (*orig_sqlite3_wal_checkpoint)(sqlite3*, const char*) = NULL;
static int (*orig_sqlite3_wal_checkpoint_v2)(sqlite3*, const char*, int, int*, int*) = NULL;
static int (*orig_sqlite3_table_column_metadata)(sqlite3*, const char*, const char*, const char*, const char**, const char**, int*, int*, int*) = NULL;

static void resolve_all_symbols() {
    struct rebinding rebs[] = {
        {"sqlite3_open", NULL, (void**)&orig_sqlite3_open},
        {"sqlite3_open_v2", NULL, (void**)&orig_sqlite3_open_v2},
        {"sqlite3_close", NULL, (void**)&orig_sqlite3_close},
        {"sqlite3_close_v2", NULL, (void**)&orig_sqlite3_close_v2},
        {"sqlite3_prepare", NULL, (void**)&orig_sqlite3_prepare},
        {"sqlite3_prepare_v2", NULL, (void**)&orig_sqlite3_prepare_v2},
        {"sqlite3_prepare_v3", NULL, (void**)&orig_sqlite3_prepare_v3},
        {"sqlite3_step", NULL, (void**)&orig_sqlite3_step},
        {"sqlite3_reset", NULL, (void**)&orig_sqlite3_reset},
        {"sqlite3_finalize", NULL, (void**)&orig_sqlite3_finalize},
        {"sqlite3_exec", NULL, (void**)&orig_sqlite3_exec},
        {"sqlite3_last_insert_rowid", NULL, (void**)&orig_sqlite3_last_insert_rowid},
        {"sqlite3_changes", NULL, (void**)&orig_sqlite3_changes},
        {"sqlite3_errmsg", NULL, (void**)&orig_sqlite3_errmsg},
        {"sqlite3_errcode", NULL, (void**)&orig_sqlite3_errcode},
        {"sqlite3_column_count", NULL, (void**)&orig_sqlite3_column_count},
        {"sqlite3_column_name", NULL, (void**)&orig_sqlite3_column_name},
        {"sqlite3_column_type", NULL, (void**)&orig_sqlite3_column_type},
        {"sqlite3_column_int", NULL, (void**)&orig_sqlite3_column_int},
        {"sqlite3_column_int64", NULL, (void**)&orig_sqlite3_column_int64},
        {"sqlite3_column_text", NULL, (void**)&orig_sqlite3_column_text},
        {"sqlite3_column_blob", NULL, (void**)&orig_sqlite3_column_blob},
        {"sqlite3_column_double", NULL, (void**)&orig_sqlite3_column_double},
        {"sqlite3_column_bytes", NULL, (void**)&orig_sqlite3_column_bytes},
        {"sqlite3_bind_int", NULL, (void**)&orig_sqlite3_bind_int},
        {"sqlite3_bind_int64", NULL, (void**)&orig_sqlite3_bind_int64},
        {"sqlite3_bind_text", NULL, (void**)&orig_sqlite3_bind_text},
        {"sqlite3_bind_double", NULL, (void**)&orig_sqlite3_bind_double},
        {"sqlite3_bind_blob", NULL, (void**)&orig_sqlite3_bind_blob},
        {"sqlite3_bind_null", NULL, (void**)&orig_sqlite3_bind_null},
        {"sqlite3_bind_parameter_count", NULL, (void**)&orig_sqlite3_bind_parameter_count},
        {"sqlite3_stmt_readonly", NULL, (void**)&orig_sqlite3_stmt_readonly},
        {"sqlite3_column_decltype", NULL, (void**)&orig_sqlite3_column_decltype},
        {"sqlite3_clear_bindings", NULL, (void**)&orig_sqlite3_clear_bindings},
        {"sqlite3_wal_checkpoint", NULL, (void**)&orig_sqlite3_wal_checkpoint},
        {"sqlite3_wal_checkpoint_v2", NULL, (void**)&orig_sqlite3_wal_checkpoint_v2},
        {"sqlite3_table_column_metadata", NULL, (void**)&orig_sqlite3_table_column_metadata}
    };
    // Fishhook finds the original implementations in the lazy/non-lazy jump tables
    rebind_symbols(rebs, sizeof(rebs)/sizeof(struct rebinding));
    LOG_INFO("Resolved all symbols via fishhook");
}

// ============================================================================
// Core Interposers
// ============================================================================

static int my_sqlite3_open_v2(const char *filename, sqlite3 **ppDb, int flags, const char *zVfs) {
    int should_redir = should_redirect(filename);
    if (should_redir) LOG_INFO("Redirecting DB: %s", filename);
    
    if (!orig_sqlite3_open_v2) return SQLITE_ERROR;
    int rc = orig_sqlite3_open_v2(filename, ppDb, flags, zVfs);
    
    if (rc == SQLITE_OK && should_redir) {
        sqlite3 *db = *ppDb;
        pg_connection_t *conn = calloc(1, sizeof(pg_connection_t));
        if (conn) {
            strncpy(conn->db_path, filename ? filename : "", sizeof(conn->db_path) - 1);
            conn->is_pg_active = 1;
            pthread_mutex_init(&conn->mutex, NULL);
            if (ensure_pg_connection(conn)) {
                pg_register_connection(db, conn);
            } else {
                LOG_ERROR("Failed to connect PG for %s", filename);
                free(conn);
            }
        }
    }
    return rc;
}

static int my_sqlite3_open(const char *filename, sqlite3 **ppDb) {
    return my_sqlite3_open_v2(filename, ppDb, SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE, NULL);
}

static int my_sqlite3_close(sqlite3 *db) {
    pg_connection_t *conn = find_pg_connection(db);
    if (conn) {
        pg_unregister_connection(db);
        pg_close(conn);
    }
    if (!orig_sqlite3_close) return SQLITE_OK;
    return orig_sqlite3_close(db);
}

static int my_sqlite3_close_v2(sqlite3 *db) {
    return my_sqlite3_close(db); // close_v2 is similar enough for us
}

static int my_sqlite3_prepare_v2(sqlite3 *db, const char *zSql, int nByte, sqlite3_stmt **ppStmt, const char **pzTail) {
    pg_connection_t *conn = find_pg_connection(db);
    if (conn && conn->is_pg_active) {
        LOG_INFO("PREPARE: %s", zSql);
        if (!should_skip_sql(zSql)) {
            char *sql_str = NULL;
            if (nByte < 0) sql_str = strdup(zSql);
            else { sql_str = malloc(nByte + 1); memcpy(sql_str, zSql, nByte); sql_str[nByte] = '\0'; }
            
            pg_stmt_t *stmt = pg_prepare(conn, sql_str, NULL);
            free(sql_str);
            if (stmt) {
                *ppStmt = (sqlite3_stmt*)stmt;
                if (pzTail) *pzTail = zSql + strlen(zSql);
                LOG_INFO("PREPARE -> PG SUCCESS");
                return SQLITE_OK;
            }
            LOG_ERROR("PREPARE -> PG FAILED: %s", conn->last_error);
            return SQLITE_ERROR;
        }
        LOG_INFO("PREPARE -> SKIP to SQLite");
    }
    
    if (!orig_sqlite3_prepare_v2) return SQLITE_ERROR;
    return orig_sqlite3_prepare_v2(db, zSql, nByte, ppStmt, pzTail);
}

static int my_sqlite3_prepare(sqlite3 *db, const char *zSql, int nByte, sqlite3_stmt **ppStmt, const char **pzTail) {
    return my_sqlite3_prepare_v2(db, zSql, nByte, ppStmt, pzTail);
}

static int my_sqlite3_prepare_v3(sqlite3 *db, const char *zSql, int nByte, unsigned int prepFlags, sqlite3_stmt **ppStmt, const char **pzTail) {
    return my_sqlite3_prepare_v2(db, zSql, nByte, ppStmt, pzTail);
}

static int my_sqlite3_prepare16_v2(sqlite3 *db, const void *zSql, int nByte, sqlite3_stmt **ppStmt, const void **pzTail) {
    return SQLITE_ERROR;
}

static int my_sqlite3_step(sqlite3_stmt *pStmt) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_step((pg_stmt_t*)pStmt);
    if (!orig_sqlite3_step) return SQLITE_ERROR;
    return orig_sqlite3_step(pStmt);
}

static int my_sqlite3_reset(sqlite3_stmt *pStmt) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_reset((pg_stmt_t*)pStmt);
    if (!orig_sqlite3_reset) return SQLITE_OK;
    return orig_sqlite3_reset(pStmt);
}

static int my_sqlite3_finalize(sqlite3_stmt *pStmt) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_finalize((pg_stmt_t*)pStmt);
    if (!orig_sqlite3_finalize) return SQLITE_OK;
    return orig_sqlite3_finalize(pStmt);
}

static int my_sqlite3_exec(sqlite3 *db, const char *sql, int (*callback)(void*,int,char**,char**), void *arg, char **errmsg) {
    pg_connection_t *conn = find_pg_connection(db);
    if (conn && conn->is_pg_active && !should_skip_sql(sql)) {
         LOG_INFO("EXEC: %s", sql);
         pg_stmt_t *stmt = pg_prepare(conn, sql, NULL);
         if (!stmt) { if(errmsg) *errmsg=strdup("PG Prepare Fail"); return SQLITE_ERROR; }
         int rc;
         while((rc = pg_step(stmt)) == SQLITE_ROW);
         pg_finalize(stmt);
         return SQLITE_OK;
    }
    if (!orig_sqlite3_exec) return SQLITE_ERROR;
    return orig_sqlite3_exec(db, sql, callback, arg, errmsg);
}

static sqlite3_int64 my_sqlite3_last_insert_rowid(sqlite3 *db) {
    pg_connection_t *conn = find_pg_connection(db);
    if (conn && conn->is_pg_active) return conn->last_insert_rowid;
    if (!orig_sqlite3_last_insert_rowid) return 0;
    return orig_sqlite3_last_insert_rowid(db);
}

static int my_sqlite3_changes(sqlite3 *db) {
    pg_connection_t *conn = find_pg_connection(db);
    if (conn && conn->is_pg_active) return conn->last_changes;
    if (!orig_sqlite3_changes) return 0;
    return orig_sqlite3_changes(db);
}

static const char *my_sqlite3_errmsg(sqlite3 *db) {
    pg_connection_t *conn = find_pg_connection(db);
    if (conn && conn->is_pg_active) return conn->last_error;
    if (orig_sqlite3_errmsg) return orig_sqlite3_errmsg(db);
    return "Shim Error: original errmsg not found";
}

static int my_sqlite3_errcode(sqlite3 *db) {
    pg_connection_t *conn = find_pg_connection(db);
    if (conn && conn->is_pg_active) return conn->last_error_code;
    return orig_sqlite3_errcode ? orig_sqlite3_errcode(db) : SQLITE_ERROR;
}

static int my_sqlite3_bind_parameter_count(sqlite3_stmt *pStmt) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return ((pg_stmt_t*)pStmt)->param_count;
    return orig_sqlite3_bind_parameter_count ? orig_sqlite3_bind_parameter_count(pStmt) : 0;
}

static int my_sqlite3_stmt_readonly(sqlite3_stmt *pStmt) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return 0; // Assume RW for safety
    return orig_sqlite3_stmt_readonly ? orig_sqlite3_stmt_readonly(pStmt) : 0;
}

static const char* my_sqlite3_column_decltype(sqlite3_stmt *pStmt, int idx) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return "text";
    return orig_sqlite3_column_decltype ? orig_sqlite3_column_decltype(pStmt, idx) : NULL;
}

// Column Accessors
static int my_sqlite3_column_count(sqlite3_stmt *pStmt) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_column_count((pg_stmt_t*)pStmt);
    return orig_sqlite3_column_count ? orig_sqlite3_column_count(pStmt) : 0;
}

static const char* my_sqlite3_column_name(sqlite3_stmt *pStmt, int idx) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_column_name((pg_stmt_t*)pStmt, idx);
    return orig_sqlite3_column_name ? orig_sqlite3_column_name(pStmt, idx) : NULL;
}

static int my_sqlite3_column_type(sqlite3_stmt *pStmt, int idx) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_column_type((pg_stmt_t*)pStmt, idx);
    return orig_sqlite3_column_type ? orig_sqlite3_column_type(pStmt, idx) : SQLITE_NULL;
}

static int my_sqlite3_column_int(sqlite3_stmt *pStmt, int idx) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_column_int((pg_stmt_t*)pStmt, idx);
    return orig_sqlite3_column_int ? orig_sqlite3_column_int(pStmt, idx) : 0;
}

static sqlite3_int64 my_sqlite3_column_int64(sqlite3_stmt *pStmt, int idx) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_column_int64((pg_stmt_t*)pStmt, idx);
    return orig_sqlite3_column_int64 ? orig_sqlite3_column_int64(pStmt, idx) : 0;
}

static const unsigned char* my_sqlite3_column_text(sqlite3_stmt *pStmt, int idx) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_column_text((pg_stmt_t*)pStmt, idx);
    return orig_sqlite3_column_text ? orig_sqlite3_column_text(pStmt, idx) : NULL;
}

static const void* my_sqlite3_column_blob(sqlite3_stmt *pStmt, int idx) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_column_blob((pg_stmt_t*)pStmt, idx);
    return orig_sqlite3_column_blob ? orig_sqlite3_column_blob(pStmt, idx) : NULL;
}

static double my_sqlite3_column_double(sqlite3_stmt *pStmt, int idx) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_column_double((pg_stmt_t*)pStmt, idx);
    return orig_sqlite3_column_double ? orig_sqlite3_column_double(pStmt, idx) : 0.0;
}

static int my_sqlite3_column_bytes(sqlite3_stmt *pStmt, int idx) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_column_bytes((pg_stmt_t*)pStmt, idx);
    return orig_sqlite3_column_bytes ? orig_sqlite3_column_bytes(pStmt, idx) : 0;
}

static int my_sqlite3_bind_int(sqlite3_stmt *pStmt, int idx, int val) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_bind_int((pg_stmt_t*)pStmt, idx, val);
    return orig_sqlite3_bind_int ? orig_sqlite3_bind_int(pStmt, idx, val) : SQLITE_ERROR;
}

static int my_sqlite3_bind_int64(sqlite3_stmt *pStmt, int idx, sqlite3_int64 val) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_bind_int64((pg_stmt_t*)pStmt, idx, val);
    return orig_sqlite3_bind_int64 ? orig_sqlite3_bind_int64(pStmt, idx, val) : SQLITE_ERROR;
}

static int my_sqlite3_bind_text(sqlite3_stmt *pStmt, int idx, const char *val, int len, void(*destructor)(void*)) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_bind_text((pg_stmt_t*)pStmt, idx, val, len, destructor);
    return orig_sqlite3_bind_text ? orig_sqlite3_bind_text(pStmt, idx, val, len, destructor) : SQLITE_ERROR;
}

static int my_sqlite3_bind_double(sqlite3_stmt *pStmt, int idx, double val) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_bind_double((pg_stmt_t*)pStmt, idx, val);
    return orig_sqlite3_bind_double ? orig_sqlite3_bind_double(pStmt, idx, val) : SQLITE_ERROR;
}

static int my_sqlite3_bind_blob(sqlite3_stmt *pStmt, int idx, const void *val, int len, void(*destructor)(void*)) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_bind_blob((pg_stmt_t*)pStmt, idx, val, len, destructor);
    return orig_sqlite3_bind_blob ? orig_sqlite3_bind_blob(pStmt, idx, val, len, destructor) : SQLITE_ERROR;
}

static int my_sqlite3_bind_null(sqlite3_stmt *pStmt, int idx) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_bind_null((pg_stmt_t*)pStmt, idx);
    return orig_sqlite3_bind_null ? orig_sqlite3_bind_null(pStmt, idx) : SQLITE_ERROR;
}

static int my_sqlite3_clear_bindings(sqlite3_stmt *pStmt) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return SQLITE_OK;
    return orig_sqlite3_clear_bindings ? orig_sqlite3_clear_bindings(pStmt) : SQLITE_OK;
}

static int my_sqlite3_wal_checkpoint(sqlite3 *db, const char *zDb) {
    if (!orig_sqlite3_wal_checkpoint) return SQLITE_OK;
    return orig_sqlite3_wal_checkpoint(db, zDb);
}

static int my_sqlite3_wal_checkpoint_v2(sqlite3 *db, const char *zDb, int eMode, int *pnLog, int *pnCkpt) {
    if (!orig_sqlite3_wal_checkpoint_v2) return SQLITE_OK;
    return orig_sqlite3_wal_checkpoint_v2(db, zDb, eMode, pnLog, pnCkpt);
}

static int my_sqlite3_table_column_metadata(sqlite3 *db, const char *zDbName, const char *zTableName, const char *zColumnName, const char **pzDataType, const char **pzCollSeq, int *pNotNull, int *pPrimaryKey, int *pAutoinc) {
    if (!orig_sqlite3_table_column_metadata) return SQLITE_ERROR;
    return orig_sqlite3_table_column_metadata(db, zDbName, zTableName, zColumnName, pzDataType, pzCollSeq, pNotNull, pPrimaryKey, pAutoinc);
}

// ============================================================================
// Interpose Map
// ============================================================================

DYLD_INTERPOSE(my_sqlite3_open, sqlite3_open)
DYLD_INTERPOSE(my_sqlite3_open_v2, sqlite3_open_v2)
DYLD_INTERPOSE(my_sqlite3_close, sqlite3_close)
DYLD_INTERPOSE(my_sqlite3_close_v2, sqlite3_close_v2)
DYLD_INTERPOSE(my_sqlite3_prepare, sqlite3_prepare)
DYLD_INTERPOSE(my_sqlite3_prepare_v2, sqlite3_prepare_v2)
DYLD_INTERPOSE(my_sqlite3_prepare_v3, sqlite3_prepare_v3)
DYLD_INTERPOSE(my_sqlite3_prepare16_v2, sqlite3_prepare16_v2)
DYLD_INTERPOSE(my_sqlite3_step, sqlite3_step)
DYLD_INTERPOSE(my_sqlite3_reset, sqlite3_reset)
DYLD_INTERPOSE(my_sqlite3_finalize, sqlite3_finalize)
DYLD_INTERPOSE(my_sqlite3_exec, sqlite3_exec)
DYLD_INTERPOSE(my_sqlite3_last_insert_rowid, sqlite3_last_insert_rowid)
DYLD_INTERPOSE(my_sqlite3_changes, sqlite3_changes)
DYLD_INTERPOSE(my_sqlite3_errmsg, sqlite3_errmsg)
DYLD_INTERPOSE(my_sqlite3_errcode, sqlite3_errcode)
DYLD_INTERPOSE(my_sqlite3_column_count, sqlite3_column_count)
DYLD_INTERPOSE(my_sqlite3_column_name, sqlite3_column_name)
DYLD_INTERPOSE(my_sqlite3_column_type, sqlite3_column_type)
DYLD_INTERPOSE(my_sqlite3_column_int, sqlite3_column_int)
DYLD_INTERPOSE(my_sqlite3_column_int64, sqlite3_column_int64)
DYLD_INTERPOSE(my_sqlite3_column_text, sqlite3_column_text)
DYLD_INTERPOSE(my_sqlite3_column_blob, sqlite3_column_blob)
DYLD_INTERPOSE(my_sqlite3_column_double, sqlite3_column_double)
DYLD_INTERPOSE(my_sqlite3_column_bytes, sqlite3_column_bytes)
DYLD_INTERPOSE(my_sqlite3_bind_int, sqlite3_bind_int)
DYLD_INTERPOSE(my_sqlite3_bind_int64, sqlite3_bind_int64)
DYLD_INTERPOSE(my_sqlite3_bind_text, sqlite3_bind_text)
DYLD_INTERPOSE(my_sqlite3_bind_double, sqlite3_bind_double)
DYLD_INTERPOSE(my_sqlite3_bind_blob, sqlite3_bind_blob)
DYLD_INTERPOSE(my_sqlite3_bind_null, sqlite3_bind_null)
DYLD_INTERPOSE(my_sqlite3_bind_parameter_count, sqlite3_bind_parameter_count)
DYLD_INTERPOSE(my_sqlite3_stmt_readonly, sqlite3_stmt_readonly)
DYLD_INTERPOSE(my_sqlite3_column_decltype, sqlite3_column_decltype)
DYLD_INTERPOSE(my_sqlite3_clear_bindings, sqlite3_clear_bindings)
DYLD_INTERPOSE(my_sqlite3_wal_checkpoint, sqlite3_wal_checkpoint)
DYLD_INTERPOSE(my_sqlite3_wal_checkpoint_v2, sqlite3_wal_checkpoint_v2)
DYLD_INTERPOSE(my_sqlite3_table_column_metadata, sqlite3_table_column_metadata)

__attribute__((constructor))
static void shim_init(void) {
    fprintf(stderr, "!!!!! SHIM INIT RUNNING !!!!!\n");
    init_logging();
    load_config();
    resolve_all_symbols(); // Must be called after init_logging
    LOG_INFO("Shim initialized.");
}
