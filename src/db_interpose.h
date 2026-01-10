/*
 * Plex PostgreSQL Interposing Shim - Common Header
 *
 * This header provides shared declarations for all db_interpose modules.
 * All functions are NON-static so they can be referenced by DYLD_INTERPOSE
 * in db_interpose_core.c
 */

#ifndef DB_INTERPOSE_H
#define DB_INTERPOSE_H

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <stdint.h>
#include <strings.h>
#include <ctype.h>
#include <dlfcn.h>
#include <unistd.h>
#include <sqlite3.h>
#include <libpq-fe.h>

// Visibility attributes for exported symbols
#ifdef __APPLE__
    #define EXPORT __attribute__((visibility("default")))
    // CRITICAL: orig_* pointers must be visible for child processes (Scanner, Transcoder)
    // when they load the dylib via inherited DYLD_INSERT_LIBRARIES
    #define VISIBLE __attribute__((visibility("default")))
#else
    #define EXPORT
    #define VISIBLE
#endif

// Module headers
#include "pg_types.h"
#include "pg_logging.h"
#include "pg_config.h"
#include "pg_client.h"
#include "pg_statement.h"
#include "sql_translator.h"

// ============================================================================
// Constants
// ============================================================================

#define WORKER_STACK_SIZE (8 * 1024 * 1024)  // 8MB stack for worker
#define WORKER_DELEGATION_THRESHOLD 400000   // 400KB - delegate early!
#define MAX_FAKE_VALUES 256
#define PG_FAKE_VALUE_MAGIC 0x50475641  // "PGVA"

// ============================================================================
// Shared Types
// ============================================================================

typedef enum {
    WORK_NONE = 0,
    WORK_PREPARE_V2,
    WORK_SHUTDOWN
} work_type_t;

typedef struct {
    // Input (set by caller)
    work_type_t type;
    sqlite3 *db;
    const char *zSql;
    int nByte;

    // Output (set by worker)
    sqlite3_stmt *stmt;
    const char *tail;
    int result;

    // Synchronization
    int work_ready;
    int work_done;
} worker_request_t;

typedef struct {
    uint32_t magic;      // Magic number to identify our fake values
    void *pg_stmt;       // Pointer to pg_stmt_t
    int col_idx;         // Column index
    int row_idx;         // Row index at time of column_value call
} pg_fake_value_t;

// ============================================================================
// Shared Global State (extern declarations)
// ============================================================================

// Recursion prevention
extern __thread int in_interpose_call;
extern __thread int prepare_v2_depth;

// SQLite library handle for dlsym fallback
extern void *sqlite_handle;

// Original SQLite function pointers (populated by fishhook)
// Used by my_* implementations to call the real SQLite functions
// CRITICAL: Must be VISIBLE for child processes to resolve symbols!
VISIBLE extern int (*orig_sqlite3_open)(const char*, sqlite3**);
VISIBLE extern int (*orig_sqlite3_open_v2)(const char*, sqlite3**, int, const char*);
VISIBLE extern int (*orig_sqlite3_close)(sqlite3*);
VISIBLE extern int (*orig_sqlite3_close_v2)(sqlite3*);
VISIBLE extern int (*orig_sqlite3_exec)(sqlite3*, const char*, int(*)(void*,int,char**,char**), void*, char**);
VISIBLE extern int (*orig_sqlite3_changes)(sqlite3*);
VISIBLE extern sqlite3_int64 (*orig_sqlite3_changes64)(sqlite3*);
VISIBLE extern sqlite3_int64 (*orig_sqlite3_last_insert_rowid)(sqlite3*);
VISIBLE extern int (*orig_sqlite3_get_table)(sqlite3*, const char*, char***, int*, int*, char**);

VISIBLE extern const char* (*orig_sqlite3_errmsg)(sqlite3*);
VISIBLE extern int (*orig_sqlite3_errcode)(sqlite3*);
VISIBLE extern int (*orig_sqlite3_extended_errcode)(sqlite3*);

