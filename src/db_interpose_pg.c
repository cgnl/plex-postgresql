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
// Robust Symbol Resolution
// ============================================================================

static void* resolve_sqlite_symbol(const char *symbol) {
    // 1. Prioritize Plex bundled library for ABI compatibility
    const char *bundled_path = "/Applications/Plex Media Server.app/Contents/Frameworks/libsqlite3.dylib";
    void *bundled = dlopen(bundled_path, RTLD_LAZY | RTLD_NOLOAD);
    if (bundled) {
        void *s = dlsym(bundled, symbol);
        if (s) {
            Dl_info info;
            if (dladdr(s, &info) && !strstr(info.dli_fname, "db_interpose_pg.dylib")) {
                dlclose(bundled);
                return s;
            }
        }
        dlclose(bundled);
    }

    // 2. Fallback to RTLD_NEXT
    void *sym = dlsym(RTLD_NEXT, symbol);
    if (sym) {
        Dl_info info;
        if (dladdr(sym, &info) && !strstr(info.dli_fname, "db_interpose_pg.dylib")) {
            return sym;
        }
    }
    
    // 3. Last resort: Search all images but skip ourselves
    uint32_t count = _dyld_image_count();
    for (uint32_t i = 0; i < count; i++) {
        const char *img_name = _dyld_get_image_name(i);
        if (strstr(img_name, "db_interpose_pg.dylib")) continue;
        
        void *h = dlopen(img_name, RTLD_LAZY | RTLD_NOLOAD);
        if (h) {
            void *s = dlsym(h, symbol);
            if (s) {
                Dl_info info;
                if (dladdr(s, &info) && !strstr(info.dli_fname, "db_interpose_pg.dylib")) {
                    LOG_INFO("Found %s in %s at %p", symbol, info.dli_fname, s);
                    dlclose(h);
                    return s;
                }
            }
            dlclose(h);
        }
    }
    
    LOG_ERROR("CRITICAL: Failed to resolve symbol %s", symbol);
    return NULL;
}

// ============================================================================
// Core Interposers
// ============================================================================

static int my_sqlite3_open_v2(const char *filename, sqlite3 **ppDb, int flags, const char *zVfs) {
    int should_redir = should_redirect(filename);
    if (should_redir) LOG_INFO("Redirecting DB: %s", filename);
    
    static int (*orig)(const char*, sqlite3**, int, const char*) = NULL;
    if (!orig) orig = (int(*)(const char*, sqlite3**, int, const char*)) resolve_sqlite_symbol("sqlite3_open_v2");
    
    if (!orig) return SQLITE_ERROR;
    int rc = orig(filename, ppDb, flags, zVfs);
    
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
    static int (*orig)(sqlite3*) = NULL;
    if (!orig) orig = (int(*)(sqlite3*)) resolve_sqlite_symbol("sqlite3_close");
    return orig ? orig(db) : SQLITE_OK;
}

static int my_sqlite3_close_v2(sqlite3 *db) {
    return my_sqlite3_close(db);
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
    
    static int (*orig)(sqlite3*, const char*, int, sqlite3_stmt**, const char**) = NULL;
    if (!orig) orig = (int(*)(sqlite3*, const char*, int, sqlite3_stmt**, const char**)) resolve_sqlite_symbol("sqlite3_prepare_v2");
    return orig ? orig(db, zSql, nByte, ppStmt, pzTail) : SQLITE_ERROR;
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
    static int (*orig)(sqlite3_stmt*) = NULL;
    if (!orig) orig = (int(*)(sqlite3_stmt*)) resolve_sqlite_symbol("sqlite3_step");
    return orig ? orig(pStmt) : SQLITE_ERROR;
}

static int my_sqlite3_reset(sqlite3_stmt *pStmt) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_reset((pg_stmt_t*)pStmt);
    static int (*orig)(sqlite3_stmt*) = NULL;
    if (!orig) orig = (int(*)(sqlite3_stmt*)) resolve_sqlite_symbol("sqlite3_reset");
    return orig ? orig(pStmt) : SQLITE_OK;
}

