/*
 * Plex PostgreSQL Interposing Shim - Core Module (Linux)
 *
 * This is the Linux version of the core module containing:
 * - Global shared state definitions
 * - Original SQLite function pointers (via dlsym RTLD_NEXT)
 * - Worker thread implementation
 * - Constructor/destructor
 * - LD_PRELOAD wrapper functions that call my_sqlite3_* from shared modules
 */

#define _GNU_SOURCE
#include "db_interpose.h"
#include "pg_query_cache.h"
#include "sql_translator.h"
#include <signal.h>
#include <dlfcn.h>

// execinfo.h is glibc-specific, not available on musl
#ifdef __GLIBC__
#include <execinfo.h>
#define HAS_BACKTRACE 1
#else
#define HAS_BACKTRACE 0
#endif

// ============================================================================
// Crash/Exit Handler for Debugging
// ============================================================================
static void print_backtrace(const char *reason) {
#if HAS_BACKTRACE
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
#else
    fprintf(stderr, "\n=== CRASH (%s) - backtrace not available on musl ===\n", reason);
    LOG_ERROR("=== CRASH (%s) - backtrace not available on musl ===", reason);
    fflush(stderr);
#endif
}

static void signal_handler(int sig) {
    const char *sig_name = "UNKNOWN";
    switch(sig) {
        case SIGSEGV: sig_name = "SIGSEGV"; break;
        case SIGBUS: sig_name = "SIGBUS"; break;
        case SIGFPE: sig_name = "SIGFPE"; break;
        case SIGILL: sig_name = "SIGILL"; break;
    }
    print_backtrace(sig_name);

    // Re-raise to get default behavior
    signal(sig, SIG_DFL);
    raise(sig);
}

// Only log exits if we actually crashed (non-zero exit status)
// Remove atexit handler as it triggers on all subprocess exits

// ============================================================================
// Global State Definitions (exported via db_interpose.h)
// ============================================================================

// Recursion prevention
__thread int in_interpose_call = 0;
__thread int prepare_v2_depth = 0;

// SQLite library handle for dlsym fallback
void *sqlite_handle = NULL;

// Original SQLite function pointers (populated by dlsym RTLD_NEXT)
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
sqlite3* (*orig_sqlite3_db_handle)(sqlite3_stmt*) = NULL;
char* (*orig_sqlite3_expanded_sql)(sqlite3_stmt*) = NULL;
const char* (*orig_sqlite3_sql)(sqlite3_stmt*) = NULL;
void (*orig_sqlite3_free)(void*) = NULL;
const char* (*orig_sqlite3_bind_parameter_name)(sqlite3_stmt*, int) = NULL;

int (*orig_sqlite3_value_type)(sqlite3_value*) = NULL;
const unsigned char* (*orig_sqlite3_value_text)(sqlite3_value*) = NULL;
int (*orig_sqlite3_value_int)(sqlite3_value*) = NULL;
sqlite3_int64 (*orig_sqlite3_value_int64)(sqlite3_value*) = NULL;
double (*orig_sqlite3_value_double)(sqlite3_value*) = NULL;
int (*orig_sqlite3_value_bytes)(sqlite3_value*) = NULL;
const void* (*orig_sqlite3_value_blob)(sqlite3_value*) = NULL;

int (*orig_sqlite3_create_collation)(sqlite3*, const char*, int, void*, int(*)(void*,int,const void*,int,const void*)) = NULL;
int (*orig_sqlite3_create_collation_v2)(sqlite3*, const char*, int, void*, int(*)(void*,int,const void*,int,const void*), void(*)(void*)) = NULL;

// Additional SQLite API functions (free, db_handle, sql already defined above)
void* (*orig_sqlite3_malloc)(int) = NULL;
int (*orig_sqlite3_bind_parameter_count)(sqlite3_stmt*) = NULL;
int (*orig_sqlite3_stmt_readonly)(sqlite3_stmt*) = NULL;
int (*orig_sqlite3_stmt_busy)(sqlite3_stmt*) = NULL;
int (*orig_sqlite3_stmt_status)(sqlite3_stmt*, int, int) = NULL;

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
// Lazy Init for Real SQLite Functions
// ============================================================================

