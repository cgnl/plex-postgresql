/*
 * Plex PostgreSQL Interposing Shim - Linux Version
 *
 * Uses LD_PRELOAD to intercept SQLite calls and redirect to PostgreSQL.
 * Uses dlopen to load libpq at runtime to avoid library conflicts with
 * Plex's bundled libraries.
 *
 * Build:
 *   gcc -shared -fPIC -o db_interpose_pg.so db_interpose_pg_linux.c sql_translator.c \
 *       -Iinclude -lsqlite3 -ldl -lpthread
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
#include <sqlite3.h>
#include <dlfcn.h>

#include "sql_translator.h"

// ============================================================================
// PostgreSQL types and function pointers (loaded via dlopen)
// ============================================================================

// Forward declarations of PG types
typedef struct pg_conn PGconn;
typedef struct pg_result PGresult;
typedef unsigned int Oid;

// Connection status
typedef enum {
    CONNECTION_OK,
    CONNECTION_BAD
} ConnStatusType;

// Exec status
typedef enum {
    PGRES_EMPTY_QUERY = 0,
    PGRES_COMMAND_OK,
    PGRES_TUPLES_OK,
    PGRES_COPY_OUT,
    PGRES_COPY_IN,
    PGRES_BAD_RESPONSE,
    PGRES_NONFATAL_ERROR,
    PGRES_FATAL_ERROR
} ExecStatusType;

// Function pointer types
typedef PGconn* (*PQconnectdb_fn)(const char *conninfo);
typedef ConnStatusType (*PQstatus_fn)(const PGconn *conn);
typedef void (*PQfinish_fn)(PGconn *conn);
typedef char* (*PQerrorMessage_fn)(const PGconn *conn);
typedef PGresult* (*PQexec_fn)(PGconn *conn, const char *query);
typedef PGresult* (*PQexecParams_fn)(PGconn *conn, const char *command,
    int nParams, const Oid *paramTypes, const char *const *paramValues,
    const int *paramLengths, const int *paramFormats, int resultFormat);
typedef ExecStatusType (*PQresultStatus_fn)(const PGresult *res);
typedef void (*PQclear_fn)(PGresult *res);
typedef int (*PQntuples_fn)(const PGresult *res);
typedef int (*PQnfields_fn)(const PGresult *res);
typedef char* (*PQgetvalue_fn)(const PGresult *res, int tup_num, int field_num);
typedef int (*PQgetlength_fn)(const PGresult *res, int tup_num, int field_num);
typedef int (*PQgetisnull_fn)(const PGresult *res, int tup_num, int field_num);
typedef Oid (*PQftype_fn)(const PGresult *res, int field_num);
typedef char* (*PQfname_fn)(const PGresult *res, int field_num);

// Global function pointers
static void *libpq_handle = NULL;
static PQconnectdb_fn pPQconnectdb = NULL;
static PQstatus_fn pPQstatus = NULL;
static PQfinish_fn pPQfinish = NULL;
static PQerrorMessage_fn pPQerrorMessage = NULL;
static PQexec_fn pPQexec = NULL;
static PQexecParams_fn pPQexecParams = NULL;
static PQresultStatus_fn pPQresultStatus = NULL;
static PQclear_fn pPQclear = NULL;
static PQntuples_fn pPQntuples = NULL;
static PQnfields_fn pPQnfields = NULL;
static PQgetvalue_fn pPQgetvalue = NULL;
static PQgetlength_fn pPQgetlength = NULL;
static PQgetisnull_fn pPQgetisnull = NULL;
static PQftype_fn pPQftype = NULL;
static PQfname_fn pPQfname = NULL;

static int libpq_loaded = 0;
static pthread_mutex_t libpq_mutex = PTHREAD_MUTEX_INITIALIZER;

// Forward declaration
static void log_message(const char *fmt, ...);

// ============================================================================
// Original SQLite Library Handle and Function Pointers
// ============================================================================

static void *orig_sqlite_handle = NULL;
static int orig_sqlite_loaded = 0;
static pthread_mutex_t orig_sqlite_mutex = PTHREAD_MUTEX_INITIALIZER;

// Core functions we intercept
static int (*orig_sqlite3_open)(const char*, sqlite3**) = NULL;
static int (*orig_sqlite3_open_v2)(const char*, sqlite3**, int, const char*) = NULL;
static int (*orig_sqlite3_close)(sqlite3*) = NULL;
static int (*orig_sqlite3_close_v2)(sqlite3*) = NULL;
static int (*orig_sqlite3_exec)(sqlite3*, const char*, int(*)(void*,int,char**,char**), void*, char**) = NULL;
static int (*orig_sqlite3_prepare_v2)(sqlite3*, const char*, int, sqlite3_stmt**, const char**) = NULL;
static int (*orig_sqlite3_bind_int)(sqlite3_stmt*, int, int) = NULL;
static int (*orig_sqlite3_bind_int64)(sqlite3_stmt*, int, sqlite3_int64) = NULL;
static int (*orig_sqlite3_bind_double)(sqlite3_stmt*, int, double) = NULL;
static int (*orig_sqlite3_bind_text)(sqlite3_stmt*, int, const char*, int, void(*)(void*)) = NULL;
static int (*orig_sqlite3_bind_blob)(sqlite3_stmt*, int, const void*, int, void(*)(void*)) = NULL;
static int (*orig_sqlite3_bind_null)(sqlite3_stmt*, int) = NULL;
static int (*orig_sqlite3_step)(sqlite3_stmt*) = NULL;
static int (*orig_sqlite3_reset)(sqlite3_stmt*) = NULL;
static int (*orig_sqlite3_finalize)(sqlite3_stmt*) = NULL;
static int (*orig_sqlite3_clear_bindings)(sqlite3_stmt*) = NULL;
static int (*orig_sqlite3_column_count)(sqlite3_stmt*) = NULL;
static int (*orig_sqlite3_column_type)(sqlite3_stmt*, int) = NULL;
static int (*orig_sqlite3_column_int)(sqlite3_stmt*, int) = NULL;
static sqlite3_int64 (*orig_sqlite3_column_int64)(sqlite3_stmt*, int) = NULL;
static double (*orig_sqlite3_column_double)(sqlite3_stmt*, int) = NULL;
static const unsigned char* (*orig_sqlite3_column_text)(sqlite3_stmt*, int) = NULL;
static const void* (*orig_sqlite3_column_blob)(sqlite3_stmt*, int) = NULL;
static int (*orig_sqlite3_column_bytes)(sqlite3_stmt*, int) = NULL;
static const char* (*orig_sqlite3_column_name)(sqlite3_stmt*, int) = NULL;

// Additional SQLite functions that need forwarding
static void (*orig_sqlite3_free)(void*) = NULL;
static void* (*orig_sqlite3_malloc)(int) = NULL;
static void* (*orig_sqlite3_malloc64)(sqlite3_uint64) = NULL;
static void* (*orig_sqlite3_realloc)(void*, int) = NULL;
static void* (*orig_sqlite3_realloc64)(void*, sqlite3_uint64) = NULL;
static int (*orig_sqlite3_busy_timeout)(sqlite3*, int) = NULL;
static int (*orig_sqlite3_get_autocommit)(sqlite3*) = NULL;
static int (*orig_sqlite3_changes)(sqlite3*) = NULL;
static sqlite3_int64 (*orig_sqlite3_changes64)(sqlite3*) = NULL;
static int (*orig_sqlite3_total_changes)(sqlite3*) = NULL;
static sqlite3_int64 (*orig_sqlite3_total_changes64)(sqlite3*) = NULL;
static sqlite3_int64 (*orig_sqlite3_last_insert_rowid)(sqlite3*) = NULL;
static const char* (*orig_sqlite3_errmsg)(sqlite3*) = NULL;
static int (*orig_sqlite3_errcode)(sqlite3*) = NULL;
static int (*orig_sqlite3_extended_errcode)(sqlite3*) = NULL;
static int (*orig_sqlite3_error_offset)(sqlite3*) = NULL;
static const char* (*orig_sqlite3_libversion)(void) = NULL;
static int (*orig_sqlite3_libversion_number)(void) = NULL;
static int (*orig_sqlite3_threadsafe)(void) = NULL;
static int (*orig_sqlite3_initialize)(void) = NULL;
static int (*orig_sqlite3_shutdown)(void) = NULL;
static int (*orig_sqlite3_config)(int, ...) = NULL;
static void (*orig_sqlite3_interrupt)(sqlite3*) = NULL;
static int (*orig_sqlite3_complete)(const char*) = NULL;
static int (*orig_sqlite3_sleep)(int) = NULL;
static void (*orig_sqlite3_randomness)(int, void*) = NULL;
static int (*orig_sqlite3_limit)(sqlite3*, int, int) = NULL;
static int (*orig_sqlite3_bind_parameter_count)(sqlite3_stmt*) = NULL;
static const char* (*orig_sqlite3_bind_parameter_name)(sqlite3_stmt*, int) = NULL;
static int (*orig_sqlite3_bind_parameter_index)(sqlite3_stmt*, const char*) = NULL;
static int (*orig_sqlite3_stmt_isexplain)(sqlite3_stmt*) = NULL;
static int (*orig_sqlite3_stmt_status)(sqlite3_stmt*, int, int) = NULL;
static int (*orig_sqlite3_db_status)(sqlite3*, int, int*, int*, int) = NULL;
static int (*orig_sqlite3_status64)(int, sqlite3_int64*, sqlite3_int64*, int) = NULL;
static int (*orig_sqlite3_db_readonly)(sqlite3*, const char*) = NULL;
static int (*orig_sqlite3_txn_state)(sqlite3*, const char*) = NULL;
static int (*orig_sqlite3_vfs_register)(sqlite3_vfs*, int) = NULL;
static int (*orig_sqlite3_vfs_unregister)(sqlite3_vfs*) = NULL;
static sqlite3_vfs* (*orig_sqlite3_vfs_find)(const char*) = NULL;
static int (*orig_sqlite3_file_control)(sqlite3*, const char*, int, void*) = NULL;
static int (*orig_sqlite3_set_authorizer)(sqlite3*, int(*)(void*,int,const char*,const char*,const char*,const char*), void*) = NULL;
static int (*orig_sqlite3_enable_load_extension)(sqlite3*, int) = NULL;
static int (*orig_sqlite3_load_extension)(sqlite3*, const char*, const char*, char**) = NULL;
static int (*orig_sqlite3_test_control)(int, ...) = NULL;
static void (*orig_sqlite3_progress_handler)(sqlite3*, int, int(*)(void*), void*) = NULL;
static int (*orig_sqlite3_strnicmp)(const char*, const char*, int) = NULL;
static int (*orig_sqlite3_strglob)(const char*, const char*) = NULL;
static int (*orig_sqlite3_strlike)(const char*, const char*, unsigned int) = NULL;
static int (*orig_sqlite3_deserialize)(sqlite3*, const char*, unsigned char*, sqlite3_int64, sqlite3_int64, unsigned) = NULL;
static int (*orig_sqlite3_table_column_metadata)(sqlite3*, const char*, const char*, const char*, char const**, char const**, int*, int*, int*) = NULL;
static const char* (*orig_sqlite3_column_decltype)(sqlite3_stmt*, int) = NULL;
static int (*orig_sqlite3_create_function_v2)(sqlite3*, const char*, int, int, void*, void(*)(sqlite3_context*,int,sqlite3_value**), void(*)(sqlite3_context*,int,sqlite3_value**), void(*)(sqlite3_context*), void(*)(void*)) = NULL;
static int (*orig_sqlite3_create_window_function)(sqlite3*, const char*, int, int, void*, void(*)(sqlite3_context*,int,sqlite3_value**), void(*)(sqlite3_context*), void(*)(sqlite3_context*), void(*)(sqlite3_context*,int,sqlite3_value**), void(*)(void*)) = NULL;
static int (*orig_sqlite3_keyword_count)(void) = NULL;
static int (*orig_sqlite3_keyword_name)(int, const char**, int*) = NULL;
static int (*orig_sqlite3_keyword_check)(const char*, int) = NULL;
static int (*orig_sqlite3_vtab_config)(sqlite3*, int, ...) = NULL;
static const char* (*orig_sqlite3_vtab_collation)(sqlite3_index_info*, int) = NULL;
static void (*orig_sqlite3_result_blob)(sqlite3_context*, const void*, int, void(*)(void*)) = NULL;
static void (*orig_sqlite3_result_blob64)(sqlite3_context*, const void*, sqlite3_uint64, void(*)(void*)) = NULL;
static void (*orig_sqlite3_result_text)(sqlite3_context*, const char*, int, void(*)(void*)) = NULL;
static void (*orig_sqlite3_result_text64)(sqlite3_context*, const char*, sqlite3_uint64, void(*)(void*), unsigned char) = NULL;
static char* (*orig_sqlite3_vsnprintf)(int, char*, const char*, va_list) = NULL;
static char* (*orig_sqlite3_snprintf)(int, char*, const char*, ...) = NULL;
static sqlite3_str* (*orig_sqlite3_str_new)(sqlite3*) = NULL;
static void (*orig_sqlite3_str_appendf)(sqlite3_str*, const char*, ...) = NULL;
static void (*orig_sqlite3_str_appendall)(sqlite3_str*, const char*) = NULL;
static void (*orig_sqlite3_str_append)(sqlite3_str*, const char*, int) = NULL;
static char* (*orig_sqlite3_str_finish)(sqlite3_str*) = NULL;

// More SQLite functions needed by Plex
static void (*orig_sqlite3_result_double)(sqlite3_context*, double) = NULL;
static void (*orig_sqlite3_result_null)(sqlite3_context*) = NULL;
static void (*orig_sqlite3_result_int)(sqlite3_context*, int) = NULL;
static void (*orig_sqlite3_result_int64)(sqlite3_context*, sqlite3_int64) = NULL;
static void (*orig_sqlite3_result_error)(sqlite3_context*, const char*, int) = NULL;
static void (*orig_sqlite3_result_error_code)(sqlite3_context*, int) = NULL;
static void (*orig_sqlite3_result_error_nomem)(sqlite3_context*) = NULL;
static void (*orig_sqlite3_result_value)(sqlite3_context*, sqlite3_value*) = NULL;
static void* (*orig_sqlite3_aggregate_context)(sqlite3_context*, int) = NULL;
static int (*orig_sqlite3_create_function)(sqlite3*, const char*, int, int, void*, void(*)(sqlite3_context*,int,sqlite3_value**), void(*)(sqlite3_context*,int,sqlite3_value**), void(*)(sqlite3_context*)) = NULL;
static int (*orig_sqlite3_create_collation)(sqlite3*, const char*, int, void*, int(*)(void*,int,const void*,int,const void*)) = NULL;
static int (*orig_sqlite3_data_count)(sqlite3_stmt*) = NULL;
static int (*orig_sqlite3_enable_shared_cache)(int) = NULL;
static sqlite3* (*orig_sqlite3_db_handle)(sqlite3_stmt*) = NULL;
static sqlite3_value* (*orig_sqlite3_column_value)(sqlite3_stmt*, int) = NULL;
static int (*orig_sqlite3_value_type)(sqlite3_value*) = NULL;
static int (*orig_sqlite3_value_int)(sqlite3_value*) = NULL;
static sqlite3_int64 (*orig_sqlite3_value_int64)(sqlite3_value*) = NULL;
static double (*orig_sqlite3_value_double)(sqlite3_value*) = NULL;
static const unsigned char* (*orig_sqlite3_value_text)(sqlite3_value*) = NULL;
static int (*orig_sqlite3_value_bytes)(sqlite3_value*) = NULL;
static const void* (*orig_sqlite3_value_blob)(sqlite3_value*) = NULL;
static int (*orig_sqlite3_prepare_v3)(sqlite3*, const char*, int, unsigned int, sqlite3_stmt**, const char**) = NULL;
static int (*orig_sqlite3_prepare)(sqlite3*, const char*, int, sqlite3_stmt**, const char**) = NULL;
static int (*orig_sqlite3_bind_text64)(sqlite3_stmt*, int, const char*, sqlite3_uint64, void(*)(void*), unsigned char) = NULL;
static int (*orig_sqlite3_bind_blob64)(sqlite3_stmt*, int, const void*, sqlite3_uint64, void(*)(void*)) = NULL;
static int (*orig_sqlite3_bind_value)(sqlite3_stmt*, int, const sqlite3_value*) = NULL;
static int (*orig_sqlite3_trace_v2)(sqlite3*, unsigned, int(*)(unsigned,void*,void*,void*), void*) = NULL;
static sqlite3_backup* (*orig_sqlite3_backup_init)(sqlite3*, const char*, sqlite3*, const char*) = NULL;
static int (*orig_sqlite3_backup_step)(sqlite3_backup*, int) = NULL;
static int (*orig_sqlite3_backup_remaining)(sqlite3_backup*) = NULL;
static int (*orig_sqlite3_backup_pagecount)(sqlite3_backup*) = NULL;
static int (*orig_sqlite3_backup_finish)(sqlite3_backup*) = NULL;
static int (*orig_sqlite3_busy_handler)(sqlite3*, int(*)(void*,int), void*) = NULL;
static char* (*orig_sqlite3_expanded_sql)(sqlite3_stmt*) = NULL;
static int (*orig_sqlite3_create_module)(sqlite3*, const char*, const sqlite3_module*, void*) = NULL;
static char* (*orig_sqlite3_mprintf)(const char*, ...) = NULL;
static char* (*orig_sqlite3_vmprintf)(const char*, va_list) = NULL;
static int (*orig_sqlite3_vtab_on_conflict)(sqlite3*) = NULL;
static int (*orig_sqlite3_stricmp)(const char*, const char*) = NULL;
static int (*orig_sqlite3_declare_vtab)(sqlite3*, const char*) = NULL;
static void* (*orig_sqlite3_user_data)(sqlite3_context*) = NULL;
static sqlite3* (*orig_sqlite3_context_db_handle)(sqlite3_context*) = NULL;
static int (*orig_sqlite3_db_config)(sqlite3*, int, ...) = NULL;
static int (*orig_sqlite3_stmt_readonly)(sqlite3_stmt*) = NULL;
static const char* (*orig_sqlite3_sql)(sqlite3_stmt*) = NULL;
static void* (*orig_sqlite3_get_auxdata)(sqlite3_context*, int) = NULL;
static void (*orig_sqlite3_set_auxdata)(sqlite3_context*, int, void*, void(*)(void*)) = NULL;
static const char* (*orig_sqlite3_sourceid)(void) = NULL;

// ============================================================================
// Configuration
// ============================================================================

#define LOG_FILE "/tmp/plex_redirect_pg.log"
#define MAX_CONNECTIONS 64
#define MAX_PARAMS 64

#define ENV_PG_HOST     "PLEX_PG_HOST"
#define ENV_PG_PORT     "PLEX_PG_PORT"
#define ENV_PG_DATABASE "PLEX_PG_DATABASE"
#define ENV_PG_USER     "PLEX_PG_USER"
#define ENV_PG_PASSWORD "PLEX_PG_PASSWORD"
#define ENV_PG_SCHEMA   "PLEX_PG_SCHEMA"

static const char *REDIRECT_PATTERNS[] = {
    "com.plexapp.plugins.library.db",
    "com.plexapp.plugins.library.blobs.db",
    NULL
};

// ============================================================================
// Types
// ============================================================================

typedef struct {
    char host[256];
    int port;
    char database[128];
    char user[128];
    char password[256];
    char schema[64];
} pg_conn_config_t;

typedef struct pg_connection {
    PGconn *conn;
    sqlite3 *shadow_db;  // Real SQLite handle for fallback
    char db_path[1024];
    int is_pg_active;    // Whether we're using PostgreSQL
    int in_transaction;
    pthread_mutex_t mutex;
} pg_connection_t;

typedef struct pg_stmt {
    pg_connection_t *conn;
    sqlite3_stmt *shadow_stmt;  // Real SQLite statement for fallback
    char *sql;
    char *pg_sql;
    PGresult *result;
    int current_row;
    int num_rows;
    int num_cols;
    int is_pg;

    char *param_values[MAX_PARAMS];
    int param_lengths[MAX_PARAMS];
    int param_formats[MAX_PARAMS];
    int param_count;
} pg_stmt_t;

// ============================================================================
// Globals
// ============================================================================

static FILE *log_file = NULL;
static pthread_mutex_t log_mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_mutex_t conn_mutex = PTHREAD_MUTEX_INITIALIZER;

static pg_conn_config_t pg_config;
static int config_loaded = 0;
static int shim_initialized = 0;

static pg_connection_t *connections[MAX_CONNECTIONS];
static int connection_count = 0;

// Statement tracking - maps sqlite3_stmt* to pg_stmt_t*
#define MAX_STATEMENTS 1024
static struct {
    sqlite3_stmt *sqlite_stmt;
    pg_stmt_t *pg_stmt;
} stmt_map[MAX_STATEMENTS];
static int stmt_count = 0;
static pthread_mutex_t stmt_mutex = PTHREAD_MUTEX_INITIALIZER;

// ============================================================================
// Load libpq dynamically
// ============================================================================

// Link-map ID for isolated namespace
static Lmid_t libpq_namespace = LM_ID_BASE;
static int namespace_created = 0;

static int load_libpq(void) {
    pthread_mutex_lock(&libpq_mutex);

    if (libpq_loaded) {
        pthread_mutex_unlock(&libpq_mutex);
        return libpq_handle != NULL;
    }

    libpq_loaded = 1;

    // Pre-load our bundled musl-based SSL libraries first
    // These must be loaded before libpq to satisfy its dependencies
    log_message("Pre-loading bundled SSL libraries...");
    void *ssl_handle = dlopen("/usr/local/lib/plex-postgresql/libssl.so.3", RTLD_NOW | RTLD_GLOBAL);
    if (ssl_handle) {
        log_message("  Loaded bundled libssl.so.3");
    }
    void *crypto_handle = dlopen("/usr/local/lib/plex-postgresql/libcrypto.so.3", RTLD_NOW | RTLD_GLOBAL);
    if (crypto_handle) {
        log_message("  Loaded bundled libcrypto.so.3");
    }

    // Try to load libpq
    log_message("Attempting to load libpq...");

    // Try various libpq locations - bundled musl version first
    const char *libpq_paths[] = {
        // Our bundled musl-based libpq (for linuxserver/plex which uses musl)
        "/usr/local/lib/plex-postgresql/libpq.so.5",
        // Plex's lib directory (in case we add it there)
        "/usr/lib/plexmediaserver/lib/libpq.so.5",
        // Standard system paths (for glibc-based systems)
        "/usr/lib/aarch64-linux-gnu/libpq.so.5",
        "/usr/lib/x86_64-linux-gnu/libpq.so.5",
        "/usr/lib/libpq.so.5",
        "/usr/local/lib/libpq.so.5",
        "/lib/aarch64-linux-gnu/libpq.so.5",
        "/lib/x86_64-linux-gnu/libpq.so.5",
        "libpq.so.5",
        NULL
    };

    // First, try using dlmopen with LM_ID_NEWLM to create an isolated namespace
    for (int i = 0; libpq_paths[i]; i++) {
        // LM_ID_NEWLM creates a fresh namespace with its own copies of all libraries
        libpq_handle = dlmopen(LM_ID_NEWLM, libpq_paths[i], RTLD_NOW | RTLD_LOCAL);
        if (libpq_handle) {
            log_message("Loaded libpq in new namespace from: %s", libpq_paths[i]);
            namespace_created = 1;
            // Get the namespace ID for this handle
            if (dlinfo(libpq_handle, RTLD_DI_LMID, &libpq_namespace) == 0) {
                log_message("Using namespace ID: %ld", (long)libpq_namespace);
            }
            break;
        } else {
            const char *err = dlerror();
            if (err && libpq_paths[i][0] == '/') {
                // Only log failures for absolute paths to reduce noise
                log_message("dlmopen failed for %s: %s", libpq_paths[i], err);
            }
        }
    }

    // If dlmopen failed, fall back to regular dlopen with RTLD_DEEPBIND
    if (!libpq_handle) {
        log_message("Falling back to dlopen with RTLD_DEEPBIND...");
        for (int i = 0; libpq_paths[i]; i++) {
            libpq_handle = dlopen(libpq_paths[i], RTLD_NOW | RTLD_LOCAL | RTLD_DEEPBIND);
            if (libpq_handle) {
                log_message("Loaded libpq from: %s", libpq_paths[i]);
                break;
            } else {
                const char *err = dlerror();
                if (err && libpq_paths[i][0] == '/') {
                    log_message("dlopen failed for %s: %s", libpq_paths[i], err);
                }
            }
        }
    }

    if (!libpq_handle) {
        log_message("Could not load libpq from any location");
        pthread_mutex_unlock(&libpq_mutex);
        return 0;
    }

    // Load all function pointers
    pPQconnectdb = (PQconnectdb_fn)dlsym(libpq_handle, "PQconnectdb");
    pPQstatus = (PQstatus_fn)dlsym(libpq_handle, "PQstatus");
    pPQfinish = (PQfinish_fn)dlsym(libpq_handle, "PQfinish");
    pPQerrorMessage = (PQerrorMessage_fn)dlsym(libpq_handle, "PQerrorMessage");
    pPQexec = (PQexec_fn)dlsym(libpq_handle, "PQexec");
    pPQexecParams = (PQexecParams_fn)dlsym(libpq_handle, "PQexecParams");
    pPQresultStatus = (PQresultStatus_fn)dlsym(libpq_handle, "PQresultStatus");
    pPQclear = (PQclear_fn)dlsym(libpq_handle, "PQclear");
    pPQntuples = (PQntuples_fn)dlsym(libpq_handle, "PQntuples");
    pPQnfields = (PQnfields_fn)dlsym(libpq_handle, "PQnfields");
    pPQgetvalue = (PQgetvalue_fn)dlsym(libpq_handle, "PQgetvalue");
    pPQgetlength = (PQgetlength_fn)dlsym(libpq_handle, "PQgetlength");
    pPQgetisnull = (PQgetisnull_fn)dlsym(libpq_handle, "PQgetisnull");
    pPQftype = (PQftype_fn)dlsym(libpq_handle, "PQftype");
    pPQfname = (PQfname_fn)dlsym(libpq_handle, "PQfname");

    // Verify all functions loaded
    if (!pPQconnectdb || !pPQstatus || !pPQfinish || !pPQerrorMessage ||
        !pPQexec || !pPQexecParams || !pPQresultStatus || !pPQclear ||
        !pPQntuples || !pPQnfields || !pPQgetvalue || !pPQgetlength ||
        !pPQgetisnull || !pPQftype || !pPQfname) {
        dlclose(libpq_handle);
        libpq_handle = NULL;
        pthread_mutex_unlock(&libpq_mutex);
        return 0;
    }

    pthread_mutex_unlock(&libpq_mutex);
    return 1;
}

// ============================================================================
// Initialize Original SQLite Functions
// ============================================================================

static int load_original_sqlite(void) {
    pthread_mutex_lock(&orig_sqlite_mutex);

    if (orig_sqlite_loaded) {
        pthread_mutex_unlock(&orig_sqlite_mutex);
        return orig_sqlite_handle != NULL;
    }

    orig_sqlite_loaded = 1;

    // Try to load the original SQLite library
    // First try the renamed Plex bundled version, then system SQLite
    const char *sqlite_paths[] = {
        "/usr/lib/plexmediaserver/lib/libsqlite3_original.so",
        "/lib/libsqlite3.so.0",
        "/usr/lib/libsqlite3.so.0",
        "/lib/aarch64-linux-gnu/libsqlite3.so.0",
        "/lib/x86_64-linux-gnu/libsqlite3.so.0",
        "/usr/lib/aarch64-linux-gnu/libsqlite3.so.0",
        "/usr/lib/x86_64-linux-gnu/libsqlite3.so.0",
        "libsqlite3.so.0",
        NULL
    };

    for (int i = 0; sqlite_paths[i]; i++) {
        // Use RTLD_DEEPBIND to ensure the original SQLite uses its own symbols
        // This prevents recursive calls back into our shim
        orig_sqlite_handle = dlopen(sqlite_paths[i], RTLD_NOW | RTLD_LOCAL | RTLD_DEEPBIND);
        if (orig_sqlite_handle) {
            log_message("Loaded original SQLite from: %s", sqlite_paths[i]);
            break;
        }
    }

    if (!orig_sqlite_handle) {
        log_message("ERROR: Could not load original SQLite library!");
        pthread_mutex_unlock(&orig_sqlite_mutex);
        return 0;
    }

    // Load core functions
    orig_sqlite3_open = dlsym(orig_sqlite_handle, "sqlite3_open");
    orig_sqlite3_open_v2 = dlsym(orig_sqlite_handle, "sqlite3_open_v2");
    orig_sqlite3_close = dlsym(orig_sqlite_handle, "sqlite3_close");
    orig_sqlite3_close_v2 = dlsym(orig_sqlite_handle, "sqlite3_close_v2");
    orig_sqlite3_exec = dlsym(orig_sqlite_handle, "sqlite3_exec");
    orig_sqlite3_prepare_v2 = dlsym(orig_sqlite_handle, "sqlite3_prepare_v2");
    orig_sqlite3_bind_int = dlsym(orig_sqlite_handle, "sqlite3_bind_int");
    orig_sqlite3_bind_int64 = dlsym(orig_sqlite_handle, "sqlite3_bind_int64");
    orig_sqlite3_bind_double = dlsym(orig_sqlite_handle, "sqlite3_bind_double");
    orig_sqlite3_bind_text = dlsym(orig_sqlite_handle, "sqlite3_bind_text");
    orig_sqlite3_bind_blob = dlsym(orig_sqlite_handle, "sqlite3_bind_blob");
    orig_sqlite3_bind_null = dlsym(orig_sqlite_handle, "sqlite3_bind_null");
    orig_sqlite3_step = dlsym(orig_sqlite_handle, "sqlite3_step");
    orig_sqlite3_reset = dlsym(orig_sqlite_handle, "sqlite3_reset");
    orig_sqlite3_finalize = dlsym(orig_sqlite_handle, "sqlite3_finalize");
    orig_sqlite3_clear_bindings = dlsym(orig_sqlite_handle, "sqlite3_clear_bindings");
    orig_sqlite3_column_count = dlsym(orig_sqlite_handle, "sqlite3_column_count");
    orig_sqlite3_column_type = dlsym(orig_sqlite_handle, "sqlite3_column_type");
    orig_sqlite3_column_int = dlsym(orig_sqlite_handle, "sqlite3_column_int");
    orig_sqlite3_column_int64 = dlsym(orig_sqlite_handle, "sqlite3_column_int64");
    orig_sqlite3_column_double = dlsym(orig_sqlite_handle, "sqlite3_column_double");
    orig_sqlite3_column_text = dlsym(orig_sqlite_handle, "sqlite3_column_text");
    orig_sqlite3_column_blob = dlsym(orig_sqlite_handle, "sqlite3_column_blob");
    orig_sqlite3_column_bytes = dlsym(orig_sqlite_handle, "sqlite3_column_bytes");
    orig_sqlite3_column_name = dlsym(orig_sqlite_handle, "sqlite3_column_name");

    // Load additional forwarded functions
    orig_sqlite3_free = dlsym(orig_sqlite_handle, "sqlite3_free");
    orig_sqlite3_malloc = dlsym(orig_sqlite_handle, "sqlite3_malloc");
    orig_sqlite3_malloc64 = dlsym(orig_sqlite_handle, "sqlite3_malloc64");
    orig_sqlite3_realloc = dlsym(orig_sqlite_handle, "sqlite3_realloc");
    orig_sqlite3_realloc64 = dlsym(orig_sqlite_handle, "sqlite3_realloc64");
    orig_sqlite3_busy_timeout = dlsym(orig_sqlite_handle, "sqlite3_busy_timeout");
    orig_sqlite3_get_autocommit = dlsym(orig_sqlite_handle, "sqlite3_get_autocommit");
    orig_sqlite3_changes = dlsym(orig_sqlite_handle, "sqlite3_changes");
    orig_sqlite3_changes64 = dlsym(orig_sqlite_handle, "sqlite3_changes64");
    orig_sqlite3_total_changes = dlsym(orig_sqlite_handle, "sqlite3_total_changes");
    orig_sqlite3_total_changes64 = dlsym(orig_sqlite_handle, "sqlite3_total_changes64");
    orig_sqlite3_last_insert_rowid = dlsym(orig_sqlite_handle, "sqlite3_last_insert_rowid");
    orig_sqlite3_errmsg = dlsym(orig_sqlite_handle, "sqlite3_errmsg");
    orig_sqlite3_errcode = dlsym(orig_sqlite_handle, "sqlite3_errcode");
    orig_sqlite3_extended_errcode = dlsym(orig_sqlite_handle, "sqlite3_extended_errcode");
    orig_sqlite3_error_offset = dlsym(orig_sqlite_handle, "sqlite3_error_offset");
    orig_sqlite3_libversion = dlsym(orig_sqlite_handle, "sqlite3_libversion");
    orig_sqlite3_libversion_number = dlsym(orig_sqlite_handle, "sqlite3_libversion_number");
    orig_sqlite3_threadsafe = dlsym(orig_sqlite_handle, "sqlite3_threadsafe");
    orig_sqlite3_initialize = dlsym(orig_sqlite_handle, "sqlite3_initialize");
    orig_sqlite3_shutdown = dlsym(orig_sqlite_handle, "sqlite3_shutdown");
    orig_sqlite3_interrupt = dlsym(orig_sqlite_handle, "sqlite3_interrupt");
    orig_sqlite3_complete = dlsym(orig_sqlite_handle, "sqlite3_complete");
    orig_sqlite3_sleep = dlsym(orig_sqlite_handle, "sqlite3_sleep");
    orig_sqlite3_randomness = dlsym(orig_sqlite_handle, "sqlite3_randomness");
    orig_sqlite3_limit = dlsym(orig_sqlite_handle, "sqlite3_limit");
    orig_sqlite3_bind_parameter_count = dlsym(orig_sqlite_handle, "sqlite3_bind_parameter_count");
    orig_sqlite3_bind_parameter_name = dlsym(orig_sqlite_handle, "sqlite3_bind_parameter_name");
    orig_sqlite3_bind_parameter_index = dlsym(orig_sqlite_handle, "sqlite3_bind_parameter_index");
    orig_sqlite3_stmt_isexplain = dlsym(orig_sqlite_handle, "sqlite3_stmt_isexplain");
    orig_sqlite3_stmt_status = dlsym(orig_sqlite_handle, "sqlite3_stmt_status");
    orig_sqlite3_db_status = dlsym(orig_sqlite_handle, "sqlite3_db_status");
    orig_sqlite3_status64 = dlsym(orig_sqlite_handle, "sqlite3_status64");
    orig_sqlite3_db_readonly = dlsym(orig_sqlite_handle, "sqlite3_db_readonly");
    orig_sqlite3_txn_state = dlsym(orig_sqlite_handle, "sqlite3_txn_state");
    orig_sqlite3_vfs_register = dlsym(orig_sqlite_handle, "sqlite3_vfs_register");
    orig_sqlite3_vfs_unregister = dlsym(orig_sqlite_handle, "sqlite3_vfs_unregister");
    orig_sqlite3_vfs_find = dlsym(orig_sqlite_handle, "sqlite3_vfs_find");
    orig_sqlite3_file_control = dlsym(orig_sqlite_handle, "sqlite3_file_control");
    orig_sqlite3_set_authorizer = dlsym(orig_sqlite_handle, "sqlite3_set_authorizer");
    orig_sqlite3_enable_load_extension = dlsym(orig_sqlite_handle, "sqlite3_enable_load_extension");
    orig_sqlite3_load_extension = dlsym(orig_sqlite_handle, "sqlite3_load_extension");
    orig_sqlite3_test_control = dlsym(orig_sqlite_handle, "sqlite3_test_control");
    orig_sqlite3_progress_handler = dlsym(orig_sqlite_handle, "sqlite3_progress_handler");
    orig_sqlite3_strnicmp = dlsym(orig_sqlite_handle, "sqlite3_strnicmp");
    orig_sqlite3_strglob = dlsym(orig_sqlite_handle, "sqlite3_strglob");
    orig_sqlite3_strlike = dlsym(orig_sqlite_handle, "sqlite3_strlike");
    orig_sqlite3_deserialize = dlsym(orig_sqlite_handle, "sqlite3_deserialize");
    orig_sqlite3_table_column_metadata = dlsym(orig_sqlite_handle, "sqlite3_table_column_metadata");
    orig_sqlite3_column_decltype = dlsym(orig_sqlite_handle, "sqlite3_column_decltype");
    orig_sqlite3_create_function_v2 = dlsym(orig_sqlite_handle, "sqlite3_create_function_v2");
    orig_sqlite3_create_window_function = dlsym(orig_sqlite_handle, "sqlite3_create_window_function");
    orig_sqlite3_keyword_count = dlsym(orig_sqlite_handle, "sqlite3_keyword_count");
    orig_sqlite3_keyword_name = dlsym(orig_sqlite_handle, "sqlite3_keyword_name");
    orig_sqlite3_keyword_check = dlsym(orig_sqlite_handle, "sqlite3_keyword_check");
    orig_sqlite3_vtab_collation = dlsym(orig_sqlite_handle, "sqlite3_vtab_collation");
    orig_sqlite3_result_blob = dlsym(orig_sqlite_handle, "sqlite3_result_blob");
    orig_sqlite3_result_blob64 = dlsym(orig_sqlite_handle, "sqlite3_result_blob64");
    orig_sqlite3_result_text = dlsym(orig_sqlite_handle, "sqlite3_result_text");
    orig_sqlite3_result_text64 = dlsym(orig_sqlite_handle, "sqlite3_result_text64");
    orig_sqlite3_vsnprintf = dlsym(orig_sqlite_handle, "sqlite3_vsnprintf");
    orig_sqlite3_snprintf = dlsym(orig_sqlite_handle, "sqlite3_snprintf");
    orig_sqlite3_str_new = dlsym(orig_sqlite_handle, "sqlite3_str_new");
    orig_sqlite3_str_appendf = dlsym(orig_sqlite_handle, "sqlite3_str_appendf");
    orig_sqlite3_str_appendall = dlsym(orig_sqlite_handle, "sqlite3_str_appendall");
    orig_sqlite3_str_append = dlsym(orig_sqlite_handle, "sqlite3_str_append");
    orig_sqlite3_str_finish = dlsym(orig_sqlite_handle, "sqlite3_str_finish");

    // More Plex-required functions
    orig_sqlite3_result_double = dlsym(orig_sqlite_handle, "sqlite3_result_double");
    orig_sqlite3_result_null = dlsym(orig_sqlite_handle, "sqlite3_result_null");
    orig_sqlite3_result_int = dlsym(orig_sqlite_handle, "sqlite3_result_int");
    orig_sqlite3_result_int64 = dlsym(orig_sqlite_handle, "sqlite3_result_int64");
    orig_sqlite3_result_error = dlsym(orig_sqlite_handle, "sqlite3_result_error");
    orig_sqlite3_result_error_code = dlsym(orig_sqlite_handle, "sqlite3_result_error_code");
    orig_sqlite3_result_error_nomem = dlsym(orig_sqlite_handle, "sqlite3_result_error_nomem");
    orig_sqlite3_result_value = dlsym(orig_sqlite_handle, "sqlite3_result_value");
    orig_sqlite3_aggregate_context = dlsym(orig_sqlite_handle, "sqlite3_aggregate_context");
    orig_sqlite3_create_function = dlsym(orig_sqlite_handle, "sqlite3_create_function");
    orig_sqlite3_create_collation = dlsym(orig_sqlite_handle, "sqlite3_create_collation");
    orig_sqlite3_data_count = dlsym(orig_sqlite_handle, "sqlite3_data_count");
    orig_sqlite3_enable_shared_cache = dlsym(orig_sqlite_handle, "sqlite3_enable_shared_cache");
    orig_sqlite3_db_handle = dlsym(orig_sqlite_handle, "sqlite3_db_handle");
    orig_sqlite3_column_value = dlsym(orig_sqlite_handle, "sqlite3_column_value");
    orig_sqlite3_value_type = dlsym(orig_sqlite_handle, "sqlite3_value_type");
    orig_sqlite3_value_int = dlsym(orig_sqlite_handle, "sqlite3_value_int");
    orig_sqlite3_value_int64 = dlsym(orig_sqlite_handle, "sqlite3_value_int64");
    orig_sqlite3_value_double = dlsym(orig_sqlite_handle, "sqlite3_value_double");
    orig_sqlite3_value_text = dlsym(orig_sqlite_handle, "sqlite3_value_text");
    orig_sqlite3_value_bytes = dlsym(orig_sqlite_handle, "sqlite3_value_bytes");
    orig_sqlite3_value_blob = dlsym(orig_sqlite_handle, "sqlite3_value_blob");
    orig_sqlite3_prepare_v3 = dlsym(orig_sqlite_handle, "sqlite3_prepare_v3");
    orig_sqlite3_prepare = dlsym(orig_sqlite_handle, "sqlite3_prepare");
    orig_sqlite3_bind_text64 = dlsym(orig_sqlite_handle, "sqlite3_bind_text64");
    orig_sqlite3_bind_blob64 = dlsym(orig_sqlite_handle, "sqlite3_bind_blob64");
    orig_sqlite3_bind_value = dlsym(orig_sqlite_handle, "sqlite3_bind_value");
    orig_sqlite3_trace_v2 = dlsym(orig_sqlite_handle, "sqlite3_trace_v2");
    orig_sqlite3_backup_init = dlsym(orig_sqlite_handle, "sqlite3_backup_init");
    orig_sqlite3_backup_step = dlsym(orig_sqlite_handle, "sqlite3_backup_step");
    orig_sqlite3_backup_remaining = dlsym(orig_sqlite_handle, "sqlite3_backup_remaining");
    orig_sqlite3_backup_pagecount = dlsym(orig_sqlite_handle, "sqlite3_backup_pagecount");
    orig_sqlite3_backup_finish = dlsym(orig_sqlite_handle, "sqlite3_backup_finish");
    orig_sqlite3_busy_handler = dlsym(orig_sqlite_handle, "sqlite3_busy_handler");
    orig_sqlite3_expanded_sql = dlsym(orig_sqlite_handle, "sqlite3_expanded_sql");
    orig_sqlite3_create_module = dlsym(orig_sqlite_handle, "sqlite3_create_module");
    orig_sqlite3_mprintf = dlsym(orig_sqlite_handle, "sqlite3_mprintf");
    orig_sqlite3_vmprintf = dlsym(orig_sqlite_handle, "sqlite3_vmprintf");
    orig_sqlite3_vtab_on_conflict = dlsym(orig_sqlite_handle, "sqlite3_vtab_on_conflict");
    orig_sqlite3_stricmp = dlsym(orig_sqlite_handle, "sqlite3_stricmp");
    orig_sqlite3_declare_vtab = dlsym(orig_sqlite_handle, "sqlite3_declare_vtab");
    orig_sqlite3_user_data = dlsym(orig_sqlite_handle, "sqlite3_user_data");
    orig_sqlite3_context_db_handle = dlsym(orig_sqlite_handle, "sqlite3_context_db_handle");
    orig_sqlite3_db_config = dlsym(orig_sqlite_handle, "sqlite3_db_config");
    orig_sqlite3_stmt_readonly = dlsym(orig_sqlite_handle, "sqlite3_stmt_readonly");
    orig_sqlite3_sql = dlsym(orig_sqlite_handle, "sqlite3_sql");
    orig_sqlite3_get_auxdata = dlsym(orig_sqlite_handle, "sqlite3_get_auxdata");
    orig_sqlite3_set_auxdata = dlsym(orig_sqlite_handle, "sqlite3_set_auxdata");
    orig_sqlite3_sourceid = dlsym(orig_sqlite_handle, "sqlite3_sourceid");

    pthread_mutex_unlock(&orig_sqlite_mutex);
    return 1;
}

static void init_original_functions(void) {
    if (orig_sqlite3_open) return;  // Already initialized
    load_original_sqlite();
}

// ============================================================================
// Logging
// ============================================================================

static void log_message(const char *fmt, ...) {
    if (!log_file) {
        log_file = fopen(LOG_FILE, "a");
        if (!log_file) return;
        setbuf(log_file, NULL);
    }

    pthread_mutex_lock(&log_mutex);

    time_t now = time(NULL);
    struct tm *tm = localtime(&now);
    fprintf(log_file, "[%04d-%02d-%02d %02d:%02d:%02d] ",
            tm->tm_year + 1900, tm->tm_mon + 1, tm->tm_mday,
            tm->tm_hour, tm->tm_min, tm->tm_sec);

    va_list args;
    va_start(args, fmt);
    vfprintf(log_file, fmt, args);
    va_end(args);

    fprintf(log_file, "\n");

    pthread_mutex_unlock(&log_mutex);
}

// ============================================================================
// Configuration
// ============================================================================

static void load_config(void) {
    if (config_loaded) return;

    const char *val;

    val = getenv(ENV_PG_HOST);
    strncpy(pg_config.host, val ? val : "localhost", sizeof(pg_config.host) - 1);

    val = getenv(ENV_PG_PORT);
    pg_config.port = val ? atoi(val) : 5432;

    val = getenv(ENV_PG_DATABASE);
    strncpy(pg_config.database, val ? val : "plex", sizeof(pg_config.database) - 1);

    val = getenv(ENV_PG_USER);
    strncpy(pg_config.user, val ? val : "plex", sizeof(pg_config.user) - 1);

    val = getenv(ENV_PG_PASSWORD);
    strncpy(pg_config.password, val ? val : "", sizeof(pg_config.password) - 1);

    val = getenv(ENV_PG_SCHEMA);
    strncpy(pg_config.schema, val ? val : "plex", sizeof(pg_config.schema) - 1);

    config_loaded = 1;

    log_message("PostgreSQL config: %s@%s:%d/%s (schema: %s)",
                pg_config.user, pg_config.host, pg_config.port,
                pg_config.database, pg_config.schema);
}

// ============================================================================
// Helper Functions
// ============================================================================

static int should_redirect(const char *filename) {
    if (!filename) return 0;

    for (int i = 0; REDIRECT_PATTERNS[i]; i++) {
        if (strstr(filename, REDIRECT_PATTERNS[i])) {
            return 1;
        }
    }
    return 0;
}

// SQLite-specific functions/commands to skip on PostgreSQL
static const char *SQLITE_ONLY_PATTERNS[] = {
    "icu_load_collation",
    "fts3_tokenizer",
    "fts4",
    "fts5",
    "SELECT load_extension",
    "VACUUM",
    "PRAGMA",
    "REINDEX",
    "ANALYZE sqlite_",
    "ATTACH DATABASE",
    "DETACH DATABASE",
    "ROLLBACK",
    "SAVEPOINT",
    "RELEASE SAVEPOINT",
    "CREATE VIRTUAL",
    "spellfix",
    "metadata_item_clusterings GROUP BY",
    "metadata_item_views",
    "grandparents.id",
    "parents.id=metadata_items.parent_id",
    NULL
};

static int is_sqlite_only(const char *sql) {
    if (!sql) return 0;

    while (*sql && (*sql == ' ' || *sql == '\t' || *sql == '\n')) sql++;

    for (int i = 0; SQLITE_ONLY_PATTERNS[i]; i++) {
        if (strncasecmp(sql, SQLITE_ONLY_PATTERNS[i], strlen(SQLITE_ONLY_PATTERNS[i])) == 0) {
            return 1;
        }
        if (strcasestr(sql, SQLITE_ONLY_PATTERNS[i])) {
            return 1;
        }
    }
    return 0;
}

static pg_connection_t* find_pg_connection(sqlite3 *db) {
    pthread_mutex_lock(&conn_mutex);
    for (int i = 0; i < connection_count; i++) {
        // Check if db IS the pg_connection (PostgreSQL-only mode)
        // OR if db matches the shadow_db (legacy shadow mode)
        if (connections[i] &&
            ((pg_connection_t*)db == connections[i] || connections[i]->shadow_db == db)) {
            pthread_mutex_unlock(&conn_mutex);
            return connections[i];
        }
    }
    // Debug: log lookup failure for non-NULL db
    if (db && connection_count > 0) {
        log_message("FIND_CONN FAILED: db=%p, conn_count=%d, first_shadow=%p, last_shadow=%p",
                    (void*)db, connection_count,
                    connections[0] ? (void*)connections[0]->shadow_db : NULL,
                    connections[connection_count-1] ? (void*)connections[connection_count-1]->shadow_db : NULL);
    }
    pthread_mutex_unlock(&conn_mutex);
    return NULL;
}

static int is_write_operation(const char *sql) {
    if (!sql) return 0;

    while (*sql && (*sql == ' ' || *sql == '\t' || *sql == '\n')) sql++;

    if (strncasecmp(sql, "INSERT", 6) == 0) return 1;
    if (strncasecmp(sql, "UPDATE", 6) == 0) return 1;
    if (strncasecmp(sql, "DELETE", 6) == 0) return 1;
    if (strncasecmp(sql, "REPLACE", 7) == 0) return 1;
    if (strncasecmp(sql, "CREATE", 6) == 0) return 1;
    if (strncasecmp(sql, "DROP", 4) == 0) return 1;
    if (strncasecmp(sql, "ALTER", 5) == 0) return 1;

    return 0;
}

static int is_read_operation(const char *sql) {
    if (!sql) return 0;

    while (*sql && (*sql == ' ' || *sql == '\t' || *sql == '\n')) sql++;

    if (strncasecmp(sql, "SELECT", 6) == 0) return 1;

    return 0;
}

static void register_stmt(sqlite3_stmt *sqlite_stmt, pg_stmt_t *pg_stmt) {
    pthread_mutex_lock(&stmt_mutex);
    if (stmt_count < MAX_STATEMENTS) {
        stmt_map[stmt_count].sqlite_stmt = sqlite_stmt;
        stmt_map[stmt_count].pg_stmt = pg_stmt;
        stmt_count++;
    }
    pthread_mutex_unlock(&stmt_mutex);
}

static pg_stmt_t* find_pg_stmt(sqlite3_stmt *sqlite_stmt) {
    pthread_mutex_lock(&stmt_mutex);
    for (int i = 0; i < stmt_count; i++) {
        if (stmt_map[i].sqlite_stmt == sqlite_stmt) {
            pg_stmt_t *result = stmt_map[i].pg_stmt;
            pthread_mutex_unlock(&stmt_mutex);
            return result;
        }
    }
    pthread_mutex_unlock(&stmt_mutex);
    return NULL;
}

// Check if this is a PostgreSQL-only statement (no SQLite backend)
static int is_pg_only_stmt(sqlite3_stmt *pStmt) {
    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);
    if (pg_stmt && pg_stmt->conn) {
        return (pg_stmt->conn->shadow_db == NULL);
    }
    return 0;
}

static void unregister_stmt(sqlite3_stmt *sqlite_stmt) {
    pthread_mutex_lock(&stmt_mutex);
    for (int i = 0; i < stmt_count; i++) {
        if (stmt_map[i].sqlite_stmt == sqlite_stmt) {
            pg_stmt_t *pg_stmt = stmt_map[i].pg_stmt;
            if (pg_stmt) {
                if (pg_stmt->sql) free(pg_stmt->sql);
                if (pg_stmt->pg_sql) free(pg_stmt->pg_sql);
                if (pg_stmt->result && pPQclear) pPQclear(pg_stmt->result);
                for (int j = 0; j < pg_stmt->param_count; j++) {
                    if (pg_stmt->param_values[j]) free(pg_stmt->param_values[j]);
                }
                free(pg_stmt);
            }
            stmt_map[i] = stmt_map[--stmt_count];
            break;
        }
    }
    pthread_mutex_unlock(&stmt_mutex);
}

// ============================================================================
// PostgreSQL Connection Management
// ============================================================================

static pg_connection_t* pg_connect(const char *db_path, sqlite3 *shadow_db) {
    load_config();

    // Try to load libpq
    if (!load_libpq()) {
        log_message("Failed to load libpq, PostgreSQL disabled");
        return NULL;
    }

    char conninfo[1024];
    snprintf(conninfo, sizeof(conninfo),
             "host=%s port=%d dbname=%s user=%s password=%s "
             "connect_timeout=10 application_name=plex_shim",
             pg_config.host, pg_config.port, pg_config.database,
             pg_config.user, pg_config.password);

    PGconn *conn = pPQconnectdb(conninfo);

    if (pPQstatus(conn) != CONNECTION_OK) {
        log_message("PostgreSQL connection failed: %s", pPQerrorMessage(conn));
        pPQfinish(conn);
        return NULL;
    }

    // Create schema if it doesn't exist
    char create_schema_sql[256];
    snprintf(create_schema_sql, sizeof(create_schema_sql),
             "CREATE SCHEMA IF NOT EXISTS %s", pg_config.schema);
    PGresult *create_res = pPQexec(conn, create_schema_sql);
    if (pPQresultStatus(create_res) != PGRES_COMMAND_OK) {
        log_message("Warning: CREATE SCHEMA failed: %s", pPQerrorMessage(conn));
    }
    pPQclear(create_res);

    // Set search path to use the schema
    char schema_sql[256];
    snprintf(schema_sql, sizeof(schema_sql),
             "SET search_path TO %s, public", pg_config.schema);
    PGresult *res = pPQexec(conn, schema_sql);
    if (pPQresultStatus(res) != PGRES_COMMAND_OK) {
        log_message("Warning: SET search_path failed: %s", pPQerrorMessage(conn));
    }
    pPQclear(res);

    log_message("PostgreSQL schema set to: %s", pg_config.schema);

    pg_connection_t *pg_conn = calloc(1, sizeof(pg_connection_t));
    pg_conn->conn = conn;
    pg_conn->shadow_db = shadow_db;
    pg_conn->is_pg_active = 1;
    strncpy(pg_conn->db_path, db_path, sizeof(pg_conn->db_path) - 1);
    pthread_mutex_init(&pg_conn->mutex, NULL);

    pthread_mutex_lock(&conn_mutex);
    if (connection_count < MAX_CONNECTIONS) {
        connections[connection_count++] = pg_conn;
    }
    pthread_mutex_unlock(&conn_mutex);

    log_message("PostgreSQL connected for: %s", db_path);

    return pg_conn;
}

// ============================================================================
// Interposed SQLite Functions
// ============================================================================

#define PG_READ_ENABLED 0
#define SQLITE_DISABLED 1

int sqlite3_open(const char *filename, sqlite3 **ppDb) {
    init_original_functions();

    log_message("OPEN: %s (redirect=%d)", filename ? filename : "(null)", should_redirect(filename));

    if (!orig_sqlite3_open) {
        log_message("ERROR: orig_sqlite3_open is NULL!");
        return SQLITE_ERROR;
    }

    // Always open the real SQLite database for Plex compatibility
    int rc = orig_sqlite3_open(filename, ppDb);

    if (rc == SQLITE_OK && should_redirect(filename)) {
        // Also connect to PostgreSQL for data operations
        log_message("OPEN: Shadow mode - SQLite + PostgreSQL for: %s, db=%p", filename, (void*)*ppDb);
        pg_connection_t *pg_conn = pg_connect(filename, *ppDb);
        if (pg_conn) {
            log_message("PostgreSQL shadow connection established for db=%p", (void*)*ppDb);
        } else {
            log_message("PostgreSQL connection failed, using SQLite only");
        }
    }

    return rc;
}

int sqlite3_open_v2(const char *filename, sqlite3 **ppDb, int flags, const char *zVfs) {
    init_original_functions();

    log_message("OPEN_V2: %s flags=0x%x (redirect=%d)",
                filename ? filename : "(null)", flags, should_redirect(filename));

    if (!orig_sqlite3_open_v2) {
        log_message("ERROR: orig_sqlite3_open_v2 is NULL!");
        return SQLITE_ERROR;
    }

    // Always open the real SQLite database for Plex compatibility
    int rc = orig_sqlite3_open_v2(filename, ppDb, flags, zVfs);

    if (rc == SQLITE_OK && should_redirect(filename)) {
        // Also connect to PostgreSQL for data operations
        log_message("OPEN_V2: Shadow mode - SQLite + PostgreSQL for: %s", filename);
        pg_connection_t *pg_conn = pg_connect(filename, *ppDb);
        if (pg_conn) {
            log_message("PostgreSQL shadow connection established");
        } else {
            log_message("PostgreSQL connection failed, using SQLite only");
        }
    }

    return rc;
}

int sqlite3_close(sqlite3 *db) {
    init_original_functions();

    pg_connection_t *pg_conn = find_pg_connection(db);
    if (pg_conn) {
        log_message("CLOSE: PostgreSQL connection for %s", pg_conn->db_path);

        int is_pg_only = (pg_conn->shadow_db == NULL);

        if (pg_conn->conn && pPQfinish) {
            pPQfinish(pg_conn->conn);
            pg_conn->conn = NULL;
        }

        pthread_mutex_lock(&conn_mutex);
        for (int i = 0; i < connection_count; i++) {
            if (connections[i] == pg_conn) {
                connections[i] = connections[--connection_count];
                break;
            }
        }
        pthread_mutex_unlock(&conn_mutex);

        pthread_mutex_destroy(&pg_conn->mutex);
        free(pg_conn);

        // If PostgreSQL-only mode, don't call original SQLite close
        if (is_pg_only) {
            return SQLITE_OK;
        }
    }

    return orig_sqlite3_close ? orig_sqlite3_close(db) : SQLITE_OK;
}

int sqlite3_close_v2(sqlite3 *db) {
    init_original_functions();

    pg_connection_t *pg_conn = find_pg_connection(db);
    if (pg_conn) {
        int is_pg_only = (pg_conn->shadow_db == NULL);

        if (pg_conn->conn && pPQfinish) {
            pPQfinish(pg_conn->conn);
            pg_conn->conn = NULL;
        }

        pthread_mutex_lock(&conn_mutex);
        for (int i = 0; i < connection_count; i++) {
            if (connections[i] == pg_conn) {
                connections[i] = connections[--connection_count];
                break;
            }
        }
        pthread_mutex_unlock(&conn_mutex);

        pthread_mutex_destroy(&pg_conn->mutex);
        free(pg_conn);

        if (is_pg_only) {
            return SQLITE_OK;
        }
    }

    return orig_sqlite3_close_v2 ? orig_sqlite3_close_v2(db) : SQLITE_OK;
}

int sqlite3_exec(sqlite3 *db, const char *sql,
                 int (*callback)(void*, int, char**, char**),
                 void *arg, char **errmsg) {
    init_original_functions();

    pg_connection_t *pg_conn = find_pg_connection(db);

    if (pg_conn && pg_conn->conn && pg_conn->is_pg_active && pPQexec) {
        int is_pg_only = (pg_conn->shadow_db == NULL);

        if (is_sqlite_only(sql)) {
            // Skip SQLite-specific commands
            if (is_pg_only) return SQLITE_OK;
        } else {
            sql_translation_t trans = sql_translate(sql);
            if (trans.success && trans.sql) {
                log_message("EXEC PG: %s", trans.sql);
                PGresult *res = pPQexec(pg_conn->conn, trans.sql);
                ExecStatusType status = pPQresultStatus(res);
                if (status != PGRES_COMMAND_OK && status != PGRES_TUPLES_OK) {
                    log_message("PostgreSQL exec error: %s", pPQerrorMessage(pg_conn->conn));
                    if (errmsg) {
                        *errmsg = strdup(pPQerrorMessage(pg_conn->conn));
                    }
                    pPQclear(res);
                    sql_translation_free(&trans);
                    return SQLITE_ERROR;
                }
                pPQclear(res);
            }
            sql_translation_free(&trans);
        }

        // PostgreSQL-only mode - don't call original SQLite
        if (is_pg_only) {
            return SQLITE_OK;
        }
    }

    return orig_sqlite3_exec ? orig_sqlite3_exec(db, sql, callback, arg, errmsg) : SQLITE_ERROR;
}

int sqlite3_prepare_v2(sqlite3 *db, const char *zSql, int nByte,
                       sqlite3_stmt **ppStmt, const char **pzTail) {
    init_original_functions();

    pg_connection_t *pg_conn = find_pg_connection(db);
    int is_pg_only = pg_conn && (pg_conn->shadow_db == NULL);

    // For PostgreSQL-only mode, we create a fake sqlite3_stmt
    if (is_pg_only) {
        int is_write = is_write_operation(zSql);
        int is_read = is_read_operation(zSql);

        if (is_sqlite_only(zSql)) {
            // For SQLite-only queries, return a dummy statement
            *ppStmt = (sqlite3_stmt*)calloc(1, sizeof(void*));
            if (pzTail) *pzTail = zSql + (nByte > 0 ? nByte : strlen(zSql));
            return SQLITE_OK;
        }

        pg_stmt_t *pg_stmt = calloc(1, sizeof(pg_stmt_t));
        if (!pg_stmt) {
            *ppStmt = NULL;
            return SQLITE_NOMEM;
        }

        pg_stmt->conn = pg_conn;
        pg_stmt->sql = strdup(zSql);
        pg_stmt->is_pg = is_write ? 1 : 2;

        sql_translation_t trans = sql_translate(zSql);
        if (trans.success && trans.sql) {
            pg_stmt->pg_sql = strdup(trans.sql);
            log_message("PREPARE PG-ONLY: %s", trans.sql);
        }
        sql_translation_free(&trans);

        const char *p = zSql;
        pg_stmt->param_count = 0;
        while (*p) {
            if (*p == '?') pg_stmt->param_count++;
            p++;
        }

        // Create a fake statement handle (use pg_stmt pointer itself)
        *ppStmt = (sqlite3_stmt*)pg_stmt;
        register_stmt(*ppStmt, pg_stmt);

        if (pzTail) *pzTail = zSql + (nByte > 0 ? nByte : strlen(zSql));
        return SQLITE_OK;
    }

    // Standard path with SQLite shadow
    int rc = orig_sqlite3_prepare_v2 ? orig_sqlite3_prepare_v2(db, zSql, nByte, ppStmt, pzTail) : SQLITE_ERROR;

    if (rc != SQLITE_OK || !*ppStmt) {
        // For CREATE statements, suppress "already exists" errors
        // This makes CREATE TABLE behave like CREATE TABLE IF NOT EXISTS
        if (rc == SQLITE_ERROR && strncasecmp(zSql, "CREATE", 6) == 0) {
            const char *errmsg = orig_sqlite3_errmsg ? orig_sqlite3_errmsg(db) : NULL;
            if (errmsg && (strstr(errmsg, "already exists") || strstr(errmsg, "table exists"))) {
                log_message("PREPARE CREATE: Suppressed 'already exists' for: %.100s", zSql);
                // Create a dummy pg_stmt to handle step/finalize correctly
                pg_stmt_t *dummy = calloc(1, sizeof(pg_stmt_t));
                if (!dummy) return SQLITE_NOMEM;
                dummy->is_pg = 3;  // 3 = dummy statement
                dummy->sql = strdup(zSql);
                *ppStmt = (sqlite3_stmt*)dummy;
                register_stmt(*ppStmt, dummy);
                if (pzTail) *pzTail = zSql + (nByte > 0 ? nByte : strlen(zSql));
                return SQLITE_OK;
            }
        }

        // For ALTER TABLE ADD COLUMN, suppress "duplicate column" errors
        if (rc == SQLITE_ERROR && strncasecmp(zSql, "ALTER", 5) == 0) {
            const char *errmsg = orig_sqlite3_errmsg ? orig_sqlite3_errmsg(db) : NULL;
            if (errmsg && strstr(errmsg, "duplicate column")) {
                log_message("PREPARE ALTER: Suppressed 'duplicate column' for: %.100s", zSql);
                pg_stmt_t *dummy = calloc(1, sizeof(pg_stmt_t));
                if (!dummy) return SQLITE_NOMEM;
                dummy->is_pg = 3;  // 3 = dummy statement
                dummy->sql = strdup(zSql);
                *ppStmt = (sqlite3_stmt*)dummy;
                register_stmt(*ppStmt, dummy);
                if (pzTail) *pzTail = zSql + (nByte > 0 ? nByte : strlen(zSql));
                return SQLITE_OK;
            }
        }

        return rc;
    }

    int is_write = is_write_operation(zSql);
    int is_read = is_read_operation(zSql);

    // Log CREATE statements for debugging
    if (strncasecmp(zSql, "CREATE", 6) == 0) {
        log_message("PREPARE CREATE: db=%p, pg_conn=%p, active=%d, is_write=%d, sql=%.200s",
                    (void*)db, (void*)pg_conn,
                    pg_conn ? pg_conn->is_pg_active : -1, is_write, zSql);
    }

    if (pg_conn && pg_conn->conn && pg_conn->is_pg_active && (is_write || is_read)) {
        if (is_sqlite_only(zSql)) {
            log_message("PREPARE SKIP (sqlite_only): %.200s", zSql);
            return rc;
        }

        pg_stmt_t *pg_stmt = calloc(1, sizeof(pg_stmt_t));
        if (pg_stmt) {
            pg_stmt->conn = pg_conn;
            pg_stmt->sql = strdup(zSql);
            pg_stmt->is_pg = is_write ? 1 : 2;

            sql_translation_t trans = sql_translate(zSql);
            if (trans.success && trans.sql) {
                pg_stmt->pg_sql = strdup(trans.sql);
                if (is_write) {
                    log_message("TRANSLATE OK: %.300s", trans.sql);
                }
            } else {
                if (is_write) {
                    log_message("TRANSLATE FAIL: %s for: %.200s", trans.error, zSql);
                }
            }
            sql_translation_free(&trans);

            const char *p = zSql;
            pg_stmt->param_count = 0;
            while (*p) {
                if (*p == '?') pg_stmt->param_count++;
                p++;
            }

            register_stmt(*ppStmt, pg_stmt);
        }
    }

    return rc;
}

int sqlite3_bind_int(sqlite3_stmt *pStmt, int idx, int val) {
    init_original_functions();
    int pg_only = is_pg_only_stmt(pStmt);

    int rc = pg_only ? SQLITE_OK : (orig_sqlite3_bind_int ? orig_sqlite3_bind_int(pStmt, idx, val) : SQLITE_OK);

    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS) {
        char buf[32];
        snprintf(buf, sizeof(buf), "%d", val);
        if (pg_stmt->param_values[idx-1]) free(pg_stmt->param_values[idx-1]);
        pg_stmt->param_values[idx-1] = strdup(buf);
        pg_stmt->param_lengths[idx-1] = 0;
        pg_stmt->param_formats[idx-1] = 0;
        if (idx > pg_stmt->param_count) pg_stmt->param_count = idx;
    }

    return rc;
}

int sqlite3_bind_int64(sqlite3_stmt *pStmt, int idx, sqlite3_int64 val) {
    init_original_functions();
    int pg_only = is_pg_only_stmt(pStmt);

    int rc = pg_only ? SQLITE_OK : (orig_sqlite3_bind_int64 ? orig_sqlite3_bind_int64(pStmt, idx, val) : SQLITE_OK);

    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS) {
        char buf[32];
        snprintf(buf, sizeof(buf), "%lld", (long long)val);
        if (pg_stmt->param_values[idx-1]) free(pg_stmt->param_values[idx-1]);
        pg_stmt->param_values[idx-1] = strdup(buf);
        pg_stmt->param_lengths[idx-1] = 0;
        pg_stmt->param_formats[idx-1] = 0;
        if (idx > pg_stmt->param_count) pg_stmt->param_count = idx;
    }

    return rc;
}

int sqlite3_bind_double(sqlite3_stmt *pStmt, int idx, double val) {
    init_original_functions();
    int pg_only = is_pg_only_stmt(pStmt);

    int rc = pg_only ? SQLITE_OK : (orig_sqlite3_bind_double ? orig_sqlite3_bind_double(pStmt, idx, val) : SQLITE_OK);

    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS) {
        char buf[64];
        snprintf(buf, sizeof(buf), "%.17g", val);
        if (pg_stmt->param_values[idx-1]) free(pg_stmt->param_values[idx-1]);
        pg_stmt->param_values[idx-1] = strdup(buf);
        pg_stmt->param_lengths[idx-1] = 0;
        pg_stmt->param_formats[idx-1] = 0;
        if (idx > pg_stmt->param_count) pg_stmt->param_count = idx;
    }

    return rc;
}

int sqlite3_bind_text(sqlite3_stmt *pStmt, int idx, const char *val,
                      int nBytes, void (*destructor)(void*)) {
    init_original_functions();
    int pg_only = is_pg_only_stmt(pStmt);

    int rc = pg_only ? SQLITE_OK : (orig_sqlite3_bind_text ? orig_sqlite3_bind_text(pStmt, idx, val, nBytes, destructor) : SQLITE_OK);

    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS && val) {
        if (pg_stmt->param_values[idx-1]) free(pg_stmt->param_values[idx-1]);
        if (nBytes < 0) {
            pg_stmt->param_values[idx-1] = strdup(val);
        } else {
            pg_stmt->param_values[idx-1] = malloc(nBytes + 1);
            if (pg_stmt->param_values[idx-1]) {
                memcpy(pg_stmt->param_values[idx-1], val, nBytes);
                pg_stmt->param_values[idx-1][nBytes] = '\0';
            }
        }
        pg_stmt->param_lengths[idx-1] = 0;
        pg_stmt->param_formats[idx-1] = 0;
        if (idx > pg_stmt->param_count) pg_stmt->param_count = idx;
    }

    return rc;
}

int sqlite3_bind_blob(sqlite3_stmt *pStmt, int idx, const void *val,
                      int nBytes, void (*destructor)(void*)) {
    init_original_functions();
    int pg_only = is_pg_only_stmt(pStmt);

    int rc = pg_only ? SQLITE_OK : (orig_sqlite3_bind_blob ? orig_sqlite3_bind_blob(pStmt, idx, val, nBytes, destructor) : SQLITE_OK);

    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS && val && nBytes > 0) {
        if (pg_stmt->param_values[idx-1]) free(pg_stmt->param_values[idx-1]);
        pg_stmt->param_values[idx-1] = malloc(nBytes);
        if (pg_stmt->param_values[idx-1]) {
            memcpy(pg_stmt->param_values[idx-1], val, nBytes);
        }
        pg_stmt->param_lengths[idx-1] = nBytes;
        pg_stmt->param_formats[idx-1] = 1;
        if (idx > pg_stmt->param_count) pg_stmt->param_count = idx;
    }

    return rc;
}

int sqlite3_bind_null(sqlite3_stmt *pStmt, int idx) {
    init_original_functions();
    int pg_only = is_pg_only_stmt(pStmt);

    int rc = pg_only ? SQLITE_OK : (orig_sqlite3_bind_null ? orig_sqlite3_bind_null(pStmt, idx) : SQLITE_OK);

    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS) {
        if (pg_stmt->param_values[idx-1]) {
            free(pg_stmt->param_values[idx-1]);
            pg_stmt->param_values[idx-1] = NULL;
        }
        pg_stmt->param_lengths[idx-1] = 0;
        pg_stmt->param_formats[idx-1] = 0;
        if (idx > pg_stmt->param_count) pg_stmt->param_count = idx;
    }

    return rc;
}

int sqlite3_step(sqlite3_stmt *pStmt) {
    init_original_functions();
    int pg_only = is_pg_only_stmt(pStmt);

    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);

    // Handle dummy statements (from suppressed "already exists" errors)
    if (pg_stmt && pg_stmt->is_pg == 3) {
        return SQLITE_DONE;
    }

    if (pg_stmt && pg_stmt->pg_sql && pg_stmt->conn && pg_stmt->conn->conn && pPQexecParams) {
        if (pg_stmt->is_pg == 2) {
            if (!pg_stmt->result) {
                const char *paramValues[MAX_PARAMS];
                int paramLengths[MAX_PARAMS];
                int paramFormats[MAX_PARAMS];

                for (int i = 0; i < pg_stmt->param_count && i < MAX_PARAMS; i++) {
                    paramValues[i] = pg_stmt->param_values[i];
                    paramLengths[i] = pg_stmt->param_lengths[i];
                    paramFormats[i] = pg_stmt->param_formats[i];
                }

                pg_stmt->result = pPQexecParams(
                    pg_stmt->conn->conn,
                    pg_stmt->pg_sql,
                    pg_stmt->param_count,
                    NULL,
                    paramValues,
                    paramLengths,
                    paramFormats,
                    0
                );

                ExecStatusType status = pPQresultStatus(pg_stmt->result);
                if (status == PGRES_TUPLES_OK) {
                    pg_stmt->num_rows = pPQntuples(pg_stmt->result);
                    pg_stmt->num_cols = pPQnfields(pg_stmt->result);
                    pg_stmt->current_row = 0;
                    log_message("STEP READ: %d rows, %d cols for: %.500s",
                                pg_stmt->num_rows, pg_stmt->num_cols, pg_stmt->pg_sql);
                } else if (status != PGRES_COMMAND_OK) {
                    log_message("STEP PG read error: %s\nSQL: %s",
                                pPQerrorMessage(pg_stmt->conn->conn), pg_stmt->pg_sql);
                    pPQclear(pg_stmt->result);
                    pg_stmt->result = NULL;
                    // Reset failed transaction state
                    PGresult *rollback = pPQexec(pg_stmt->conn->conn, "ROLLBACK");
                    pPQclear(rollback);
                }
            } else {
                pg_stmt->current_row++;
            }

            // For PG-only or when PG_READ_ENABLED, return result from PostgreSQL
            if (pg_only || PG_READ_ENABLED) {
                if (pg_stmt->result) {
                    if (pg_stmt->current_row < pg_stmt->num_rows) {
                        return SQLITE_ROW;
                    } else {
                        return SQLITE_DONE;
                    }
                }
                return SQLITE_DONE;
            }
        }
        else if (pg_stmt->is_pg == 1) {
            const char *paramValues[MAX_PARAMS];
            int paramLengths[MAX_PARAMS];
            int paramFormats[MAX_PARAMS];

            for (int i = 0; i < pg_stmt->param_count && i < MAX_PARAMS; i++) {
                paramValues[i] = pg_stmt->param_values[i];
                paramLengths[i] = pg_stmt->param_lengths[i];
                paramFormats[i] = pg_stmt->param_formats[i];
            }

            PGresult *res = pPQexecParams(
                pg_stmt->conn->conn,
                pg_stmt->pg_sql,
                pg_stmt->param_count,
                NULL,
                paramValues,
                paramLengths,
                paramFormats,
                0
            );

            ExecStatusType status = pPQresultStatus(res);
            if (status != PGRES_COMMAND_OK && status != PGRES_TUPLES_OK) {
                log_message("STEP PG write error: %s\nSQL: %s",
                            pPQerrorMessage(pg_stmt->conn->conn), pg_stmt->pg_sql);
                pPQclear(res);
                // Reset failed transaction state
                PGresult *rollback = pPQexec(pg_stmt->conn->conn, "ROLLBACK");
                pPQclear(rollback);
            } else {
                log_message("STEP PG write OK: %.500s", pg_stmt->pg_sql);
                pPQclear(res);
            }

            // For PG-only mode, return DONE for write operations
            if (pg_only) {
                return SQLITE_DONE;
            }
        }
    }

    // For PG-only statements without pg_sql (like PRAGMA), return DONE
    if (pg_only) {
        return SQLITE_DONE;
    }

    int step_rc = orig_sqlite3_step ? orig_sqlite3_step(pStmt) : SQLITE_DONE;

    // Suppress UNIQUE constraint errors for INSERT (treat as INSERT OR IGNORE)
    if (step_rc == SQLITE_CONSTRAINT && pg_stmt && pg_stmt->sql &&
        strncasecmp(pg_stmt->sql, "INSERT", 6) == 0) {
        log_message("STEP: Suppressed UNIQUE constraint for: %.100s", pg_stmt->sql);
        return SQLITE_DONE;
    }

    return step_rc;
}

int sqlite3_reset(sqlite3_stmt *pStmt) {
    init_original_functions();
    int pg_only = is_pg_only_stmt(pStmt);

    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);

    if (pg_stmt) {
        for (int i = 0; i < MAX_PARAMS; i++) {
            if (pg_stmt->param_values[i]) {
                free(pg_stmt->param_values[i]);
                pg_stmt->param_values[i] = NULL;
            }
            pg_stmt->param_lengths[i] = 0;
            pg_stmt->param_formats[i] = 0;
        }

        if (pg_stmt->result && pPQclear) {
            pPQclear(pg_stmt->result);
            pg_stmt->result = NULL;
        }
        pg_stmt->current_row = 0;
    }

    // Handle dummy statements (is_pg == 3)
    if (pg_only || (pg_stmt && pg_stmt->is_pg == 3)) {
        return SQLITE_OK;
    }

    return orig_sqlite3_reset ? orig_sqlite3_reset(pStmt) : SQLITE_OK;
}

int sqlite3_finalize(sqlite3_stmt *pStmt) {
    init_original_functions();
    int pg_only = is_pg_only_stmt(pStmt);

    // Check for dummy statements before unregistering
    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);
    int is_dummy = pg_stmt && pg_stmt->is_pg == 3;

    unregister_stmt(pStmt);

    if (pg_only || is_dummy) {
        // For PG-only or dummy, pStmt IS the pg_stmt, which was freed by unregister_stmt
        return SQLITE_OK;
    }

    return orig_sqlite3_finalize ? orig_sqlite3_finalize(pStmt) : SQLITE_OK;
}

int sqlite3_clear_bindings(sqlite3_stmt *pStmt) {
    init_original_functions();
    int pg_only = is_pg_only_stmt(pStmt);

    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);

    if (pg_stmt) {
        for (int i = 0; i < MAX_PARAMS; i++) {
            if (pg_stmt->param_values[i]) {
                free(pg_stmt->param_values[i]);
                pg_stmt->param_values[i] = NULL;
            }
            pg_stmt->param_lengths[i] = 0;
            pg_stmt->param_formats[i] = 0;
        }
    }

    if (pg_only) {
        return SQLITE_OK;
    }

    return orig_sqlite3_clear_bindings ? orig_sqlite3_clear_bindings(pStmt) : SQLITE_OK;
}

// ============================================================================
// Column Value Interception
// ============================================================================

static int pg_value_to_int(const char *val) {
    if (!val || !*val) return 0;
    if (val[0] == 't' && val[1] == '\0') return 1;
    if (val[0] == 'f' && val[1] == '\0') return 0;
    return atoi(val);
}

static sqlite3_int64 pg_value_to_int64(const char *val) {
    if (!val || !*val) return 0;
    if (val[0] == 't' && val[1] == '\0') return 1;
    if (val[0] == 'f' && val[1] == '\0') return 0;
    return atoll(val);
}

static double pg_value_to_double(const char *val) {
    if (!val || !*val) return 0.0;
    if (val[0] == 't' && val[1] == '\0') return 1.0;
    if (val[0] == 'f' && val[1] == '\0') return 0.0;
    return atof(val);
}

int sqlite3_column_count(sqlite3_stmt *pStmt) {
    init_original_functions();
    int pg_only = is_pg_only_stmt(pStmt);

    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result) {
        return pg_stmt->num_cols;
    }

    if (pg_only) return 0;
    return orig_sqlite3_column_count ? orig_sqlite3_column_count(pStmt) : 0;
}

int sqlite3_column_type(sqlite3_stmt *pStmt, int idx) {
    init_original_functions();
    int pg_only = is_pg_only_stmt(pStmt);

    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result && pPQgetisnull && pPQftype) {
        if (pg_stmt->current_row < pg_stmt->num_rows) {
            if (pPQgetisnull(pg_stmt->result, pg_stmt->current_row, idx)) {
                return SQLITE_NULL;
            }
            Oid oid = pPQftype(pg_stmt->result, idx);
            switch (oid) {
                case 16:   // BOOL
                case 23:   // INT4
                case 20:   // INT8
                case 21:   // INT2
                    return SQLITE_INTEGER;
                case 700:  // FLOAT4
                case 701:  // FLOAT8
                case 1700: // NUMERIC
                    return SQLITE_FLOAT;
                case 17:   // BYTEA
                    return SQLITE_BLOB;
                default:
                    return SQLITE_TEXT;
            }
        }
    }

    if (pg_only) return SQLITE_NULL;
    return orig_sqlite3_column_type ? orig_sqlite3_column_type(pStmt, idx) : SQLITE_NULL;
}

int sqlite3_column_int(sqlite3_stmt *pStmt, int idx) {
    init_original_functions();
    int pg_only = is_pg_only_stmt(pStmt);

    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result && pPQgetisnull && pPQgetvalue) {
        if (pg_stmt->current_row < pg_stmt->num_rows &&
            !pPQgetisnull(pg_stmt->result, pg_stmt->current_row, idx)) {
            return pg_value_to_int(pPQgetvalue(pg_stmt->result, pg_stmt->current_row, idx));
        }
        return 0;
    }

    if (pg_only) return 0;
    return orig_sqlite3_column_int ? orig_sqlite3_column_int(pStmt, idx) : 0;
}

sqlite3_int64 sqlite3_column_int64(sqlite3_stmt *pStmt, int idx) {
    init_original_functions();
    int pg_only = is_pg_only_stmt(pStmt);

    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result && pPQgetisnull && pPQgetvalue) {
        if (pg_stmt->current_row < pg_stmt->num_rows &&
            !pPQgetisnull(pg_stmt->result, pg_stmt->current_row, idx)) {
            return pg_value_to_int64(pPQgetvalue(pg_stmt->result, pg_stmt->current_row, idx));
        }
        return 0;
    }

    if (pg_only) return 0;
    return orig_sqlite3_column_int64 ? orig_sqlite3_column_int64(pStmt, idx) : 0;
}

double sqlite3_column_double(sqlite3_stmt *pStmt, int idx) {
    init_original_functions();
    int pg_only = is_pg_only_stmt(pStmt);

    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result && pPQgetisnull && pPQgetvalue) {
        if (pg_stmt->current_row < pg_stmt->num_rows &&
            !pPQgetisnull(pg_stmt->result, pg_stmt->current_row, idx)) {
            return pg_value_to_double(pPQgetvalue(pg_stmt->result, pg_stmt->current_row, idx));
        }
        return 0.0;
    }

    if (pg_only) return 0.0;
    return orig_sqlite3_column_double ? orig_sqlite3_column_double(pStmt, idx) : 0.0;
}

const unsigned char* sqlite3_column_text(sqlite3_stmt *pStmt, int idx) {
    init_original_functions();
    int pg_only = is_pg_only_stmt(pStmt);

    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result && pPQgetisnull && pPQgetvalue) {
        if (pg_stmt->current_row < pg_stmt->num_rows) {
            if (pPQgetisnull(pg_stmt->result, pg_stmt->current_row, idx)) {
                return NULL;
            }
            const char *val = pPQgetvalue(pg_stmt->result, pg_stmt->current_row, idx);
            return (const unsigned char*)val;
        }
        return NULL;
    }

    if (pg_only) return NULL;
    return orig_sqlite3_column_text ? orig_sqlite3_column_text(pStmt, idx) : NULL;
}

const void* sqlite3_column_blob(sqlite3_stmt *pStmt, int idx) {
    init_original_functions();
    int pg_only = is_pg_only_stmt(pStmt);

    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result && pPQgetisnull && pPQgetvalue) {
        if (pg_stmt->current_row < pg_stmt->num_rows &&
            !pPQgetisnull(pg_stmt->result, pg_stmt->current_row, idx)) {
            return pPQgetvalue(pg_stmt->result, pg_stmt->current_row, idx);
        }
        return NULL;
    }

    if (pg_only) return NULL;
    return orig_sqlite3_column_blob ? orig_sqlite3_column_blob(pStmt, idx) : NULL;
}

int sqlite3_column_bytes(sqlite3_stmt *pStmt, int idx) {
    init_original_functions();
    int pg_only = is_pg_only_stmt(pStmt);

    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result && pPQgetisnull && pPQgetlength) {
        if (pg_stmt->current_row < pg_stmt->num_rows &&
            !pPQgetisnull(pg_stmt->result, pg_stmt->current_row, idx)) {
            return pPQgetlength(pg_stmt->result, pg_stmt->current_row, idx);
        }
        return 0;
    }

    if (pg_only) return 0;
    return orig_sqlite3_column_bytes ? orig_sqlite3_column_bytes(pStmt, idx) : 0;
}

const char* sqlite3_column_name(sqlite3_stmt *pStmt, int idx) {
    init_original_functions();
    int pg_only = is_pg_only_stmt(pStmt);

    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result && pPQfname) {
        return pPQfname(pg_stmt->result, idx);
    }

    if (pg_only) return NULL;
    return orig_sqlite3_column_name ? orig_sqlite3_column_name(pStmt, idx) : NULL;
}

// ============================================================================
// Forwarded SQLite Functions (not intercepted, just forwarded to original)
// ============================================================================

void sqlite3_free(void *p) {
    init_original_functions();
    if (orig_sqlite3_free) orig_sqlite3_free(p);
}

void *sqlite3_malloc(int n) {
    init_original_functions();
    return orig_sqlite3_malloc ? orig_sqlite3_malloc(n) : NULL;
}

void *sqlite3_malloc64(sqlite3_uint64 n) {
    init_original_functions();
    return orig_sqlite3_malloc64 ? orig_sqlite3_malloc64(n) : NULL;
}

void *sqlite3_realloc(void *p, int n) {
    init_original_functions();
    return orig_sqlite3_realloc ? orig_sqlite3_realloc(p, n) : NULL;
}

void *sqlite3_realloc64(void *p, sqlite3_uint64 n) {
    init_original_functions();
    return orig_sqlite3_realloc64 ? orig_sqlite3_realloc64(p, n) : NULL;
}

int sqlite3_busy_timeout(sqlite3 *db, int ms) {
    init_original_functions();
    return orig_sqlite3_busy_timeout ? orig_sqlite3_busy_timeout(db, ms) : SQLITE_ERROR;
}

int sqlite3_get_autocommit(sqlite3 *db) {
    init_original_functions();
    return orig_sqlite3_get_autocommit ? orig_sqlite3_get_autocommit(db) : 1;
}

int sqlite3_changes(sqlite3 *db) {
    init_original_functions();
    return orig_sqlite3_changes ? orig_sqlite3_changes(db) : 0;
}

sqlite3_int64 sqlite3_changes64(sqlite3 *db) {
    init_original_functions();
    return orig_sqlite3_changes64 ? orig_sqlite3_changes64(db) : 0;
}

int sqlite3_total_changes(sqlite3 *db) {
    init_original_functions();
    return orig_sqlite3_total_changes ? orig_sqlite3_total_changes(db) : 0;
}

sqlite3_int64 sqlite3_total_changes64(sqlite3 *db) {
    init_original_functions();
    return orig_sqlite3_total_changes64 ? orig_sqlite3_total_changes64(db) : 0;
}

sqlite3_int64 sqlite3_last_insert_rowid(sqlite3 *db) {
    init_original_functions();
    return orig_sqlite3_last_insert_rowid ? orig_sqlite3_last_insert_rowid(db) : 0;
}

const char *sqlite3_errmsg(sqlite3 *db) {
    init_original_functions();
    return orig_sqlite3_errmsg ? orig_sqlite3_errmsg(db) : "unknown error";
}

int sqlite3_errcode(sqlite3 *db) {
    init_original_functions();
    return orig_sqlite3_errcode ? orig_sqlite3_errcode(db) : SQLITE_ERROR;
}

int sqlite3_extended_errcode(sqlite3 *db) {
    init_original_functions();
    return orig_sqlite3_extended_errcode ? orig_sqlite3_extended_errcode(db) : SQLITE_ERROR;
}

int sqlite3_error_offset(sqlite3 *db) {
    init_original_functions();
    return orig_sqlite3_error_offset ? orig_sqlite3_error_offset(db) : -1;
}

const char *sqlite3_libversion(void) {
    init_original_functions();
    return orig_sqlite3_libversion ? orig_sqlite3_libversion() : "unknown";
}

int sqlite3_libversion_number(void) {
    init_original_functions();
    return orig_sqlite3_libversion_number ? orig_sqlite3_libversion_number() : 0;
}

int sqlite3_threadsafe(void) {
    init_original_functions();
    return orig_sqlite3_threadsafe ? orig_sqlite3_threadsafe() : 0;
}

int sqlite3_initialize(void) {
    init_original_functions();
    return orig_sqlite3_initialize ? orig_sqlite3_initialize() : SQLITE_OK;
}

int sqlite3_shutdown(void) {
    init_original_functions();
    return orig_sqlite3_shutdown ? orig_sqlite3_shutdown() : SQLITE_OK;
}

void sqlite3_interrupt(sqlite3 *db) {
    init_original_functions();
    if (orig_sqlite3_interrupt) orig_sqlite3_interrupt(db);
}

int sqlite3_complete(const char *sql) {
    init_original_functions();
    return orig_sqlite3_complete ? orig_sqlite3_complete(sql) : 0;
}

int sqlite3_sleep(int ms) {
    init_original_functions();
    return orig_sqlite3_sleep ? orig_sqlite3_sleep(ms) : 0;
}

void sqlite3_randomness(int n, void *p) {
    init_original_functions();
    if (orig_sqlite3_randomness) orig_sqlite3_randomness(n, p);
}

int sqlite3_limit(sqlite3 *db, int id, int newVal) {
    init_original_functions();
    return orig_sqlite3_limit ? orig_sqlite3_limit(db, id, newVal) : -1;
}

int sqlite3_bind_parameter_count(sqlite3_stmt *pStmt) {
    init_original_functions();
    return orig_sqlite3_bind_parameter_count ? orig_sqlite3_bind_parameter_count(pStmt) : 0;
}

const char *sqlite3_bind_parameter_name(sqlite3_stmt *pStmt, int i) {
    init_original_functions();
    return orig_sqlite3_bind_parameter_name ? orig_sqlite3_bind_parameter_name(pStmt, i) : NULL;
}

int sqlite3_bind_parameter_index(sqlite3_stmt *pStmt, const char *zName) {
    init_original_functions();
    return orig_sqlite3_bind_parameter_index ? orig_sqlite3_bind_parameter_index(pStmt, zName) : 0;
}

int sqlite3_stmt_isexplain(sqlite3_stmt *pStmt) {
    init_original_functions();
    return orig_sqlite3_stmt_isexplain ? orig_sqlite3_stmt_isexplain(pStmt) : 0;
}

int sqlite3_stmt_status(sqlite3_stmt *pStmt, int op, int resetFlg) {
    init_original_functions();
    return orig_sqlite3_stmt_status ? orig_sqlite3_stmt_status(pStmt, op, resetFlg) : 0;
}

int sqlite3_db_status(sqlite3 *db, int op, int *pCur, int *pHiwtr, int resetFlg) {
    init_original_functions();
    return orig_sqlite3_db_status ? orig_sqlite3_db_status(db, op, pCur, pHiwtr, resetFlg) : SQLITE_ERROR;
}

int sqlite3_status64(int op, sqlite3_int64 *pCurrent, sqlite3_int64 *pHighwater, int resetFlag) {
    init_original_functions();
    return orig_sqlite3_status64 ? orig_sqlite3_status64(op, pCurrent, pHighwater, resetFlag) : SQLITE_ERROR;
}

int sqlite3_db_readonly(sqlite3 *db, const char *zDbName) {
    init_original_functions();
    return orig_sqlite3_db_readonly ? orig_sqlite3_db_readonly(db, zDbName) : -1;
}

int sqlite3_txn_state(sqlite3 *db, const char *zSchema) {
    init_original_functions();
    return orig_sqlite3_txn_state ? orig_sqlite3_txn_state(db, zSchema) : -1;
}

int sqlite3_vfs_register(sqlite3_vfs *pVfs, int makeDflt) {
    init_original_functions();
    return orig_sqlite3_vfs_register ? orig_sqlite3_vfs_register(pVfs, makeDflt) : SQLITE_ERROR;
}

int sqlite3_vfs_unregister(sqlite3_vfs *pVfs) {
    init_original_functions();
    return orig_sqlite3_vfs_unregister ? orig_sqlite3_vfs_unregister(pVfs) : SQLITE_ERROR;
}

sqlite3_vfs *sqlite3_vfs_find(const char *zVfsName) {
    init_original_functions();
    return orig_sqlite3_vfs_find ? orig_sqlite3_vfs_find(zVfsName) : NULL;
}

int sqlite3_file_control(sqlite3 *db, const char *zDbName, int op, void *pArg) {
    init_original_functions();
    return orig_sqlite3_file_control ? orig_sqlite3_file_control(db, zDbName, op, pArg) : SQLITE_NOTFOUND;
}

int sqlite3_set_authorizer(sqlite3 *db, int(*xAuth)(void*,int,const char*,const char*,const char*,const char*), void *pUserData) {
    init_original_functions();
    return orig_sqlite3_set_authorizer ? orig_sqlite3_set_authorizer(db, xAuth, pUserData) : SQLITE_OK;
}

int sqlite3_enable_load_extension(sqlite3 *db, int onoff) {
    init_original_functions();
    return orig_sqlite3_enable_load_extension ? orig_sqlite3_enable_load_extension(db, onoff) : SQLITE_OK;
}

int sqlite3_load_extension(sqlite3 *db, const char *zFile, const char *zProc, char **pzErrMsg) {
    init_original_functions();
    return orig_sqlite3_load_extension ? orig_sqlite3_load_extension(db, zFile, zProc, pzErrMsg) : SQLITE_ERROR;
}

void sqlite3_progress_handler(sqlite3 *db, int nOps, int(*xProgress)(void*), void *pArg) {
    init_original_functions();
    if (orig_sqlite3_progress_handler) orig_sqlite3_progress_handler(db, nOps, xProgress, pArg);
}

int sqlite3_strnicmp(const char *zLeft, const char *zRight, int n) {
    init_original_functions();
    return orig_sqlite3_strnicmp ? orig_sqlite3_strnicmp(zLeft, zRight, n) : 0;
}

int sqlite3_strglob(const char *zGlob, const char *zStr) {
    init_original_functions();
    return orig_sqlite3_strglob ? orig_sqlite3_strglob(zGlob, zStr) : 0;
}

int sqlite3_strlike(const char *zGlob, const char *zStr, unsigned int cEsc) {
    init_original_functions();
    return orig_sqlite3_strlike ? orig_sqlite3_strlike(zGlob, zStr, cEsc) : 0;
}

int sqlite3_deserialize(sqlite3 *db, const char *zSchema, unsigned char *pData, sqlite3_int64 szDb, sqlite3_int64 szBuf, unsigned mFlags) {
    init_original_functions();
    return orig_sqlite3_deserialize ? orig_sqlite3_deserialize(db, zSchema, pData, szDb, szBuf, mFlags) : SQLITE_ERROR;
}

int sqlite3_table_column_metadata(sqlite3 *db, const char *zDbName, const char *zTableName, const char *zColumnName, char const **pzDataType, char const **pzCollSeq, int *pNotNull, int *pPrimaryKey, int *pAutoinc) {
    init_original_functions();
    return orig_sqlite3_table_column_metadata ? orig_sqlite3_table_column_metadata(db, zDbName, zTableName, zColumnName, pzDataType, pzCollSeq, pNotNull, pPrimaryKey, pAutoinc) : SQLITE_ERROR;
}

const char *sqlite3_column_decltype(sqlite3_stmt *pStmt, int i) {
    init_original_functions();
    return orig_sqlite3_column_decltype ? orig_sqlite3_column_decltype(pStmt, i) : NULL;
}

int sqlite3_create_function_v2(sqlite3 *db, const char *zFunctionName, int nArg, int eTextRep, void *pApp, void (*xFunc)(sqlite3_context*,int,sqlite3_value**), void (*xStep)(sqlite3_context*,int,sqlite3_value**), void (*xFinal)(sqlite3_context*), void (*xDestroy)(void*)) {
    init_original_functions();
    return orig_sqlite3_create_function_v2 ? orig_sqlite3_create_function_v2(db, zFunctionName, nArg, eTextRep, pApp, xFunc, xStep, xFinal, xDestroy) : SQLITE_ERROR;
}

int sqlite3_create_window_function(sqlite3 *db, const char *zFunctionName, int nArg, int eTextRep, void *pApp, void (*xStep)(sqlite3_context*,int,sqlite3_value**), void (*xFinal)(sqlite3_context*), void (*xValue)(sqlite3_context*), void (*xInverse)(sqlite3_context*,int,sqlite3_value**), void (*xDestroy)(void*)) {
    init_original_functions();
    return orig_sqlite3_create_window_function ? orig_sqlite3_create_window_function(db, zFunctionName, nArg, eTextRep, pApp, xStep, xFinal, xValue, xInverse, xDestroy) : SQLITE_ERROR;
}

int sqlite3_keyword_count(void) {
    init_original_functions();
    return orig_sqlite3_keyword_count ? orig_sqlite3_keyword_count() : 0;
}

int sqlite3_keyword_name(int i, const char **pzName, int *pnName) {
    init_original_functions();
    return orig_sqlite3_keyword_name ? orig_sqlite3_keyword_name(i, pzName, pnName) : SQLITE_ERROR;
}

int sqlite3_keyword_check(const char *zName, int nName) {
    init_original_functions();
    return orig_sqlite3_keyword_check ? orig_sqlite3_keyword_check(zName, nName) : 0;
}

const char *sqlite3_vtab_collation(sqlite3_index_info *pIdxInfo, int iCons) {
    init_original_functions();
    return orig_sqlite3_vtab_collation ? orig_sqlite3_vtab_collation(pIdxInfo, iCons) : NULL;
}

void sqlite3_result_blob(sqlite3_context *pCtx, const void *z, int n, void (*xDel)(void*)) {
    init_original_functions();
    if (orig_sqlite3_result_blob) orig_sqlite3_result_blob(pCtx, z, n, xDel);
}

void sqlite3_result_blob64(sqlite3_context *pCtx, const void *z, sqlite3_uint64 n, void (*xDel)(void*)) {
    init_original_functions();
    if (orig_sqlite3_result_blob64) orig_sqlite3_result_blob64(pCtx, z, n, xDel);
}

void sqlite3_result_text(sqlite3_context *pCtx, const char *z, int n, void (*xDel)(void*)) {
    init_original_functions();
    if (orig_sqlite3_result_text) orig_sqlite3_result_text(pCtx, z, n, xDel);
}

void sqlite3_result_text64(sqlite3_context *pCtx, const char *z, sqlite3_uint64 n, void (*xDel)(void*), unsigned char enc) {
    init_original_functions();
    if (orig_sqlite3_result_text64) orig_sqlite3_result_text64(pCtx, z, n, xDel, enc);
}

char *sqlite3_vsnprintf(int n, char *zBuf, const char *zFormat, va_list ap) {
    init_original_functions();
    return orig_sqlite3_vsnprintf ? orig_sqlite3_vsnprintf(n, zBuf, zFormat, ap) : NULL;
}

sqlite3_str *sqlite3_str_new(sqlite3 *db) {
    init_original_functions();
    return orig_sqlite3_str_new ? orig_sqlite3_str_new(db) : NULL;
}

void sqlite3_str_appendall(sqlite3_str *p, const char *z) {
    init_original_functions();
    if (orig_sqlite3_str_appendall) orig_sqlite3_str_appendall(p, z);
}

void sqlite3_str_append(sqlite3_str *p, const char *z, int n) {
    init_original_functions();
    if (orig_sqlite3_str_append) orig_sqlite3_str_append(p, z, n);
}

char *sqlite3_str_finish(sqlite3_str *p) {
    init_original_functions();
    return orig_sqlite3_str_finish ? orig_sqlite3_str_finish(p) : NULL;
}

// More Plex-required forwarded functions
void sqlite3_result_double(sqlite3_context *pCtx, double val) {
    init_original_functions();
    if (orig_sqlite3_result_double) orig_sqlite3_result_double(pCtx, val);
}

void sqlite3_result_null(sqlite3_context *pCtx) {
    init_original_functions();
    if (orig_sqlite3_result_null) orig_sqlite3_result_null(pCtx);
}

void sqlite3_result_int(sqlite3_context *pCtx, int val) {
    init_original_functions();
    if (orig_sqlite3_result_int) orig_sqlite3_result_int(pCtx, val);
}

void sqlite3_result_int64(sqlite3_context *pCtx, sqlite3_int64 val) {
    init_original_functions();
    if (orig_sqlite3_result_int64) orig_sqlite3_result_int64(pCtx, val);
}

void sqlite3_result_error(sqlite3_context *pCtx, const char *z, int n) {
    init_original_functions();
    if (orig_sqlite3_result_error) orig_sqlite3_result_error(pCtx, z, n);
}

void sqlite3_result_error_code(sqlite3_context *pCtx, int errCode) {
    init_original_functions();
    if (orig_sqlite3_result_error_code) orig_sqlite3_result_error_code(pCtx, errCode);
}

void sqlite3_result_error_nomem(sqlite3_context *pCtx) {
    init_original_functions();
    if (orig_sqlite3_result_error_nomem) orig_sqlite3_result_error_nomem(pCtx);
}

void sqlite3_result_value(sqlite3_context *pCtx, sqlite3_value *pValue) {
    init_original_functions();
    if (orig_sqlite3_result_value) orig_sqlite3_result_value(pCtx, pValue);
}

void *sqlite3_aggregate_context(sqlite3_context *pCtx, int nBytes) {
    init_original_functions();
    return orig_sqlite3_aggregate_context ? orig_sqlite3_aggregate_context(pCtx, nBytes) : NULL;
}

int sqlite3_create_function(sqlite3 *db, const char *zFunctionName, int nArg, int eTextRep, void *pApp, void (*xFunc)(sqlite3_context*,int,sqlite3_value**), void (*xStep)(sqlite3_context*,int,sqlite3_value**), void (*xFinal)(sqlite3_context*)) {
    init_original_functions();
    return orig_sqlite3_create_function ? orig_sqlite3_create_function(db, zFunctionName, nArg, eTextRep, pApp, xFunc, xStep, xFinal) : SQLITE_ERROR;
}

int sqlite3_create_collation(sqlite3 *db, const char *zName, int eTextRep, void *pArg, int(*xCompare)(void*,int,const void*,int,const void*)) {
    init_original_functions();
    return orig_sqlite3_create_collation ? orig_sqlite3_create_collation(db, zName, eTextRep, pArg, xCompare) : SQLITE_ERROR;
}

int sqlite3_data_count(sqlite3_stmt *pStmt) {
    init_original_functions();
    return orig_sqlite3_data_count ? orig_sqlite3_data_count(pStmt) : 0;
}

int sqlite3_enable_shared_cache(int enable) {
    init_original_functions();
    return orig_sqlite3_enable_shared_cache ? orig_sqlite3_enable_shared_cache(enable) : SQLITE_OK;
}

sqlite3 *sqlite3_db_handle(sqlite3_stmt *pStmt) {
    init_original_functions();
    return orig_sqlite3_db_handle ? orig_sqlite3_db_handle(pStmt) : NULL;
}

sqlite3_value *sqlite3_column_value(sqlite3_stmt *pStmt, int iCol) {
    init_original_functions();
    return orig_sqlite3_column_value ? orig_sqlite3_column_value(pStmt, iCol) : NULL;
}

int sqlite3_value_type(sqlite3_value *pVal) {
    init_original_functions();
    return orig_sqlite3_value_type ? orig_sqlite3_value_type(pVal) : SQLITE_NULL;
}

int sqlite3_value_int(sqlite3_value *pVal) {
    init_original_functions();
    return orig_sqlite3_value_int ? orig_sqlite3_value_int(pVal) : 0;
}

sqlite3_int64 sqlite3_value_int64(sqlite3_value *pVal) {
    init_original_functions();
    return orig_sqlite3_value_int64 ? orig_sqlite3_value_int64(pVal) : 0;
}

double sqlite3_value_double(sqlite3_value *pVal) {
    init_original_functions();
    return orig_sqlite3_value_double ? orig_sqlite3_value_double(pVal) : 0.0;
}

const unsigned char *sqlite3_value_text(sqlite3_value *pVal) {
    init_original_functions();
    return orig_sqlite3_value_text ? orig_sqlite3_value_text(pVal) : NULL;
}

int sqlite3_value_bytes(sqlite3_value *pVal) {
    init_original_functions();
    return orig_sqlite3_value_bytes ? orig_sqlite3_value_bytes(pVal) : 0;
}

const void *sqlite3_value_blob(sqlite3_value *pVal) {
    init_original_functions();
    return orig_sqlite3_value_blob ? orig_sqlite3_value_blob(pVal) : NULL;
}

int sqlite3_prepare_v3(sqlite3 *db, const char *zSql, int nByte, unsigned int prepFlags, sqlite3_stmt **ppStmt, const char **pzTail) {
    init_original_functions();
    return orig_sqlite3_prepare_v3 ? orig_sqlite3_prepare_v3(db, zSql, nByte, prepFlags, ppStmt, pzTail) : SQLITE_ERROR;
}

int sqlite3_prepare(sqlite3 *db, const char *zSql, int nByte, sqlite3_stmt **ppStmt, const char **pzTail) {
    init_original_functions();
    return orig_sqlite3_prepare ? orig_sqlite3_prepare(db, zSql, nByte, ppStmt, pzTail) : SQLITE_ERROR;
}

int sqlite3_bind_text64(sqlite3_stmt *pStmt, int i, const char *zData, sqlite3_uint64 nData, void (*xDel)(void*), unsigned char enc) {
    init_original_functions();
    return orig_sqlite3_bind_text64 ? orig_sqlite3_bind_text64(pStmt, i, zData, nData, xDel, enc) : SQLITE_ERROR;
}

int sqlite3_bind_blob64(sqlite3_stmt *pStmt, int i, const void *zData, sqlite3_uint64 nData, void (*xDel)(void*)) {
    init_original_functions();
    return orig_sqlite3_bind_blob64 ? orig_sqlite3_bind_blob64(pStmt, i, zData, nData, xDel) : SQLITE_ERROR;
}

int sqlite3_bind_value(sqlite3_stmt *pStmt, int i, const sqlite3_value *pValue) {
    init_original_functions();
    return orig_sqlite3_bind_value ? orig_sqlite3_bind_value(pStmt, i, pValue) : SQLITE_ERROR;
}

int sqlite3_trace_v2(sqlite3 *db, unsigned uMask, int(*xCallback)(unsigned,void*,void*,void*), void *pCtx) {
    init_original_functions();
    return orig_sqlite3_trace_v2 ? orig_sqlite3_trace_v2(db, uMask, xCallback, pCtx) : SQLITE_OK;
}

sqlite3_backup *sqlite3_backup_init(sqlite3 *pDest, const char *zDestName, sqlite3 *pSource, const char *zSourceName) {
    init_original_functions();
    return orig_sqlite3_backup_init ? orig_sqlite3_backup_init(pDest, zDestName, pSource, zSourceName) : NULL;
}

int sqlite3_backup_step(sqlite3_backup *p, int nPage) {
    init_original_functions();
    return orig_sqlite3_backup_step ? orig_sqlite3_backup_step(p, nPage) : SQLITE_ERROR;
}

int sqlite3_backup_remaining(sqlite3_backup *p) {
    init_original_functions();
    return orig_sqlite3_backup_remaining ? orig_sqlite3_backup_remaining(p) : 0;
}

int sqlite3_backup_pagecount(sqlite3_backup *p) {
    init_original_functions();
    return orig_sqlite3_backup_pagecount ? orig_sqlite3_backup_pagecount(p) : 0;
}

int sqlite3_backup_finish(sqlite3_backup *p) {
    init_original_functions();
    return orig_sqlite3_backup_finish ? orig_sqlite3_backup_finish(p) : SQLITE_ERROR;
}

int sqlite3_busy_handler(sqlite3 *db, int(*xBusy)(void*,int), void *pArg) {
    init_original_functions();
    return orig_sqlite3_busy_handler ? orig_sqlite3_busy_handler(db, xBusy, pArg) : SQLITE_OK;
}

char *sqlite3_expanded_sql(sqlite3_stmt *pStmt) {
    init_original_functions();
    return orig_sqlite3_expanded_sql ? orig_sqlite3_expanded_sql(pStmt) : NULL;
}

int sqlite3_create_module(sqlite3 *db, const char *zName, const sqlite3_module *pModule, void *pAux) {
    init_original_functions();
    return orig_sqlite3_create_module ? orig_sqlite3_create_module(db, zName, pModule, pAux) : SQLITE_ERROR;
}

char *sqlite3_vmprintf(const char *zFormat, va_list ap) {
    init_original_functions();
    return orig_sqlite3_vmprintf ? orig_sqlite3_vmprintf(zFormat, ap) : NULL;
}

int sqlite3_vtab_on_conflict(sqlite3 *db) {
    init_original_functions();
    return orig_sqlite3_vtab_on_conflict ? orig_sqlite3_vtab_on_conflict(db) : 0;
}

int sqlite3_stricmp(const char *zLeft, const char *zRight) {
    init_original_functions();
    return orig_sqlite3_stricmp ? orig_sqlite3_stricmp(zLeft, zRight) : 0;
}

int sqlite3_declare_vtab(sqlite3 *db, const char *zSQL) {
    init_original_functions();
    return orig_sqlite3_declare_vtab ? orig_sqlite3_declare_vtab(db, zSQL) : SQLITE_ERROR;
}

void *sqlite3_user_data(sqlite3_context *pCtx) {
    init_original_functions();
    return orig_sqlite3_user_data ? orig_sqlite3_user_data(pCtx) : NULL;
}

sqlite3 *sqlite3_context_db_handle(sqlite3_context *pCtx) {
    init_original_functions();
    return orig_sqlite3_context_db_handle ? orig_sqlite3_context_db_handle(pCtx) : NULL;
}

int sqlite3_stmt_readonly(sqlite3_stmt *pStmt) {
    init_original_functions();
    return orig_sqlite3_stmt_readonly ? orig_sqlite3_stmt_readonly(pStmt) : 0;
}

const char *sqlite3_sql(sqlite3_stmt *pStmt) {
    init_original_functions();
    return orig_sqlite3_sql ? orig_sqlite3_sql(pStmt) : NULL;
}

void *sqlite3_get_auxdata(sqlite3_context *pCtx, int N) {
    init_original_functions();
    return orig_sqlite3_get_auxdata ? orig_sqlite3_get_auxdata(pCtx, N) : NULL;
}

void sqlite3_set_auxdata(sqlite3_context *pCtx, int N, void *pAux, void (*xDelete)(void*)) {
    init_original_functions();
    if (orig_sqlite3_set_auxdata) orig_sqlite3_set_auxdata(pCtx, N, pAux, xDelete);
}

const char *sqlite3_sourceid(void) {
    init_original_functions();
    return orig_sqlite3_sourceid ? orig_sqlite3_sourceid() : "unknown";
}

// Variadic functions - need special handling
char *sqlite3_mprintf(const char *zFormat, ...) {
    init_original_functions();
    if (!orig_sqlite3_vmprintf) return NULL;
    va_list ap;
    va_start(ap, zFormat);
    char *result = orig_sqlite3_vmprintf(zFormat, ap);
    va_end(ap);
    return result;
}

char *sqlite3_snprintf(int n, char *zBuf, const char *zFormat, ...) {
    init_original_functions();
    if (!orig_sqlite3_vsnprintf) return zBuf;
    va_list ap;
    va_start(ap, zFormat);
    char *result = orig_sqlite3_vsnprintf(n, zBuf, zFormat, ap);
    va_end(ap);
    return result;
}

void sqlite3_str_appendf(sqlite3_str *p, const char *zFormat, ...) {
    init_original_functions();
    // sqlite3_str_appendf uses sqlite3_str_vappendf internally, but we don't have access to that
    // For now, just append the format string directly if we can't forward
    if (orig_sqlite3_str_appendall) {
        // Basic fallback - just append the format string
        // This won't handle format specifiers but avoids crashes
        orig_sqlite3_str_appendall(p, zFormat);
    }
}

// These are variadic but we can provide stubs that work for common cases
int sqlite3_config(int op, ...) {
    // sqlite3_config is typically called before any db operations
    // We can safely return OK for most operations
    return SQLITE_OK;
}

int sqlite3_db_config(sqlite3 *db, int op, ...) {
    // Similar to sqlite3_config, return OK for compatibility
    return SQLITE_OK;
}

int sqlite3_vtab_config(sqlite3 *db, int op, ...) {
    // Virtual table config - return OK
    return SQLITE_OK;
}

int sqlite3_test_control(int op, ...) {
    // Test control - return 0 for unknown operations
    return 0;
}

// ============================================================================
// Constructor/Destructor
// ============================================================================

__attribute__((constructor))
static void shim_init(void) {
    init_original_functions();
    log_message("=== Plex PostgreSQL LD_PRELOAD Shim loaded (dlopen mode) ===");
    sql_translator_init();
    load_config();
    shim_initialized = 1;
}

__attribute__((destructor))
static void shim_cleanup(void) {
    log_message("=== Plex PostgreSQL LD_PRELOAD Shim unloading ===");

    pthread_mutex_lock(&conn_mutex);
    for (int i = 0; i < connection_count; i++) {
        if (connections[i] && connections[i]->conn && pPQfinish) {
            pPQfinish(connections[i]->conn);
        }
    }
    connection_count = 0;
    pthread_mutex_unlock(&conn_mutex);

    sql_translator_cleanup();

    if (libpq_handle) {
        dlclose(libpq_handle);
        libpq_handle = NULL;
    }

    if (log_file) {
        fclose(log_file);
        log_file = NULL;
    }
}
