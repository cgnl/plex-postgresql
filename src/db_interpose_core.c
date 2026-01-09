/*
 * Plex PostgreSQL Interposing Shim - Core Module
 *
 * This is the main entry point containing:
 * - Global shared state definitions
 * - Original SQLite function pointers (for fishhook rebinding)
 * - Worker thread implementation
 * - Constructor/destructor with fishhook initialization
 */

#include "db_interpose.h"
#include "pg_query_cache.h"
#include "fishhook.h"
#include <execinfo.h>
#include <signal.h>

// ============================================================================
// Crash/Exit Handler for Debugging
// ============================================================================
static void print_backtrace(const char *reason) {
    void *callstack[128];
    int frames = backtrace(callstack, 128);
    char **symbols = backtrace_symbols(callstack, frames);
    
    fprintf(stderr, "\n=== BACKTRACE (%s) ===\n", reason);
    LOG_ERROR("=== BACKTRACE (%s) ===", reason);
    
    for (int i = 0; i < frames; i++) {
        fprintf(stderr, "  [%d] %s\n", i, symbols[i]);
        LOG_ERROR("  [%d] %s", i, symbols[i]);
    }
    fprintf(stderr, "=== END BACKTRACE ===\n\n");
    fflush(stderr);
    
    free(symbols);
}

static void signal_handler(int sig) {
    const char *sig_name = "UNKNOWN";
    switch(sig) {
        case SIGSEGV: sig_name = "SIGSEGV"; break;
        case SIGABRT: sig_name = "SIGABRT"; break;
        case SIGBUS: sig_name = "SIGBUS"; break;
        case SIGFPE: sig_name = "SIGFPE"; break;
        case SIGILL: sig_name = "SIGILL"; break;
    }
    print_backtrace(sig_name);
    
    // Re-raise to get default behavior
    signal(sig, SIG_DFL);
    raise(sig);
}

static void exit_handler(void) {
    print_backtrace("EXIT/ATEXIT");
}

// ============================================================================
// Global State Definitions (exported via db_interpose.h)
// ============================================================================

// Recursion prevention
__thread int in_interpose_call = 0;
__thread int prepare_v2_depth = 0;

// SQLite library handle for dlsym fallback
void *sqlite_handle = NULL;

// Original SQLite function pointers (populated by fishhook rebind_symbols)
// These are called by our my_* implementations to invoke the real SQLite
int (*orig_sqlite3_open)(const char*, sqlite3**) = NULL;
int (*orig_sqlite3_open_v2)(const char*, sqlite3**, int, const char*) = NULL;
int (*orig_sqlite3_close)(sqlite3*) = NULL;
int (*orig_sqlite3_close_v2)(sqlite3*) = NULL;
int (*orig_sqlite3_exec)(sqlite3*, const char*, int(*)(void*,int,char**,char**), void*, char**) = NULL;
int (*orig_sqlite3_changes)(sqlite3*) = NULL;
sqlite3_int64 (*orig_sqlite3_changes64)(sqlite3*) = NULL;
sqlite3_int64 (*orig_sqlite3_last_insert_rowid)(sqlite3*) = NULL;
int (*orig_sqlite3_get_table)(sqlite3*, const char*, char***, int*, int*, char**) = NULL;

const char* (*orig_sqlite3_errmsg)(sqlite3*) = NULL;
int (*orig_sqlite3_errcode)(sqlite3*) = NULL;
int (*orig_sqlite3_extended_errcode)(sqlite3*) = NULL;

int (*orig_sqlite3_prepare)(sqlite3*, const char*, int, sqlite3_stmt**, const char**) = NULL;
int (*orig_sqlite3_prepare_v2)(sqlite3*, const char*, int, sqlite3_stmt**, const char**) = NULL;
int (*orig_sqlite3_prepare_v3)(sqlite3*, const char*, int, unsigned int, sqlite3_stmt**, const char**) = NULL;
int (*orig_sqlite3_prepare16_v2)(sqlite3*, const void*, int, sqlite3_stmt**, const void**) = NULL;