static int my_sqlite3_finalize(sqlite3_stmt *pStmt) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_finalize((pg_stmt_t*)pStmt);
    static int (*orig)(sqlite3_stmt*) = NULL;
    if (!orig) orig = (int(*)(sqlite3_stmt*)) resolve_sqlite_symbol("sqlite3_finalize");
    return orig ? orig(pStmt) : SQLITE_OK;
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
    static int (*orig)(sqlite3*, const char*, int(*)(void*,int,char**,char**), void*, char**) = NULL;
    if (!orig) orig = (int(*)(sqlite3*, const char*, int(*)(void*,int,char**,char**), void*, char**)) resolve_sqlite_symbol("sqlite3_exec");
    return orig ? orig(db, sql, callback, arg, errmsg) : SQLITE_ERROR;
}

static sqlite3_int64 my_sqlite3_last_insert_rowid(sqlite3 *db) {
    pg_connection_t *conn = find_pg_connection(db);
    if (conn && conn->is_pg_active) return conn->last_insert_rowid;
    static sqlite3_int64 (*orig)(sqlite3*) = NULL;
    if (!orig) orig = (sqlite3_int64(*)(sqlite3*)) resolve_sqlite_symbol("sqlite3_last_insert_rowid");
    return orig ? orig(db) : 0;
}

static int my_sqlite3_changes(sqlite3 *db) {
    pg_connection_t *conn = find_pg_connection(db);
    if (conn && conn->is_pg_active) return conn->last_changes;
    static int (*orig)(sqlite3*) = NULL;
    if (!orig) orig = (int(*)(sqlite3*)) resolve_sqlite_symbol("sqlite3_changes");
    return orig ? orig(db) : 0;
}

static const char *my_sqlite3_errmsg(sqlite3 *db) {
    pg_connection_t *conn = find_pg_connection(db);
    if (conn && conn->is_pg_active) return conn->last_error;
    static const char* (*orig)(sqlite3*) = NULL;
    if (!orig) orig = (const char*(*)(sqlite3*)) resolve_sqlite_symbol("sqlite3_errmsg");
    if (orig) return orig(db);
    return "Shim Error: original errmsg not found";
}

static int my_sqlite3_errcode(sqlite3 *db) {
    pg_connection_t *conn = find_pg_connection(db);
    if (conn && conn->is_pg_active) return conn->last_error_code;
    static int (*orig)(sqlite3*) = NULL;
    if (!orig) orig = (int(*)(sqlite3*)) resolve_sqlite_symbol("sqlite3_errcode");
    return orig ? orig(db) : SQLITE_ERROR;
}

static int my_sqlite3_bind_parameter_count(sqlite3_stmt *pStmt) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return ((pg_stmt_t*)pStmt)->param_count;
    static int (*orig)(sqlite3_stmt*) = NULL;
    if (!orig) orig = (int(*)(sqlite3_stmt*)) resolve_sqlite_symbol("sqlite3_bind_parameter_count");
    return orig ? orig(pStmt) : 0;
}

static int my_sqlite3_stmt_readonly(sqlite3_stmt *pStmt) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return 0; // Assume RW for safety
    static int (*orig)(sqlite3_stmt*) = NULL;
    if (!orig) orig = (int(*)(sqlite3_stmt*)) resolve_sqlite_symbol("sqlite3_stmt_readonly");
    return orig ? orig(pStmt) : 0;
}

static const char* my_sqlite3_column_decltype(sqlite3_stmt *pStmt, int idx) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return "text";
    static const char* (*orig)(sqlite3_stmt*, int) = NULL;
    if (!orig) orig = (const char*(*)(sqlite3_stmt*, int)) resolve_sqlite_symbol("sqlite3_column_decltype");
    return orig ? orig(pStmt, idx) : NULL;
}

// Column Accessors
static int my_sqlite3_column_count(sqlite3_stmt *pStmt) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_column_count((pg_stmt_t*)pStmt);
    static int (*orig)(sqlite3_stmt*) = NULL;
    if (!orig) orig = (int(*)(sqlite3_stmt*)) resolve_sqlite_symbol("sqlite3_column_count");
    return orig ? orig(pStmt) : 0;
}

static const char* my_sqlite3_column_name(sqlite3_stmt *pStmt, int idx) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_column_name((pg_stmt_t*)pStmt, idx);
    static const char* (*orig)(sqlite3_stmt*, int) = NULL;
    if (!orig) orig = (const char*(*)(sqlite3_stmt*, int)) resolve_sqlite_symbol("sqlite3_column_name");
    return orig ? orig(pStmt, idx) : NULL;
}