void ensure_real_sqlite_loaded(void) {
    if (real_sqlite3_prepare_v2) return;  // Already loaded

    // On Linux, use RTLD_NEXT to get the real functions
    real_sqlite3_prepare_v2 = orig_sqlite3_prepare_v2;
    real_sqlite3_errmsg = orig_sqlite3_errmsg;
    real_sqlite3_errcode = orig_sqlite3_errcode;
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

    LOG_DEBUG("WORKER: Delegating query (%.100s)", zSql ? zSql : "NULL");

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

    LOG_DEBUG("WORKER: Delegation complete, rc=%d", result);
    return result;
}

// ============================================================================
// Load Original SQLite Functions via dlsym(RTLD_NEXT)
// ============================================================================

static void load_original_functions(void) {
    fprintf(stderr, "[SHIM_INIT] Loading original SQLite functions via RTLD_NEXT...\n");

    // Use RTLD_NEXT to get the real SQLite functions (next in load order)
    orig_sqlite3_open = dlsym(RTLD_NEXT, "sqlite3_open");
    orig_sqlite3_open_v2 = dlsym(RTLD_NEXT, "sqlite3_open_v2");
    orig_sqlite3_close = dlsym(RTLD_NEXT, "sqlite3_close");
    orig_sqlite3_close_v2 = dlsym(RTLD_NEXT, "sqlite3_close_v2");
    orig_sqlite3_exec = dlsym(RTLD_NEXT, "sqlite3_exec");
    orig_sqlite3_changes = dlsym(RTLD_NEXT, "sqlite3_changes");
    orig_sqlite3_changes64 = dlsym(RTLD_NEXT, "sqlite3_changes64");
    orig_sqlite3_last_insert_rowid = dlsym(RTLD_NEXT, "sqlite3_last_insert_rowid");
    orig_sqlite3_get_table = dlsym(RTLD_NEXT, "sqlite3_get_table");

    orig_sqlite3_errmsg = dlsym(RTLD_NEXT, "sqlite3_errmsg");
    orig_sqlite3_errcode = dlsym(RTLD_NEXT, "sqlite3_errcode");
    orig_sqlite3_extended_errcode = dlsym(RTLD_NEXT, "sqlite3_extended_errcode");

    orig_sqlite3_prepare = dlsym(RTLD_NEXT, "sqlite3_prepare");
    orig_sqlite3_prepare_v2 = dlsym(RTLD_NEXT, "sqlite3_prepare_v2");
    orig_sqlite3_prepare_v3 = dlsym(RTLD_NEXT, "sqlite3_prepare_v3");
    orig_sqlite3_prepare16_v2 = dlsym(RTLD_NEXT, "sqlite3_prepare16_v2");

    orig_sqlite3_bind_int = dlsym(RTLD_NEXT, "sqlite3_bind_int");
    orig_sqlite3_bind_int64 = dlsym(RTLD_NEXT, "sqlite3_bind_int64");
    orig_sqlite3_bind_double = dlsym(RTLD_NEXT, "sqlite3_bind_double");
    orig_sqlite3_bind_text = dlsym(RTLD_NEXT, "sqlite3_bind_text");
    orig_sqlite3_bind_text64 = dlsym(RTLD_NEXT, "sqlite3_bind_text64");
    orig_sqlite3_bind_blob = dlsym(RTLD_NEXT, "sqlite3_bind_blob");
    orig_sqlite3_bind_blob64 = dlsym(RTLD_NEXT, "sqlite3_bind_blob64");
    orig_sqlite3_bind_value = dlsym(RTLD_NEXT, "sqlite3_bind_value");
    orig_sqlite3_bind_null = dlsym(RTLD_NEXT, "sqlite3_bind_null");

    orig_sqlite3_step = dlsym(RTLD_NEXT, "sqlite3_step");
    orig_sqlite3_reset = dlsym(RTLD_NEXT, "sqlite3_reset");
    orig_sqlite3_finalize = dlsym(RTLD_NEXT, "sqlite3_finalize");
    orig_sqlite3_clear_bindings = dlsym(RTLD_NEXT, "sqlite3_clear_bindings");

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
    orig_sqlite3_data_count = dlsym(RTLD_NEXT, "sqlite3_data_count");
    orig_sqlite3_db_handle = dlsym(RTLD_NEXT, "sqlite3_db_handle");
    orig_sqlite3_expanded_sql = dlsym(RTLD_NEXT, "sqlite3_expanded_sql");
    orig_sqlite3_sql = dlsym(RTLD_NEXT, "sqlite3_sql");
    orig_sqlite3_free = dlsym(RTLD_NEXT, "sqlite3_free");
    orig_sqlite3_bind_parameter_name = dlsym(RTLD_NEXT, "sqlite3_bind_parameter_name");

    orig_sqlite3_value_type = dlsym(RTLD_NEXT, "sqlite3_value_type");
    orig_sqlite3_value_text = dlsym(RTLD_NEXT, "sqlite3_value_text");
    orig_sqlite3_value_int = dlsym(RTLD_NEXT, "sqlite3_value_int");
    orig_sqlite3_value_int64 = dlsym(RTLD_NEXT, "sqlite3_value_int64");
    orig_sqlite3_value_double = dlsym(RTLD_NEXT, "sqlite3_value_double");
    orig_sqlite3_value_bytes = dlsym(RTLD_NEXT, "sqlite3_value_bytes");
    orig_sqlite3_value_blob = dlsym(RTLD_NEXT, "sqlite3_value_blob");

    orig_sqlite3_create_collation = dlsym(RTLD_NEXT, "sqlite3_create_collation");
    orig_sqlite3_create_collation_v2 = dlsym(RTLD_NEXT, "sqlite3_create_collation_v2");

    // Additional SQLite API functions
    orig_sqlite3_malloc = dlsym(RTLD_NEXT, "sqlite3_malloc");
    orig_sqlite3_bind_parameter_count = dlsym(RTLD_NEXT, "sqlite3_bind_parameter_count");
    orig_sqlite3_stmt_readonly = dlsym(RTLD_NEXT, "sqlite3_stmt_readonly");
    orig_sqlite3_stmt_busy = dlsym(RTLD_NEXT, "sqlite3_stmt_busy");
    orig_sqlite3_stmt_status = dlsym(RTLD_NEXT, "sqlite3_stmt_status");

    // Set up aliases for backward compatibility
    real_sqlite3_prepare_v2 = orig_sqlite3_prepare_v2;
    real_sqlite3_errmsg = orig_sqlite3_errmsg;
    real_sqlite3_errcode = orig_sqlite3_errcode;

    // Verify critical functions were found
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

    fprintf(stderr, "[SHIM_INIT] Original SQLite functions loaded\n");
}