VISIBLE extern int (*orig_sqlite3_prepare)(sqlite3*, const char*, int, sqlite3_stmt**, const char**);
VISIBLE extern int (*orig_sqlite3_prepare_v2)(sqlite3*, const char*, int, sqlite3_stmt**, const char**);
VISIBLE extern int (*orig_sqlite3_prepare_v3)(sqlite3*, const char*, int, unsigned int, sqlite3_stmt**, const char**);
VISIBLE extern int (*orig_sqlite3_prepare16_v2)(sqlite3*, const void*, int, sqlite3_stmt**, const void**);

VISIBLE extern int (*orig_sqlite3_bind_int)(sqlite3_stmt*, int, int);
VISIBLE extern int (*orig_sqlite3_bind_int64)(sqlite3_stmt*, int, sqlite3_int64);
VISIBLE extern int (*orig_sqlite3_bind_double)(sqlite3_stmt*, int, double);
VISIBLE extern int (*orig_sqlite3_bind_text)(sqlite3_stmt*, int, const char*, int, void(*)(void*));
VISIBLE extern int (*orig_sqlite3_bind_text64)(sqlite3_stmt*, int, const char*, sqlite3_uint64, void(*)(void*), unsigned char);
VISIBLE extern int (*orig_sqlite3_bind_blob)(sqlite3_stmt*, int, const void*, int, void(*)(void*));
VISIBLE extern int (*orig_sqlite3_bind_blob64)(sqlite3_stmt*, int, const void*, sqlite3_uint64, void(*)(void*));
VISIBLE extern int (*orig_sqlite3_bind_value)(sqlite3_stmt*, int, const sqlite3_value*);
VISIBLE extern int (*orig_sqlite3_bind_null)(sqlite3_stmt*, int);

VISIBLE extern int (*orig_sqlite3_step)(sqlite3_stmt*);
VISIBLE extern int (*orig_sqlite3_reset)(sqlite3_stmt*);
VISIBLE extern int (*orig_sqlite3_finalize)(sqlite3_stmt*);
VISIBLE extern int (*orig_sqlite3_clear_bindings)(sqlite3_stmt*);

VISIBLE extern int (*orig_sqlite3_column_count)(sqlite3_stmt*);
VISIBLE extern int (*orig_sqlite3_column_type)(sqlite3_stmt*, int);
VISIBLE extern int (*orig_sqlite3_column_int)(sqlite3_stmt*, int);
VISIBLE extern sqlite3_int64 (*orig_sqlite3_column_int64)(sqlite3_stmt*, int);
VISIBLE extern double (*orig_sqlite3_column_double)(sqlite3_stmt*, int);
VISIBLE extern const unsigned char* (*orig_sqlite3_column_text)(sqlite3_stmt*, int);
VISIBLE extern const void* (*orig_sqlite3_column_blob)(sqlite3_stmt*, int);
VISIBLE extern int (*orig_sqlite3_column_bytes)(sqlite3_stmt*, int);
VISIBLE extern const char* (*orig_sqlite3_column_name)(sqlite3_stmt*, int);
VISIBLE extern const char* (*orig_sqlite3_column_decltype)(sqlite3_stmt*, int);
VISIBLE extern sqlite3_value* (*orig_sqlite3_column_value)(sqlite3_stmt*, int);
VISIBLE extern int (*orig_sqlite3_data_count)(sqlite3_stmt*);

VISIBLE extern int (*orig_sqlite3_value_type)(sqlite3_value*);
VISIBLE extern const unsigned char* (*orig_sqlite3_value_text)(sqlite3_value*);
VISIBLE extern int (*orig_sqlite3_value_int)(sqlite3_value*);
VISIBLE extern sqlite3_int64 (*orig_sqlite3_value_int64)(sqlite3_value*);
VISIBLE extern double (*orig_sqlite3_value_double)(sqlite3_value*);
VISIBLE extern int (*orig_sqlite3_value_bytes)(sqlite3_value*);
VISIBLE extern const void* (*orig_sqlite3_value_blob)(sqlite3_value*);