int (*orig_sqlite3_bind_int)(sqlite3_stmt*, int, int) = NULL;
int (*orig_sqlite3_bind_int64)(sqlite3_stmt*, int, sqlite3_int64) = NULL;
int (*orig_sqlite3_bind_double)(sqlite3_stmt*, int, double) = NULL;
int (*orig_sqlite3_bind_text)(sqlite3_stmt*, int, const char*, int, void(*)(void*)) = NULL;
int (*orig_sqlite3_bind_text64)(sqlite3_stmt*, int, const char*, sqlite3_uint64, void(*)(void*), unsigned char) = NULL;
int (*orig_sqlite3_bind_blob)(sqlite3_stmt*, int, const void*, int, void(*)(void*)) = NULL;
int (*orig_sqlite3_bind_blob64)(sqlite3_stmt*, int, const void*, sqlite3_uint64, void(*)(void*)) = NULL;
int (*orig_sqlite3_bind_value)(sqlite3_stmt*, int, const sqlite3_value*) = NULL;
int (*orig_sqlite3_bind_null)(sqlite3_stmt*, int) = NULL;

int (*orig_sqlite3_step)(sqlite3_stmt*) = NULL;
int (*orig_sqlite3_reset)(sqlite3_stmt*) = NULL;
int (*orig_sqlite3_finalize)(sqlite3_stmt*) = NULL;
int (*orig_sqlite3_clear_bindings)(sqlite3_stmt*) = NULL;

int (*orig_sqlite3_column_count)(sqlite3_stmt*) = NULL;
int (*orig_sqlite3_column_type)(sqlite3_stmt*, int) = NULL;
int (*orig_sqlite3_column_int)(sqlite3_stmt*, int) = NULL;
sqlite3_int64 (*orig_sqlite3_column_int64)(sqlite3_stmt*, int) = NULL;
double (*orig_sqlite3_column_double)(sqlite3_stmt*, int) = NULL;
const unsigned char* (*orig_sqlite3_column_text)(sqlite3_stmt*, int) = NULL;
const void* (*orig_sqlite3_column_blob)(sqlite3_stmt*, int) = NULL;
int (*orig_sqlite3_column_bytes)(sqlite3_stmt*, int) = NULL;
const char* (*orig_sqlite3_column_name)(sqlite3_stmt*, int) = NULL;
sqlite3_value* (*orig_sqlite3_column_value)(sqlite3_stmt*, int) = NULL;
int (*orig_sqlite3_data_count)(sqlite3_stmt*) = NULL;

int (*orig_sqlite3_value_type)(sqlite3_value*) = NULL;
const unsigned char* (*orig_sqlite3_value_text)(sqlite3_value*) = NULL;
int (*orig_sqlite3_value_int)(sqlite3_value*) = NULL;
sqlite3_int64 (*orig_sqlite3_value_int64)(sqlite3_value*) = NULL;
double (*orig_sqlite3_value_double)(sqlite3_value*) = NULL;
int (*orig_sqlite3_value_bytes)(sqlite3_value*) = NULL;
const void* (*orig_sqlite3_value_blob)(sqlite3_value*) = NULL;

int (*orig_sqlite3_create_collation)(sqlite3*, const char*, int, void*, int(*)(void*,int,const void*,int,const void*)) = NULL;
int (*orig_sqlite3_create_collation_v2)(sqlite3*, const char*, int, void*, int(*)(void*,int,const void*,int,const void*), void(*)(void*)) = NULL;

// Aliases for backward compatibility (used by prepare module)
int (*real_sqlite3_prepare_v2)(sqlite3*, const char*, int, sqlite3_stmt**, const char**) = NULL;
const char* (*real_sqlite3_errmsg)(sqlite3*) = NULL;
int (*real_sqlite3_errcode)(sqlite3*) = NULL;

// Worker thread state
pthread_t worker_thread;
pthread_mutex_t worker_mutex = PTHREAD_MUTEX_INITIALIZER;
pthread_cond_t worker_cond_request = PTHREAD_COND_INITIALIZER;
pthread_cond_t worker_cond_response = PTHREAD_COND_INITIALIZER;
worker_request_t worker_request;
volatile int worker_running = 0;

// Fake value pool for sqlite3_column_value
pg_fake_value_t fake_value_pool[MAX_FAKE_VALUES];
unsigned int fake_value_next = 0;
pthread_mutex_t fake_value_mutex = PTHREAD_MUTEX_INITIALIZER;

// Initialization flag
int shim_initialized = 0;

// ============================================================================
// Helper Functions
// ============================================================================