static int my_sqlite3_column_type(sqlite3_stmt *pStmt, int idx) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_column_type((pg_stmt_t*)pStmt, idx);
    static int (*orig)(sqlite3_stmt*, int) = NULL;
    if (!orig) orig = (int(*)(sqlite3_stmt*, int)) resolve_sqlite_symbol("sqlite3_column_type");
    return orig ? orig(pStmt, idx) : SQLITE_NULL;
}

static int my_sqlite3_column_int(sqlite3_stmt *pStmt, int idx) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_column_int((pg_stmt_t*)pStmt, idx);
    static int (*orig)(sqlite3_stmt*, int) = NULL;
    if (!orig) orig = (int(*)(sqlite3_stmt*, int)) resolve_sqlite_symbol("sqlite3_column_int");
    return orig ? orig(pStmt, idx) : 0;
}

static sqlite3_int64 my_sqlite3_column_int64(sqlite3_stmt *pStmt, int idx) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_column_int64((pg_stmt_t*)pStmt, idx);
    static sqlite3_int64 (*orig)(sqlite3_stmt*, int) = NULL;
    if (!orig) orig = (sqlite3_int64(*)(sqlite3_stmt*, int)) resolve_sqlite_symbol("sqlite3_column_int64");
    return orig ? orig(pStmt, idx) : 0;
}

static const unsigned char* my_sqlite3_column_text(sqlite3_stmt *pStmt, int idx) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_column_text((pg_stmt_t*)pStmt, idx);
    static const unsigned char* (*orig)(sqlite3_stmt*, int) = NULL;
    if (!orig) orig = (const unsigned char*(*)(sqlite3_stmt*, int)) resolve_sqlite_symbol("sqlite3_column_text");
    return orig ? orig(pStmt, idx) : NULL;
}

static const void* my_sqlite3_column_blob(sqlite3_stmt *pStmt, int idx) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_column_blob((pg_stmt_t*)pStmt, idx);
    static const void* (*orig)(sqlite3_stmt*, int) = NULL;
    if (!orig) orig = (const void*(*)(sqlite3_stmt*, int)) resolve_sqlite_symbol("sqlite3_column_blob");
    return orig ? orig(pStmt, idx) : NULL;
}

static double my_sqlite3_column_double(sqlite3_stmt *pStmt, int idx) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_column_double((pg_stmt_t*)pStmt, idx);
    static double (*orig)(sqlite3_stmt*, int) = NULL;
    if (!orig) orig = (double(*)(sqlite3_stmt*, int)) resolve_sqlite_symbol("sqlite3_column_double");
    return orig ? orig(pStmt, idx) : 0.0;
}

static int my_sqlite3_column_bytes(sqlite3_stmt *pStmt, int idx) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_column_bytes((pg_stmt_t*)pStmt, idx);
    static int (*orig)(sqlite3_stmt*, int) = NULL;
    if (!orig) orig = (int(*)(sqlite3_stmt*, int)) resolve_sqlite_symbol("sqlite3_column_bytes");
    return orig ? orig(pStmt, idx) : 0;
}

// Bindings
static int my_sqlite3_bind_int(sqlite3_stmt *pStmt, int idx, int val) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_bind_int((pg_stmt_t*)pStmt, idx, val);
    static int (*orig)(sqlite3_stmt*, int, int) = NULL;
    if (!orig) orig = (int(*)(sqlite3_stmt*, int, int)) resolve_sqlite_symbol("sqlite3_bind_int");
    return orig ? orig(pStmt, idx, val) : SQLITE_ERROR;
}

static int my_sqlite3_bind_int64(sqlite3_stmt *pStmt, int idx, sqlite3_int64 val) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_bind_int64((pg_stmt_t*)pStmt, idx, val);
    static int (*orig)(sqlite3_stmt*, int, sqlite3_int64) = NULL;
    if (!orig) orig = (int(*)(sqlite3_stmt*, int, sqlite3_int64)) resolve_sqlite_symbol("sqlite3_bind_int64");
    return orig ? orig(pStmt, idx, val) : SQLITE_ERROR;
}