VISIBLE extern int (*orig_sqlite3_create_collation)(sqlite3*, const char*, int, void*, int(*)(void*,int,const void*,int,const void*));
VISIBLE extern int (*orig_sqlite3_create_collation_v2)(sqlite3*, const char*, int, void*, int(*)(void*,int,const void*,int,const void*), void(*)(void*));

// Backward compatibility aliases (used by prepare module)
extern int (*real_sqlite3_prepare_v2)(sqlite3*, const char*, int, sqlite3_stmt**, const char**);
extern const char* (*real_sqlite3_errmsg)(sqlite3*);
extern int (*real_sqlite3_errcode)(sqlite3*);

// Worker thread state
extern pthread_t worker_thread;
extern pthread_mutex_t worker_mutex;
extern pthread_cond_t worker_cond_request;
extern pthread_cond_t worker_cond_response;
extern worker_request_t worker_request;
extern volatile int worker_running;

// Fake value pool
extern pg_fake_value_t fake_value_pool[MAX_FAKE_VALUES];
extern unsigned int fake_value_next;
extern pthread_mutex_t fake_value_mutex;

// Initialization flag
extern int shim_initialized;

// ============================================================================
// Core Functions (db_interpose_core.c)
// ============================================================================

void ensure_real_sqlite_loaded(void);
int worker_init(void);
void worker_cleanup(void);
int delegate_prepare_to_worker(sqlite3 *db, const char *zSql, int nByte,
                               sqlite3_stmt **ppStmt, const char **pzTail);

// ============================================================================
// Helper Functions (shared across modules)
// ============================================================================

// Check if a pointer is one of our fake values
pg_fake_value_t* pg_check_fake_value(sqlite3_value *pVal);

// Check if path is library.db
int is_library_db_path(const char *path);

// Simple string replace helper
char* simple_str_replace(const char *str, const char *old, const char *new_str);

// Check if param_value points to pre-allocated buffer
static inline int is_preallocated_buffer(pg_stmt_t *stmt, int idx) {
    return stmt->param_values[idx] >= stmt->param_buffers[idx] &&
           stmt->param_values[idx] < stmt->param_buffers[idx] + 32;
}

// ============================================================================
// Open/Close Functions (db_interpose_open.c)
// ============================================================================

void drop_icu_root_indexes(sqlite3 *db);
void drop_fts_triggers(sqlite3 *db);

EXPORT int my_sqlite3_open(const char *filename, sqlite3 **ppDb);
EXPORT int my_sqlite3_open_v2(const char *filename, sqlite3 **ppDb, int flags, const char *zVfs);
EXPORT int my_sqlite3_close(sqlite3 *db);
EXPORT int my_sqlite3_close_v2(sqlite3 *db);

// ============================================================================
// Exec Functions (db_interpose_exec.c)
// ============================================================================

EXPORT int my_sqlite3_exec(sqlite3 *db, const char *sql,
                    int (*callback)(void*, int, char**, char**),
                    void *arg, char **errmsg);

// ============================================================================
// Prepare Functions (db_interpose_prepare.c)
// ============================================================================

char* simplify_fts_for_sqlite(const char *sql);

EXPORT int my_sqlite3_prepare_v2_internal(sqlite3 *db, const char *zSql, int nByte,
                                   sqlite3_stmt **ppStmt, const char **pzTail,
                                   int from_worker);

EXPORT int my_sqlite3_prepare(sqlite3 *db, const char *zSql, int nByte,
                       sqlite3_stmt **ppStmt, const char **pzTail);
EXPORT int my_sqlite3_prepare_v2(sqlite3 *db, const char *zSql, int nByte,
                          sqlite3_stmt **ppStmt, const char **pzTail);
EXPORT int my_sqlite3_prepare_v3(sqlite3 *db, const char *zSql, int nByte,
                          unsigned int prepFlags, sqlite3_stmt **ppStmt,
                          const char **pzTail);