// Check if a pointer is one of our fake values
pg_fake_value_t* pg_check_fake_value(sqlite3_value *pVal) {
    if (!pVal) return NULL;

    // Check if pointer is in our pool
    uintptr_t ptr = (uintptr_t)pVal;
    uintptr_t pool_start = (uintptr_t)&fake_value_pool[0];
    uintptr_t pool_end = (uintptr_t)&fake_value_pool[MAX_FAKE_VALUES];

    if (ptr >= pool_start && ptr < pool_end) {
        pg_fake_value_t *fake = (pg_fake_value_t*)pVal;
        if (fake->magic == PG_FAKE_VALUE_MAGIC) {
            return fake;
        }
    }
    return NULL;
}

// Helper to check if path is library.db
int is_library_db_path(const char *path) {
    return path && strstr(path, "com.plexapp.plugins.library.db") != NULL;
}

// Simple string replace helper
char* simple_str_replace(const char *str, const char *old, const char *new_str) {
    if (!str || !old || !new_str) return NULL;

    const char *pos = strstr(str, old);
    if (!pos) return NULL;

    size_t old_len = strlen(old);
    size_t new_len = strlen(new_str);
    size_t result_len = strlen(str) - old_len + new_len;

    char *result = malloc(result_len + 1);
    if (!result) return NULL;

    size_t prefix_len = pos - str;
    memcpy(result, str, prefix_len);
    memcpy(result + prefix_len, new_str, new_len);
    strcpy(result + prefix_len + new_len, pos + old_len);

    return result;
}

// ============================================================================
// Lazy Init for Real SQLite Functions (fallback if fishhook fails)
// ============================================================================

void ensure_real_sqlite_loaded(void) {
    if (real_sqlite3_prepare_v2) return;  // Already loaded

    const char *sqlite_paths[] = {
        "/Applications/Plex Media Server.app/Contents/Frameworks/libsqlite3_orig.dylib",
        "/Applications/Plex Media Server.app/Contents/Frameworks/libsqlite3.dylib",
        "/usr/lib/libsqlite3.dylib",
        NULL
    };

    for (int i = 0; sqlite_paths[i] != NULL && !sqlite_handle; i++) {
        sqlite_handle = dlopen(sqlite_paths[i], RTLD_LAZY | RTLD_LOCAL);
    }

    if (sqlite_handle) {
        real_sqlite3_prepare_v2 = dlsym(sqlite_handle, "sqlite3_prepare_v2");
        real_sqlite3_errmsg = dlsym(sqlite_handle, "sqlite3_errmsg");
        real_sqlite3_errcode = dlsym(sqlite_handle, "sqlite3_errcode");
    }
}

// ============================================================================
// Worker Thread Implementation
// ============================================================================

static void* worker_thread_func(void *arg) {
    (void)arg;
    LOG_INFO("WORKER: Thread started with %d MB stack", WORKER_STACK_SIZE / (1024*1024));

    while (1) {
        pthread_mutex_lock(&worker_mutex);

        // Wait for work
        while (!worker_request.work_ready && worker_running) {
            pthread_cond_wait(&worker_cond_request, &worker_mutex);
        }

        if (!worker_running) {
            pthread_mutex_unlock(&worker_mutex);
            break;
        }

        worker_request.work_ready = 0;

        // Handle the request
        if (worker_request.type == WORK_SHUTDOWN) {
            worker_request.work_done = 1;
            pthread_cond_signal(&worker_cond_response);
            pthread_mutex_unlock(&worker_mutex);
            break;
        }

        if (worker_request.type == WORK_PREPARE_V2) {
            sqlite3_stmt *stmt = NULL;
            const char *tail = NULL;

            // Call internal prepare with from_worker=1 to avoid recursion
            int rc = my_sqlite3_prepare_v2_internal(
                worker_request.db,
                worker_request.zSql,
                worker_request.nByte,
                &stmt,
                &tail,
                1  // from_worker - prevents re-delegation
            );

            worker_request.stmt = stmt;
            worker_request.tail = tail;
            worker_request.result = rc;
        }

        worker_request.work_done = 1;
        pthread_cond_signal(&worker_cond_response);
        pthread_mutex_unlock(&worker_mutex);
    }

    LOG_INFO("WORKER: Thread exiting");
    return NULL;
}