// ============================================================================
// Fork Handlers - Critical for Connection Pool Safety
// ============================================================================

// Called in PARENT before fork()
static void atfork_prepare(void) {
    // No action needed - parent continues with its connections
}

// Called in PARENT after fork()
static void atfork_parent(void) {
    // No action needed - parent keeps its connections
}

// Called in CHILD after fork()
static void atfork_child(void) {
    // CRITICAL: Child process must NOT use parent's PostgreSQL connections
    // The PostgreSQL protocol is not fork-safe - sockets are in the middle of I/O
    // This prevents the crash where parent's query hangs while child creates connections

    // Use fprintf since logging may not be initialized yet
    fprintf(stderr, "[FORK_CHILD] Cleaning up inherited connection pool\n");
    fflush(stderr);

    // Call pg_client cleanup function to clear pool state
    extern void pg_pool_cleanup_after_fork(void);
    pg_pool_cleanup_after_fork();

    fprintf(stderr, "[FORK_CHILD] Pool cleared, child will create new connections\n");
    fflush(stderr);
}

// ============================================================================
// Constructor/Destructor
// ============================================================================

__attribute__((constructor))
static void shim_init(void) {
    // Write to stderr first to verify constructor runs
    fprintf(stderr, "[SHIM_INIT] Constructor starting (Linux)...\n");
    fflush(stderr);

    // Install signal handlers for crash debugging (only actual crashes, not SIGABRT which is often legitimate)
    signal(SIGSEGV, signal_handler);
    signal(SIGBUS, signal_handler);
    signal(SIGFPE, signal_handler);
    signal(SIGILL, signal_handler);
    // Note: No SIGABRT handler (used by legitimate assert/abort) and no atexit (triggers on subprocesses)

    // CRITICAL: Install fork handlers BEFORE any PostgreSQL connections are made
    // This ensures child processes don't inherit parent's active connections
    pthread_atfork(atfork_prepare, atfork_parent, atfork_child);
    fprintf(stderr, "[SHIM_INIT] Registered pthread_atfork handlers for connection pool safety\n");
    fflush(stderr);

    // Load original SQLite functions via RTLD_NEXT
    load_original_functions();

    // Skip full initialization if SQLite isn't loaded (e.g., in /bin/sh, nc, etc.)
    // This prevents crashes in processes that don't use SQLite
    if (!orig_sqlite3_open || !orig_sqlite3_prepare_v2) {
        fprintf(stderr, "[SHIM_INIT] SQLite not found in this process, skipping initialization\n");
        fflush(stderr);
        return;
    }

    pg_logging_init();
    LOG_INFO("=== Plex PostgreSQL Interpose Shim loaded (Linux) ===");
    LOG_ERROR("SHIM_CONSTRUCTOR: Initialization complete");

    // Also write to stderr after logging init
    fprintf(stderr, "[SHIM_INIT] Logging initialized\n");
    fflush(stderr);

    pg_config_init();
    pg_client_init();
    pg_statement_init();
    pg_query_cache_init();  // Initialize query result cache
    sql_translator_init();

    // Start worker thread with 8MB stack for heavy queries
    worker_init();

    shim_initialized = 1;

    fprintf(stderr, "[SHIM_INIT] All modules initialized (Linux)\n");
    fflush(stderr);
}