static int my_sqlite3_bind_text(sqlite3_stmt *pStmt, int idx, const char *val, int len, void(*destructor)(void*)) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_bind_text((pg_stmt_t*)pStmt, idx, val, len, destructor);
    static int (*orig)(sqlite3_stmt*, int, const char*, int, void(*)(void*)) = NULL;
    if (!orig) orig = (int(*)(sqlite3_stmt*, int, const char*, int, void(*)(void*))) resolve_sqlite_symbol("sqlite3_bind_text");
    return orig ? orig(pStmt, idx, val, len, destructor) : SQLITE_ERROR;
}

static int my_sqlite3_bind_double(sqlite3_stmt *pStmt, int idx, double val) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_bind_double((pg_stmt_t*)pStmt, idx, val);
    static int (*orig)(sqlite3_stmt*, int, double) = NULL;
    if (!orig) orig = (int(*)(sqlite3_stmt*, int, double)) resolve_sqlite_symbol("sqlite3_bind_double");
    return orig ? orig(pStmt, idx, val) : SQLITE_ERROR;
}

static int my_sqlite3_bind_blob(sqlite3_stmt *pStmt, int idx, const void *val, int len, void(*destructor)(void*)) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_bind_blob((pg_stmt_t*)pStmt, idx, val, len, destructor);
    static int (*orig)(sqlite3_stmt*, int, const void*, int, void(*)(void*)) = NULL;
    if (!orig) orig = (int(*)(sqlite3_stmt*, int, const void*, int, void(*)(void*))) resolve_sqlite_symbol("sqlite3_bind_blob");
    return orig ? orig(pStmt, idx, val, len, destructor) : SQLITE_ERROR;
}

static int my_sqlite3_bind_null(sqlite3_stmt *pStmt, int idx) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return pg_bind_null((pg_stmt_t*)pStmt, idx);
    static int (*orig)(sqlite3_stmt*, int) = NULL;
    if (!orig) orig = (int(*)(sqlite3_stmt*, int)) resolve_sqlite_symbol("sqlite3_bind_null");
    return orig ? orig(pStmt, idx) : SQLITE_ERROR;
}

static int my_sqlite3_clear_bindings(sqlite3_stmt *pStmt) {
    if (is_pg_stmt((pg_stmt_t*)pStmt)) return SQLITE_OK;
    static int (*orig)(sqlite3_stmt*) = NULL;
    if (!orig) orig = (int(*)(sqlite3_stmt*)) resolve_sqlite_symbol("sqlite3_clear_bindings");
    return orig ? orig(pStmt) : SQLITE_OK;
}

static int my_sqlite3_wal_checkpoint(sqlite3 *db, const char *zDb) {
    static int (*orig)(sqlite3*, const char*) = NULL;
    if (!orig) orig = (int(*)(sqlite3*, const char*)) resolve_sqlite_symbol("sqlite3_wal_checkpoint");
    return orig ? orig(db, zDb) : SQLITE_OK;
}

static int my_sqlite3_wal_checkpoint_v2(sqlite3 *db, const char *zDb, int eMode, int *pnLog, int *pnCkpt) {
    static int (*orig)(sqlite3*, const char*, int, int*, int*) = NULL;
    if (!orig) orig = (int(*)(sqlite3*, const char*, int, int*, int*)) resolve_sqlite_symbol("sqlite3_wal_checkpoint_v2");
    return orig ? orig(db, zDb, eMode, pnLog, pnCkpt) : SQLITE_OK;
}

static int my_sqlite3_table_column_metadata(sqlite3 *db, const char *zDbName, const char *zTableName, const char *zColumnName, const char **pzDataType, const char **pzCollSeq, int *pNotNull, int *pPrimaryKey, int *pAutoinc) {
    static int (*orig)(sqlite3*, const char*, const char*, const char*, const char**, const char**, int*, int*, int*) = NULL;
    if (!orig) orig = (int(*)(sqlite3*, const char*, const char*, const char*, const char**, const char**, int*, int*, int*)) resolve_sqlite_symbol("sqlite3_table_column_metadata");
    return orig ? orig(db, zDbName, zTableName, zColumnName, pzDataType, pzCollSeq, pNotNull, pPrimaryKey, pAutoinc) : SQLITE_ERROR;
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
    LOG_INFO("Shim initialized.");
}