int worker_init(void) {
    pthread_attr_t attr;
    if (pthread_attr_init(&attr) != 0) {
        LOG_ERROR("WORKER: Failed to init thread attributes");
        return -1;
    }

    // Set 8MB stack size
    if (pthread_attr_setstacksize(&attr, WORKER_STACK_SIZE) != 0) {
        LOG_ERROR("WORKER: Failed to set stack size");
        pthread_attr_destroy(&attr);
        return -1;
    }

    worker_running = 1;
    memset(&worker_request, 0, sizeof(worker_request));

    if (pthread_create(&worker_thread, &attr, worker_thread_func, NULL) != 0) {
        LOG_ERROR("WORKER: Failed to create thread");
        worker_running = 0;
        pthread_attr_destroy(&attr);
        return -1;
    }

    pthread_attr_destroy(&attr);
    LOG_INFO("WORKER: Initialized with %d MB stack", WORKER_STACK_SIZE / (1024*1024));
    return 0;
}

void worker_cleanup(void) {
    if (!worker_running) return;

    pthread_mutex_lock(&worker_mutex);
    worker_request.type = WORK_SHUTDOWN;
    worker_request.work_ready = 1;
    worker_running = 0;
    pthread_cond_signal(&worker_cond_request);
    pthread_mutex_unlock(&worker_mutex);

    pthread_join(worker_thread, NULL);
    LOG_INFO("WORKER: Cleaned up");
}

// Delegate prepare_v2 to worker thread (called when stack is low)
int delegate_prepare_to_worker(sqlite3 *db, const char *zSql, int nByte,
                               sqlite3_stmt **ppStmt, const char **pzTail) {
    if (!worker_running) {
        LOG_ERROR("WORKER: Not running, cannot delegate");
        return SQLITE_ERROR;
    }

    LOG_INFO("WORKER: Delegating query (%.100s)", zSql ? zSql : "NULL");

    pthread_mutex_lock(&worker_mutex);

    // Set up request
    worker_request.type = WORK_PREPARE_V2;
    worker_request.db = db;
    worker_request.zSql = zSql;
    worker_request.nByte = nByte;
    worker_request.stmt = NULL;
    worker_request.tail = NULL;
    worker_request.result = SQLITE_ERROR;
    worker_request.work_done = 0;
    worker_request.work_ready = 1;

    // Signal worker
    pthread_cond_signal(&worker_cond_request);

    // Wait for response
    while (!worker_request.work_done) {
        pthread_cond_wait(&worker_cond_response, &worker_mutex);
    }

    // Get results
    if (ppStmt) *ppStmt = worker_request.stmt;
    if (pzTail) *pzTail = worker_request.tail;
    int result = worker_request.result;

    pthread_mutex_unlock(&worker_mutex);

    LOG_INFO("WORKER: Delegation complete, rc=%d", result);
    return result;
}

// ============================================================================
// fishhook Rebinding Setup
// ============================================================================