__attribute__((destructor))
static void shim_cleanup(void) {
    // Only cleanup if we actually initialized
    if (!shim_initialized) return;

    LOG_INFO("=== Plex PostgreSQL Interpose Shim unloading ===");
    worker_cleanup();  // Stop worker thread first
    pg_statement_cleanup();
    pg_client_cleanup();
    sql_translator_cleanup();
    pg_logging_cleanup();
}

// ============================================================================
// LD_PRELOAD Wrapper Functions
// These have the exact sqlite3_* names so they intercept via LD_PRELOAD.
// They call the my_sqlite3_* implementations from the shared modules.
// ============================================================================

// Open/Close
int sqlite3_open(const char *filename, sqlite3 **ppDb) {
    return my_sqlite3_open(filename, ppDb);
}

int sqlite3_open_v2(const char *filename, sqlite3 **ppDb, int flags, const char *zVfs) {
    return my_sqlite3_open_v2(filename, ppDb, flags, zVfs);
}

int sqlite3_close(sqlite3 *db) {
    return my_sqlite3_close(db);
}

int sqlite3_close_v2(sqlite3 *db) {
    return my_sqlite3_close_v2(db);
}

// Exec
int sqlite3_exec(sqlite3 *db, const char *sql,
                 int (*callback)(void*,int,char**,char**),
                 void *arg, char **errmsg) {
    return my_sqlite3_exec(db, sql, callback, arg, errmsg);
}

int sqlite3_get_table(sqlite3 *db, const char *sql, char ***pazResult,
                      int *pnRow, int *pnColumn, char **pzErrMsg) {
    return my_sqlite3_get_table(db, sql, pazResult, pnRow, pnColumn, pzErrMsg);
}

// Metadata
int sqlite3_changes(sqlite3 *db) {
    return my_sqlite3_changes(db);
}

sqlite3_int64 sqlite3_changes64(sqlite3 *db) {
    return my_sqlite3_changes64(db);
}

sqlite3_int64 sqlite3_last_insert_rowid(sqlite3 *db) {
    return my_sqlite3_last_insert_rowid(db);
}

const char* sqlite3_errmsg(sqlite3 *db) {
    return my_sqlite3_errmsg(db);
}

int sqlite3_errcode(sqlite3 *db) {
    return my_sqlite3_errcode(db);
}

int sqlite3_extended_errcode(sqlite3 *db) {
    return my_sqlite3_extended_errcode(db);
}

// Prepare
int sqlite3_prepare(sqlite3 *db, const char *zSql, int nByte,
                    sqlite3_stmt **ppStmt, const char **pzTail) {
    return my_sqlite3_prepare(db, zSql, nByte, ppStmt, pzTail);
}