EXPORT int my_sqlite3_prepare16_v2(sqlite3 *db, const void *zSql, int nByte,
                            sqlite3_stmt **ppStmt, const void **pzTail);

// ============================================================================
// Bind Functions (db_interpose_bind.c)
// ============================================================================

int pg_map_param_index(pg_stmt_t *pg_stmt, sqlite3_stmt *pStmt, int sqlite_idx);
int contains_binary_bytes(const unsigned char *data, size_t len);
char* bytes_to_pg_hex(const unsigned char *data, size_t len);

EXPORT int my_sqlite3_bind_int(sqlite3_stmt *pStmt, int idx, int val);
EXPORT int my_sqlite3_bind_int64(sqlite3_stmt *pStmt, int idx, sqlite3_int64 val);
EXPORT int my_sqlite3_bind_double(sqlite3_stmt *pStmt, int idx, double val);
EXPORT int my_sqlite3_bind_text(sqlite3_stmt *pStmt, int idx, const char *val,
                         int nBytes, void (*destructor)(void*));
EXPORT int my_sqlite3_bind_text64(sqlite3_stmt *pStmt, int idx, const char *val,
                           sqlite3_uint64 nBytes, void (*destructor)(void*),
                           unsigned char encoding);
EXPORT int my_sqlite3_bind_blob(sqlite3_stmt *pStmt, int idx, const void *val,
                         int nBytes, void (*destructor)(void*));
EXPORT int my_sqlite3_bind_blob64(sqlite3_stmt *pStmt, int idx, const void *val,
                           sqlite3_uint64 nBytes, void (*destructor)(void*));
EXPORT int my_sqlite3_bind_value(sqlite3_stmt *pStmt, int idx, const sqlite3_value *pValue);
EXPORT int my_sqlite3_bind_null(sqlite3_stmt *pStmt, int idx);

// ============================================================================
// Step Functions (db_interpose_step.c)
// ============================================================================

EXPORT int my_sqlite3_step(sqlite3_stmt *pStmt);
EXPORT int my_sqlite3_reset(sqlite3_stmt *pStmt);
EXPORT int my_sqlite3_finalize(sqlite3_stmt *pStmt);
EXPORT int my_sqlite3_clear_bindings(sqlite3_stmt *pStmt);

// ============================================================================
// Column Functions (db_interpose_column.c)
// ============================================================================

const void* pg_decode_bytea(pg_stmt_t *pg_stmt, int row, int col, int *out_length);

EXPORT int my_sqlite3_column_count(sqlite3_stmt *pStmt);
EXPORT int my_sqlite3_column_type(sqlite3_stmt *pStmt, int idx);
EXPORT int my_sqlite3_column_int(sqlite3_stmt *pStmt, int idx);
EXPORT sqlite3_int64 my_sqlite3_column_int64(sqlite3_stmt *pStmt, int idx);
EXPORT double my_sqlite3_column_double(sqlite3_stmt *pStmt, int idx);
EXPORT const unsigned char* my_sqlite3_column_text(sqlite3_stmt *pStmt, int idx);
EXPORT const void* my_sqlite3_column_blob(sqlite3_stmt *pStmt, int idx);
EXPORT int my_sqlite3_column_bytes(sqlite3_stmt *pStmt, int idx);
EXPORT const char* my_sqlite3_column_name(sqlite3_stmt *pStmt, int idx);
EXPORT const char* my_sqlite3_column_decltype(sqlite3_stmt *pStmt, int idx);
EXPORT sqlite3_value* my_sqlite3_column_value(sqlite3_stmt *pStmt, int idx);
EXPORT int my_sqlite3_data_count(sqlite3_stmt *pStmt);

// ============================================================================
// Value Functions (db_interpose_column.c)
// ============================================================================