static void setup_fishhook_rebindings(void) {
    fprintf(stderr, "[SHIM_INIT] Setting up fishhook rebindings...\n");

    // Define all rebindings - fishhook will store the original function pointers
    struct rebinding rebindings[] = {
        // Open/Close
        {"sqlite3_open", my_sqlite3_open, (void**)&orig_sqlite3_open},
        {"sqlite3_open_v2", my_sqlite3_open_v2, (void**)&orig_sqlite3_open_v2},
        {"sqlite3_close", my_sqlite3_close, (void**)&orig_sqlite3_close},
        {"sqlite3_close_v2", my_sqlite3_close_v2, (void**)&orig_sqlite3_close_v2},

        // Exec
        {"sqlite3_exec", my_sqlite3_exec, (void**)&orig_sqlite3_exec},
        {"sqlite3_get_table", my_sqlite3_get_table, (void**)&orig_sqlite3_get_table},

        // Metadata
        {"sqlite3_changes", my_sqlite3_changes, (void**)&orig_sqlite3_changes},
        {"sqlite3_changes64", my_sqlite3_changes64, (void**)&orig_sqlite3_changes64},
        {"sqlite3_last_insert_rowid", my_sqlite3_last_insert_rowid, (void**)&orig_sqlite3_last_insert_rowid},
        {"sqlite3_errmsg", my_sqlite3_errmsg, (void**)&orig_sqlite3_errmsg},
        {"sqlite3_errcode", my_sqlite3_errcode, (void**)&orig_sqlite3_errcode},
        {"sqlite3_extended_errcode", my_sqlite3_extended_errcode, (void**)&orig_sqlite3_extended_errcode},

        // Prepare
        {"sqlite3_prepare", my_sqlite3_prepare, (void**)&orig_sqlite3_prepare},
        {"sqlite3_prepare_v2", my_sqlite3_prepare_v2, (void**)&orig_sqlite3_prepare_v2},
        {"sqlite3_prepare_v3", my_sqlite3_prepare_v3, (void**)&orig_sqlite3_prepare_v3},
        {"sqlite3_prepare16_v2", my_sqlite3_prepare16_v2, (void**)&orig_sqlite3_prepare16_v2},

        // Bind
        {"sqlite3_bind_int", my_sqlite3_bind_int, (void**)&orig_sqlite3_bind_int},
        {"sqlite3_bind_int64", my_sqlite3_bind_int64, (void**)&orig_sqlite3_bind_int64},
        {"sqlite3_bind_double", my_sqlite3_bind_double, (void**)&orig_sqlite3_bind_double},
        {"sqlite3_bind_text", my_sqlite3_bind_text, (void**)&orig_sqlite3_bind_text},
        {"sqlite3_bind_text64", my_sqlite3_bind_text64, (void**)&orig_sqlite3_bind_text64},
        {"sqlite3_bind_blob", my_sqlite3_bind_blob, (void**)&orig_sqlite3_bind_blob},
        {"sqlite3_bind_blob64", my_sqlite3_bind_blob64, (void**)&orig_sqlite3_bind_blob64},
        {"sqlite3_bind_value", my_sqlite3_bind_value, (void**)&orig_sqlite3_bind_value},
        {"sqlite3_bind_null", my_sqlite3_bind_null, (void**)&orig_sqlite3_bind_null},

        // Step/Reset/Finalize
        {"sqlite3_step", my_sqlite3_step, (void**)&orig_sqlite3_step},
        {"sqlite3_reset", my_sqlite3_reset, (void**)&orig_sqlite3_reset},
        {"sqlite3_finalize", my_sqlite3_finalize, (void**)&orig_sqlite3_finalize},
        {"sqlite3_clear_bindings", my_sqlite3_clear_bindings, (void**)&orig_sqlite3_clear_bindings},

        // Column access
        {"sqlite3_column_count", my_sqlite3_column_count, (void**)&orig_sqlite3_column_count},
        {"sqlite3_column_type", my_sqlite3_column_type, (void**)&orig_sqlite3_column_type},
        {"sqlite3_column_int", my_sqlite3_column_int, (void**)&orig_sqlite3_column_int},
        {"sqlite3_column_int64", my_sqlite3_column_int64, (void**)&orig_sqlite3_column_int64},
        {"sqlite3_column_double", my_sqlite3_column_double, (void**)&orig_sqlite3_column_double},
        {"sqlite3_column_text", my_sqlite3_column_text, (void**)&orig_sqlite3_column_text},
        {"sqlite3_column_blob", my_sqlite3_column_blob, (void**)&orig_sqlite3_column_blob},
        {"sqlite3_column_bytes", my_sqlite3_column_bytes, (void**)&orig_sqlite3_column_bytes},
        {"sqlite3_column_name", my_sqlite3_column_name, (void**)&orig_sqlite3_column_name},
        {"sqlite3_column_value", my_sqlite3_column_value, (void**)&orig_sqlite3_column_value},
        {"sqlite3_data_count", my_sqlite3_data_count, (void**)&orig_sqlite3_data_count},

        // Value access
        {"sqlite3_value_type", my_sqlite3_value_type, (void**)&orig_sqlite3_value_type},
        {"sqlite3_value_text", my_sqlite3_value_text, (void**)&orig_sqlite3_value_text},
        {"sqlite3_value_int", my_sqlite3_value_int, (void**)&orig_sqlite3_value_int},
        {"sqlite3_value_int64", my_sqlite3_value_int64, (void**)&orig_sqlite3_value_int64},
        {"sqlite3_value_double", my_sqlite3_value_double, (void**)&orig_sqlite3_value_double},
        {"sqlite3_value_bytes", my_sqlite3_value_bytes, (void**)&orig_sqlite3_value_bytes},
        {"sqlite3_value_blob", my_sqlite3_value_blob, (void**)&orig_sqlite3_value_blob},

        // Collation
        {"sqlite3_create_collation", my_sqlite3_create_collation, (void**)&orig_sqlite3_create_collation},
        {"sqlite3_create_collation_v2", my_sqlite3_create_collation_v2, (void**)&orig_sqlite3_create_collation_v2},
    };

    int count = sizeof(rebindings) / sizeof(rebindings[0]);
    int result = rebind_symbols(rebindings, count);

    if (result == 0) {
        fprintf(stderr, "[SHIM_INIT] fishhook rebind_symbols succeeded for %d functions\n", count);

        // Set up aliases for backward compatibility
        real_sqlite3_prepare_v2 = orig_sqlite3_prepare_v2;
        real_sqlite3_errmsg = orig_sqlite3_errmsg;
        real_sqlite3_errcode = orig_sqlite3_errcode;

        // Verify critical functions were bound
        if (orig_sqlite3_open) {
            fprintf(stderr, "[SHIM_INIT] orig_sqlite3_open = %p\n", (void*)orig_sqlite3_open);
        } else {
            fprintf(stderr, "[SHIM_INIT] WARNING: orig_sqlite3_open is NULL!\n");
        }
        if (orig_sqlite3_prepare_v2) {
            fprintf(stderr, "[SHIM_INIT] orig_sqlite3_prepare_v2 = %p\n", (void*)orig_sqlite3_prepare_v2);
        } else {
            fprintf(stderr, "[SHIM_INIT] WARNING: orig_sqlite3_prepare_v2 is NULL!\n");
        }
    } else {
        fprintf(stderr, "[SHIM_INIT] ERROR: fishhook rebind_symbols failed with code %d\n", result);
    }
}