int sqlite3_prepare_v2(sqlite3 *db, const char *zSql, int nByte,
                       sqlite3_stmt **ppStmt, const char **pzTail) {
    return my_sqlite3_prepare_v2(db, zSql, nByte, ppStmt, pzTail);
}

int sqlite3_prepare_v3(sqlite3 *db, const char *zSql, int nByte, unsigned int prepFlags,
                       sqlite3_stmt **ppStmt, const char **pzTail) {
    return my_sqlite3_prepare_v3(db, zSql, nByte, prepFlags, ppStmt, pzTail);
}

int sqlite3_prepare16_v2(sqlite3 *db, const void *zSql, int nByte,
                         sqlite3_stmt **ppStmt, const void **pzTail) {
    return my_sqlite3_prepare16_v2(db, zSql, nByte, ppStmt, pzTail);
}

// Bind
int sqlite3_bind_int(sqlite3_stmt *pStmt, int idx, int val) {
    return my_sqlite3_bind_int(pStmt, idx, val);
}

int sqlite3_bind_int64(sqlite3_stmt *pStmt, int idx, sqlite3_int64 val) {
    return my_sqlite3_bind_int64(pStmt, idx, val);
}

int sqlite3_bind_double(sqlite3_stmt *pStmt, int idx, double val) {
    return my_sqlite3_bind_double(pStmt, idx, val);
}

int sqlite3_bind_text(sqlite3_stmt *pStmt, int idx, const char *val,
                      int nByte, void (*xDel)(void*)) {
    return my_sqlite3_bind_text(pStmt, idx, val, nByte, xDel);
}

int sqlite3_bind_text64(sqlite3_stmt *pStmt, int idx, const char *val,
                        sqlite3_uint64 nByte, void (*xDel)(void*), unsigned char enc) {
    return my_sqlite3_bind_text64(pStmt, idx, val, nByte, xDel, enc);
}

int sqlite3_bind_blob(sqlite3_stmt *pStmt, int idx, const void *val,
                      int nByte, void (*xDel)(void*)) {
    return my_sqlite3_bind_blob(pStmt, idx, val, nByte, xDel);
}

int sqlite3_bind_blob64(sqlite3_stmt *pStmt, int idx, const void *val,
                        sqlite3_uint64 nByte, void (*xDel)(void*)) {
    return my_sqlite3_bind_blob64(pStmt, idx, val, nByte, xDel);
}

int sqlite3_bind_value(sqlite3_stmt *pStmt, int idx, const sqlite3_value *pValue) {
    return my_sqlite3_bind_value(pStmt, idx, pValue);
}

int sqlite3_bind_null(sqlite3_stmt *pStmt, int idx) {
    return my_sqlite3_bind_null(pStmt, idx);
}

// Step/Reset/Finalize
int sqlite3_step(sqlite3_stmt *pStmt) {
    return my_sqlite3_step(pStmt);
}

int sqlite3_reset(sqlite3_stmt *pStmt) {
    return my_sqlite3_reset(pStmt);
}

int sqlite3_finalize(sqlite3_stmt *pStmt) {
    return my_sqlite3_finalize(pStmt);
}

int sqlite3_clear_bindings(sqlite3_stmt *pStmt) {
    return my_sqlite3_clear_bindings(pStmt);
}

// Column access
int sqlite3_column_count(sqlite3_stmt *pStmt) {
    return my_sqlite3_column_count(pStmt);
}

int sqlite3_column_type(sqlite3_stmt *pStmt, int idx) {
    return my_sqlite3_column_type(pStmt, idx);
}

int sqlite3_column_int(sqlite3_stmt *pStmt, int idx) {
    return my_sqlite3_column_int(pStmt, idx);
}

sqlite3_int64 sqlite3_column_int64(sqlite3_stmt *pStmt, int idx) {
    return my_sqlite3_column_int64(pStmt, idx);
}

double sqlite3_column_double(sqlite3_stmt *pStmt, int idx) {
    return my_sqlite3_column_double(pStmt, idx);
}

const unsigned char* sqlite3_column_text(sqlite3_stmt *pStmt, int idx) {
    return my_sqlite3_column_text(pStmt, idx);
}