EXPORT int my_sqlite3_value_type(sqlite3_value *pVal);
EXPORT const unsigned char* my_sqlite3_value_text(sqlite3_value *pVal);
EXPORT int my_sqlite3_value_int(sqlite3_value *pVal);
EXPORT sqlite3_int64 my_sqlite3_value_int64(sqlite3_value *pVal);
EXPORT double my_sqlite3_value_double(sqlite3_value *pVal);
EXPORT int my_sqlite3_value_bytes(sqlite3_value *pVal);
EXPORT const void* my_sqlite3_value_blob(sqlite3_value *pVal);

// ============================================================================
// Metadata Functions (db_interpose_metadata.c)
// ============================================================================

EXPORT int my_sqlite3_changes(sqlite3 *db);
EXPORT sqlite3_int64 my_sqlite3_changes64(sqlite3 *db);
EXPORT sqlite3_int64 my_sqlite3_last_insert_rowid(sqlite3 *db);
EXPORT const char* my_sqlite3_errmsg(sqlite3 *db);
EXPORT int my_sqlite3_errcode(sqlite3 *db);
EXPORT int my_sqlite3_extended_errcode(sqlite3 *db);
EXPORT int my_sqlite3_get_table(sqlite3 *db, const char *sql, char ***pazResult,
                         int *pnRow, int *pnColumn, char **pzErrMsg);

// ============================================================================
// Collation Functions (db_interpose_metadata.c)
// ============================================================================

EXPORT int my_sqlite3_create_collation(sqlite3 *db, const char *zName, int eTextRep,
                                void *pArg,
                                int(*xCompare)(void*,int,const void*,int,const void*));
EXPORT int my_sqlite3_create_collation_v2(sqlite3 *db, const char *zName, int eTextRep,
                                   void *pArg,
                                   int(*xCompare)(void*,int,const void*,int,const void*),
                                   void(*xDestroy)(void*));

// ============================================================================
// Memory and Statement Info Functions (db_interpose_metadata.c)
// ============================================================================

EXPORT void my_sqlite3_free(void *ptr);
EXPORT void* my_sqlite3_malloc(int n);
EXPORT sqlite3* my_sqlite3_db_handle(sqlite3_stmt *pStmt);
EXPORT const char* my_sqlite3_sql(sqlite3_stmt *pStmt);
EXPORT char* my_sqlite3_expanded_sql(sqlite3_stmt *pStmt);
EXPORT int my_sqlite3_bind_parameter_count(sqlite3_stmt *pStmt);
EXPORT int my_sqlite3_bind_parameter_index(sqlite3_stmt *pStmt, const char *zName);
EXPORT int my_sqlite3_stmt_readonly(sqlite3_stmt *pStmt);
EXPORT int my_sqlite3_stmt_busy(sqlite3_stmt *pStmt);
EXPORT int my_sqlite3_stmt_status(sqlite3_stmt *pStmt, int op, int resetFlg);
EXPORT const char* my_sqlite3_bind_parameter_name(sqlite3_stmt *pStmt, int idx);

// Original function pointers (extern - defined in db_interpose_core.c)
VISIBLE extern void (*orig_sqlite3_free)(void*);
VISIBLE extern void* (*orig_sqlite3_malloc)(int);
VISIBLE extern sqlite3* (*orig_sqlite3_db_handle)(sqlite3_stmt*);
VISIBLE extern const char* (*orig_sqlite3_sql)(sqlite3_stmt*);
VISIBLE extern char* (*orig_sqlite3_expanded_sql)(sqlite3_stmt*);
VISIBLE extern int (*orig_sqlite3_bind_parameter_count)(sqlite3_stmt*);
VISIBLE extern int (*orig_sqlite3_bind_parameter_index)(sqlite3_stmt*, const char*);
VISIBLE extern int (*orig_sqlite3_stmt_readonly)(sqlite3_stmt*);
VISIBLE extern int (*orig_sqlite3_stmt_busy)(sqlite3_stmt*);
VISIBLE extern int (*orig_sqlite3_stmt_status)(sqlite3_stmt*, int, int);
VISIBLE extern const char* (*orig_sqlite3_bind_parameter_name)(sqlite3_stmt*, int);

#endif /* DB_INTERPOSE_H */