// ============================================================================
// Constructor/Destructor
// ============================================================================

__attribute__((constructor))
static void shim_init(void) {
    // Write to stderr first to verify constructor runs
    fprintf(stderr, "[SHIM_INIT] Constructor starting...\n");
    fflush(stderr);

    // Install signal handlers for crash debugging
    signal(SIGSEGV, signal_handler);
    signal(SIGABRT, signal_handler);
    signal(SIGBUS, signal_handler);
    signal(SIGFPE, signal_handler);
    signal(SIGILL, signal_handler);
    atexit(exit_handler);

    pg_logging_init();
    LOG_INFO("=== Plex PostgreSQL Interpose Shim loaded ===");
    LOG_ERROR("SHIM_CONSTRUCTOR: Initialization complete");

    // Also write to stderr after logging init
    fprintf(stderr, "[SHIM_INIT] Logging initialized\n");
    fflush(stderr);

    // Use fishhook to rebind SQLite symbols at runtime
    // This works with modular compilation (unlike DYLD_INTERPOSE)
    setup_fishhook_rebindings();

    // Fallback: Load SQLite library directly for functions that fishhook might miss
    const char *sqlite_paths[] = {
        "/Applications/Plex Media Server.app/Contents/Frameworks/libsqlite3_orig.dylib",
        "/Applications/Plex Media Server.app/Contents/Frameworks/libsqlite3.dylib",
        "/usr/lib/libsqlite3.dylib",
        NULL
    };

    for (int i = 0; sqlite_paths[i] != NULL && sqlite_handle == NULL; i++) {
        sqlite_handle = dlopen(sqlite_paths[i], RTLD_LAZY | RTLD_LOCAL);
        if (sqlite_handle) {
            fprintf(stderr, "[SHIM_INIT] Loaded SQLite fallback from: %s\n", sqlite_paths[i]);
            break;
        }
    }

    // If fishhook didn't set up the pointers, use dlsym as fallback
    if (!real_sqlite3_prepare_v2 && sqlite_handle) {
        real_sqlite3_prepare_v2 = dlsym(sqlite_handle, "sqlite3_prepare_v2");
        real_sqlite3_errmsg = dlsym(sqlite_handle, "sqlite3_errmsg");
        real_sqlite3_errcode = dlsym(sqlite_handle, "sqlite3_errcode");
        fprintf(stderr, "[SHIM_INIT] Used dlsym fallback for real_sqlite3_* functions\n");
    }

    LOG_INFO("Resolved real SQLite functions for recursion prevention");

    pg_config_init();
    pg_client_init();
    pg_statement_init();
    pg_query_cache_init();  // Initialize query result cache
    sql_translator_init();

    // Start worker thread with 8MB stack for heavy queries
    worker_init();

    shim_initialized = 1;

    fprintf(stderr, "[SHIM_INIT] All modules initialized\n");
    fflush(stderr);
}

__attribute__((destructor))
static void shim_cleanup(void) {
    LOG_INFO("=== Plex PostgreSQL Interpose Shim unloading ===");
    worker_cleanup();  // Stop worker thread first
    pg_statement_cleanup();
    pg_client_cleanup();
    sql_translator_cleanup();
    pg_logging_cleanup();
}