const void* sqlite3_column_blob(sqlite3_stmt *pStmt, int idx) {
    return my_sqlite3_column_blob(pStmt, idx);
}

int sqlite3_column_bytes(sqlite3_stmt *pStmt, int idx) {
    return my_sqlite3_column_bytes(pStmt, idx);
}

const char* sqlite3_column_name(sqlite3_stmt *pStmt, int idx) {
    return my_sqlite3_column_name(pStmt, idx);
}

sqlite3_value* sqlite3_column_value(sqlite3_stmt *pStmt, int idx) {
    return my_sqlite3_column_value(pStmt, idx);
}

int sqlite3_data_count(sqlite3_stmt *pStmt) {
    return my_sqlite3_data_count(pStmt);
}

// sqlite3_db_handle - use my_ implementation for PG statement support
sqlite3* sqlite3_db_handle(sqlite3_stmt *pStmt) {
    return my_sqlite3_db_handle(pStmt);
}

// sqlite3_expanded_sql - use my_ implementation for PG statement support
char* sqlite3_expanded_sql(sqlite3_stmt *pStmt) {
    return my_sqlite3_expanded_sql(pStmt);
}

// sqlite3_sql - use my_ implementation for PG statement support
const char* sqlite3_sql(sqlite3_stmt *pStmt) {
    return my_sqlite3_sql(pStmt);
}

// sqlite3_free - use my_ implementation
void sqlite3_free(void *p) {
    my_sqlite3_free(p);
}

// sqlite3_malloc - use my_ implementation
void* sqlite3_malloc(int n) {
    return my_sqlite3_malloc(n);
}

// sqlite3_bind_parameter_count - use my_ implementation for PG statement support
int sqlite3_bind_parameter_count(sqlite3_stmt *pStmt) {
    return my_sqlite3_bind_parameter_count(pStmt);
}

// sqlite3_stmt_readonly - use my_ implementation for PG statement support
int sqlite3_stmt_readonly(sqlite3_stmt *pStmt) {
    return my_sqlite3_stmt_readonly(pStmt);
}

// sqlite3_stmt_busy - use my_ implementation for PG statement support
int sqlite3_stmt_busy(sqlite3_stmt *pStmt) {
    return my_sqlite3_stmt_busy(pStmt);
}

// sqlite3_stmt_status - use my_ implementation for PG statement support
int sqlite3_stmt_status(sqlite3_stmt *pStmt, int op, int resetFlg) {
    return my_sqlite3_stmt_status(pStmt, op, resetFlg);
}

// sqlite3_bind_parameter_name - use my_ implementation for PG statement support
const char* sqlite3_bind_parameter_name(sqlite3_stmt *pStmt, int idx) {
    return my_sqlite3_bind_parameter_name(pStmt, idx);
}

// Value access
int sqlite3_value_type(sqlite3_value *pVal) {
    return my_sqlite3_value_type(pVal);
}

const unsigned char* sqlite3_value_text(sqlite3_value *pVal) {
    return my_sqlite3_value_text(pVal);
}

int sqlite3_value_int(sqlite3_value *pVal) {
    return my_sqlite3_value_int(pVal);
}

sqlite3_int64 sqlite3_value_int64(sqlite3_value *pVal) {
    return my_sqlite3_value_int64(pVal);
}

double sqlite3_value_double(sqlite3_value *pVal) {
    return my_sqlite3_value_double(pVal);
}

int sqlite3_value_bytes(sqlite3_value *pVal) {
    return my_sqlite3_value_bytes(pVal);
}

const void* sqlite3_value_blob(sqlite3_value *pVal) {
    return my_sqlite3_value_blob(pVal);
}

// Collation
int sqlite3_create_collation(sqlite3 *db, const char *zName, int eTextRep,
                              void *pArg,
                              int(*xCompare)(void*,int,const void*,int,const void*)) {
    return my_sqlite3_create_collation(db, zName, eTextRep, pArg, xCompare);
}

int sqlite3_create_collation_v2(sqlite3 *db, const char *zName, int eTextRep,
                                 void *pArg,
                                 int(*xCompare)(void*,int,const void*,int,const void*),
                                 void(*xDestroy)(void*)) {
    return my_sqlite3_create_collation_v2(db, zName, eTextRep, pArg, xCompare, xDestroy);
}
