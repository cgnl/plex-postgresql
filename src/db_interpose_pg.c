/*
 * Plex PostgreSQL Interposing Shim
 *
 * Uses macOS DYLD_INTERPOSE to intercept SQLite calls and redirect to PostgreSQL.
 * This approach allows the original SQLite to handle all functions we don't override.
 */

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

// Use modular components
#include "pg_types.h"
#include "pg_logging.h"
#include "pg_config.h"
#include "pg_client.h"
#include "pg_statement.h"
#include "sql_translator.h"

// ============================================================================
// Recursion prevention for interposed functions
// ============================================================================

// With DYLD_INTERPOSE, ALL calls to sqlite3_prepare_v2 go through our shim,
// including calls from within our own code. We use RTLD_NEXT to get a pointer
// to the original SQLite function, bypassing our interpose.

static __thread int in_interpose_call = 0;

// Track recursion depth for prepare_v2 to detect infinite loops
// SQLite can call prepare_v2 internally, which calls our shim again
static __thread int prepare_v2_depth = 0;

// Pointers to the REAL SQLite functions (resolved from system SQLite)
// We load the SYSTEM libsqlite3.dylib directly to bypass our interpose entirely.
// CRITICAL: We need pointers for ALL functions we might call from within our shim
// to prevent infinite recursion via DYLD_INTERPOSE.
static void *sqlite_handle = NULL;
static int (*real_sqlite3_prepare_v2)(sqlite3*, const char*, int, sqlite3_stmt**, const char**) = NULL;
static const char* (*real_sqlite3_errmsg)(sqlite3*) = NULL;
static int (*real_sqlite3_errcode)(sqlite3*) = NULL;

// ============================================================================
// Globals (minimal - most state is in modules)
// ============================================================================

static int shim_initialized = 0;

// ============================================================================
// Worker Thread with Large Stack (8MB)
// ============================================================================
// When Plex's thread has insufficient stack for SQL parsing, we delegate
// the work to our own thread which has plenty of stack space.

#define WORKER_STACK_SIZE (8 * 1024 * 1024)  // 8MB stack for worker

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

static pthread_t worker_thread;
static pthread_mutex_t worker_mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t worker_cond_request = PTHREAD_COND_INITIALIZER;
static pthread_cond_t worker_cond_response = PTHREAD_COND_INITIALIZER;
static worker_request_t worker_request;
static volatile int worker_running = 0;

// Forward declarations for functions used by worker
static int my_sqlite3_prepare_v2_internal(sqlite3 *db, const char *zSql, int nByte,
                                          sqlite3_stmt **ppStmt, const char **pzTail,
                                          int from_worker);

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

static int worker_init(void) {
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

static void worker_cleanup(void) {
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
static int delegate_prepare_to_worker(sqlite3 *db, const char *zSql, int nByte,
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
// Fake sqlite3_value for PostgreSQL results
// ============================================================================

// When Plex calls sqlite3_column_value(), we need to return something that
// sqlite3_value_type(), sqlite3_value_text(), etc. can work with.
// For PostgreSQL results, we create "fake" values that encode the pg_stmt and column.

typedef struct {
    uint32_t magic;      // Magic number to identify our fake values
    void *pg_stmt;       // Pointer to pg_stmt_t
    int col_idx;         // Column index
    int row_idx;         // Row index at time of column_value call
} pg_fake_value_t;

#define PG_FAKE_VALUE_MAGIC 0x50475641  // "PGVA"

// Pool of fake values (one per column, reused)
#define MAX_FAKE_VALUES 256
static pg_fake_value_t fake_value_pool[MAX_FAKE_VALUES];
static unsigned int fake_value_next = 0;  // MUST be unsigned to prevent negative index after overflow!
static pthread_mutex_t fake_value_mutex = PTHREAD_MUTEX_INITIALIZER;

// Check if a pointer is one of our fake values
static pg_fake_value_t* pg_check_fake_value(sqlite3_value *pVal) {
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
static int is_library_db_path(const char *path) {
    return path && strstr(path, "com.plexapp.plugins.library.db") != NULL;
}

// ============================================================================
// Simple string replace helper
// ============================================================================

static char* simple_str_replace(const char *str, const char *old, const char *new_str) {
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
// Interposed SQLite Functions - Open/Close
// ============================================================================

// Helper to drop indexes that use icu_root collation
// These indexes cause "no such collation sequence: icu_root" errors
static void drop_icu_root_indexes(sqlite3 *db) {
    if (!db) return;

    const char *drop_index_sqls[] = {
        "DROP INDEX IF EXISTS index_title_sort_icu",
        "DROP INDEX IF EXISTS index_original_title_icu",
        NULL
    };

    int indexes_dropped = 0;
    char *errmsg = NULL;

    for (int i = 0; drop_index_sqls[i] != NULL; i++) {
        int rc = sqlite3_exec(db, drop_index_sqls[i], NULL, NULL, &errmsg);
        if (rc == SQLITE_OK) {
            indexes_dropped++;
        } else if (errmsg) {
            LOG_DEBUG("Failed to drop icu index: %s", errmsg);
            sqlite3_free(errmsg);
            errmsg = NULL;
        }
    }

    if (indexes_dropped > 0) {
        LOG_INFO("Dropped %d icu_root indexes to avoid collation errors", indexes_dropped);
    }
}

// Helper to drop FTS triggers that use "collating" tokenizer
// These triggers cause "unknown tokenizer: collating" errors when
// DELETE/UPDATE on tags or metadata_items fires them
static void drop_fts_triggers(sqlite3 *db) {
    if (!db) return;

    const char *drop_trigger_sqls[] = {
        "DROP TRIGGER IF EXISTS fts4_tag_titles_before_update_icu",
        "DROP TRIGGER IF EXISTS fts4_tag_titles_before_delete_icu",
        "DROP TRIGGER IF EXISTS fts4_tag_titles_after_update_icu",
        "DROP TRIGGER IF EXISTS fts4_tag_titles_after_insert_icu",
        "DROP TRIGGER IF EXISTS fts4_metadata_titles_before_update_icu",
        "DROP TRIGGER IF EXISTS fts4_metadata_titles_before_delete_icu",
        "DROP TRIGGER IF EXISTS fts4_metadata_titles_after_update_icu",
        "DROP TRIGGER IF EXISTS fts4_metadata_titles_after_insert_icu",
        NULL
    };

    int triggers_dropped = 0;
    char *errmsg = NULL;

    for (int i = 0; drop_trigger_sqls[i] != NULL; i++) {
        int rc = sqlite3_exec(db, drop_trigger_sqls[i], NULL, NULL, &errmsg);
        if (rc == SQLITE_OK) {
            triggers_dropped++;
        } else if (errmsg) {
            LOG_DEBUG("Failed to drop trigger: %s", errmsg);
            sqlite3_free(errmsg);
            errmsg = NULL;
        }
    }

    if (triggers_dropped > 0) {
        LOG_INFO("Dropped %d FTS triggers to avoid 'unknown tokenizer' errors", triggers_dropped);
    }
}

static int my_sqlite3_open(const char *filename, sqlite3 **ppDb) {
    LOG_INFO("OPEN: %s (redirect=%d)", filename ? filename : "(null)", should_redirect(filename));

    int rc = sqlite3_open(filename, ppDb);

    if (rc == SQLITE_OK && should_redirect(filename)) {
        // Drop FTS triggers to prevent "unknown tokenizer: collating" errors
        drop_fts_triggers(*ppDb);
        // Drop icu_root indexes to prevent "no such collation sequence" errors
        drop_icu_root_indexes(*ppDb);

        pg_connection_t *pg_conn = pg_connect(filename, *ppDb);
        if (pg_conn) {
            pg_register_connection(pg_conn);
            LOG_INFO("PostgreSQL shadow connection established for: %s", filename);
        }
    }

    return rc;
}

static int my_sqlite3_open_v2(const char *filename, sqlite3 **ppDb, int flags, const char *zVfs) {
    LOG_INFO("OPEN_V2: %s flags=0x%x (redirect=%d)",
             filename ? filename : "(null)", flags, should_redirect(filename));

    int rc = sqlite3_open_v2(filename, ppDb, flags, zVfs);

    if (rc == SQLITE_OK && should_redirect(filename)) {
        // Drop FTS triggers to prevent "unknown tokenizer: collating" errors
        drop_fts_triggers(*ppDb);
        // Drop icu_root indexes to prevent "no such collation sequence" errors
        drop_icu_root_indexes(*ppDb);

        pg_connection_t *pg_conn = pg_connect(filename, *ppDb);
        if (pg_conn) {
            pg_register_connection(pg_conn);
            LOG_INFO("PostgreSQL shadow connection established for: %s", filename);
        }
    }

    return rc;
}

static int my_sqlite3_close(sqlite3 *db) {
    pg_connection_t *pg_conn = pg_find_connection(db);
    if (pg_conn) {
        LOG_INFO("CLOSE: PostgreSQL connection for %s", pg_conn->db_path);

        // If this is a library.db, release pool connection
        if (strstr(pg_conn->db_path, "com.plexapp.plugins.library.db")) {
            pg_close_pool_for_db(db);
        }

        pg_unregister_connection(pg_conn);
        pg_close(pg_conn);
    }
    return sqlite3_close(db);
}

static int my_sqlite3_close_v2(sqlite3 *db) {
    pg_connection_t *pg_conn = pg_find_connection(db);
    if (pg_conn) {
        // If this is a library.db, release pool connection
        if (strstr(pg_conn->db_path, "com.plexapp.plugins.library.db")) {
            pg_close_pool_for_db(db);
        }

        pg_unregister_connection(pg_conn);
        pg_close(pg_conn);
    }
    return sqlite3_close_v2(db);
}

// ============================================================================
// Interposed SQLite Functions - Exec
// ============================================================================

static int my_sqlite3_exec(sqlite3 *db, const char *sql,
                          int (*callback)(void*, int, char**, char**),
                          void *arg, char **errmsg) {
    // CRITICAL FIX: NULL check to prevent crash in strcasestr
    if (!sql) {
        LOG_ERROR("exec called with NULL SQL");
        return sqlite3_exec(db, sql, callback, arg, errmsg);
    }

    pg_connection_t *pg_conn = pg_find_connection(db);

    if (pg_conn && pg_conn->conn && pg_conn->is_pg_active) {
        if (!should_skip_sql(sql)) {
            sql_translation_t trans = sql_translate(sql);
            if (trans.success && trans.sql) {
                char *exec_sql = trans.sql;
                char *insert_sql = NULL;

                // Add RETURNING id for INSERT statements
                if (strncasecmp(sql, "INSERT", 6) == 0 && !strstr(trans.sql, "RETURNING")) {
                    size_t len = strlen(trans.sql);
                    insert_sql = malloc(len + 20);
                    if (insert_sql) {
                        snprintf(insert_sql, len + 20, "%s RETURNING id", trans.sql);
                        exec_sql = insert_sql;
                        if (strstr(sql, "play_queue_generators")) {
                            LOG_INFO("EXEC play_queue_generators INSERT with RETURNING: %s", exec_sql);
                        }
                    }
                }

                // CRITICAL FIX: Lock connection mutex to prevent concurrent libpq access
                pthread_mutex_lock(&pg_conn->mutex);
                PGresult *res = PQexec(pg_conn->conn, exec_sql);
                ExecStatusType status = PQresultStatus(res);

                if (status == PGRES_COMMAND_OK || status == PGRES_TUPLES_OK) {
                    pg_conn->last_changes = atoi(PQcmdTuples(res) ?: "1");

                    // Extract ID from RETURNING clause for INSERT
                    if (strncasecmp(sql, "INSERT", 6) == 0 && status == PGRES_TUPLES_OK && PQntuples(res) > 0) {
                        const char *id_str = PQgetvalue(res, 0, 0);
                        if (id_str && *id_str) {
                            if (strstr(sql, "play_queue_generators")) {
                                LOG_INFO("EXEC play_queue_generators: RETURNING id = %s", id_str);
                            }
                            sqlite3_int64 meta_id = extract_metadata_id_from_generator_sql(sql);
                            if (meta_id > 0) pg_set_global_metadata_id(meta_id);
                        }
                    }
                } else {
                    const char *err = (pg_conn && pg_conn->conn) ? PQerrorMessage(pg_conn->conn) : "NULL connection";
                    LOG_ERROR("PostgreSQL exec error: %s", err);
                    // CRITICAL: Check if connection is corrupted and needs reset
                    // Note: pg_conn may be a pool connection for library.db
                    pg_pool_check_connection_health(pg_conn);
                }

                if (insert_sql) free(insert_sql);
                PQclear(res);
                pthread_mutex_unlock(&pg_conn->mutex);
            }
            sql_translation_free(&trans);
        }
        return SQLITE_OK;
    }

    // For non-PG databases, strip collate icu_root since SQLite doesn't support it
    char *cleaned_sql = NULL;
    const char *exec_sql = sql;
    if (strcasestr(sql, "collate icu_root")) {
        cleaned_sql = strdup(sql);
        if (cleaned_sql) {
            char *pos;
            while ((pos = strcasestr(cleaned_sql, " collate icu_root")) != NULL) {
                memmove(pos, pos + 17, strlen(pos + 17) + 1);
            }
            while ((pos = strcasestr(cleaned_sql, "collate icu_root")) != NULL) {
                memmove(pos, pos + 16, strlen(pos + 16) + 1);
            }
            exec_sql = cleaned_sql;
        }
    }

    int rc = sqlite3_exec(db, exec_sql, callback, arg, errmsg);
    if (cleaned_sql) free(cleaned_sql);
    return rc;
}

// ============================================================================
// Interposed SQLite Functions - Prepare
// ============================================================================

// Helper to create a simplified SQL for SQLite when query uses FTS
// Removes FTS joins and MATCH clauses since SQLite shadow DB doesn't have FTS tables
static char* simplify_fts_for_sqlite(const char *sql) {
    if (!sql || !strcasestr(sql, "fts4_")) return NULL;

    char *result = malloc(strlen(sql) * 2 + 100);
    if (!result) return NULL;
    strcpy(result, sql);

    // Remove JOINs with fts4_* tables
    const char *fts_patterns[] = {
        "join fts4_metadata_titles_icu",
        "join fts4_metadata_titles",
        "join fts4_tag_titles_icu",
        "join fts4_tag_titles"
    };

    for (int p = 0; p < 4; p++) {
        char *join_start;
        while ((join_start = strcasestr(result, fts_patterns[p])) != NULL) {
            char *join_end = join_start;
            while (*join_end) {
                if (strncasecmp(join_end, " where ", 7) == 0 ||
                    strncasecmp(join_end, " join ", 6) == 0 ||
                    strncasecmp(join_end, " left ", 6) == 0 ||
                    strncasecmp(join_end, " group ", 7) == 0 ||
                    strncasecmp(join_end, " order ", 7) == 0) {
                    break;
                }
                join_end++;
            }
            memmove(join_start, join_end, strlen(join_end) + 1);
        }
    }

    // Remove MATCH clauses: "fts4_*.title match 'term'" -> "1=1"
    // Also handle title_sort match
    const char *match_patterns[] = {
        "fts4_metadata_titles_icu.title match ",
        "fts4_metadata_titles_icu.title_sort match ",
        "fts4_metadata_titles.title match ",
        "fts4_metadata_titles.title_sort match ",
        "fts4_tag_titles_icu.title match ",
        "fts4_tag_titles_icu.tag match ",
        "fts4_tag_titles.title match ",
        "fts4_tag_titles.tag match "
    };
    int num_patterns = 8;

    for (int p = 0; p < num_patterns; p++) {
        char *match_pos;
        while ((match_pos = strcasestr(result, match_patterns[p])) != NULL) {
            char *quote_start = strchr(match_pos, '\'');
            if (!quote_start) break;
            char *quote_end = strchr(quote_start + 1, '\'');
            if (!quote_end) break;

            // Replace with "1=1"
            const char *replacement = "1=1";
            size_t old_len = (quote_end + 1) - match_pos;
            size_t new_len = strlen(replacement);

            memmove(match_pos + new_len, quote_end + 1, strlen(quote_end + 1) + 1);
            memcpy(match_pos, replacement, new_len);
        }
    }

    return result;
}

// Internal prepare_v2 implementation - called either directly or from worker thread
// from_worker: 0 = called from Plex's thread, 1 = called from worker with large stack
static int my_sqlite3_prepare_v2_internal(sqlite3 *db, const char *zSql, int nByte,
                                          sqlite3_stmt **ppStmt, const char **pzTail,
                                          int from_worker) {
    // CRITICAL: Track recursion depth to prevent infinite loops
    // SQLite can internally call prepare_v2 again, creating deep recursion
    prepare_v2_depth++;

    // If recursion is too deep, bail out immediately to prevent stack overflow
    // Normal operations should never recurse more than 5-10 times
    // The crash on 2026-01-06 showed 218 recursive frames!
    if (prepare_v2_depth > 100) {
        LOG_ERROR("RECURSION LIMIT: prepare_v2 called %d times (depth=%d)!",
                  prepare_v2_depth, prepare_v2_depth);
        LOG_ERROR("  This indicates infinite recursion - ABORTING to prevent crash");
        LOG_ERROR("  Query: %.200s", zSql ? zSql : "NULL");
        prepare_v2_depth--;
        if (ppStmt) *ppStmt = NULL;
        if (pzTail) *pzTail = NULL;
        return SQLITE_ERROR;
    }

    // CRITICAL FIX: Stack overflow protection
    // Get thread stack bounds to detect how much stack we have left
    pthread_t self = pthread_self();
    void *stack_addr = pthread_get_stackaddr_np(self);
    size_t stack_size = pthread_get_stacksize_np(self);

    // Calculate stack base and current position
    char *stack_base = (char*)stack_addr;
    char local_var;
    char *current_stack = &local_var;

    // Calculate how much stack we've used
    // Stack grows downward on macOS/ARM64
    ptrdiff_t stack_used = stack_base - current_stack;
    if (stack_used < 0) stack_used = -stack_used;

    // Calculate how much stack is left
    ptrdiff_t stack_remaining = (ptrdiff_t)stack_size - stack_used;

    // WORKER THREAD DELEGATION:
    // If stack is low and we're NOT already on the worker thread, delegate to worker
    // The worker has 8MB stack so it can handle complex queries safely
    // CRITICAL: Must delegate EARLY - Plex uses ~500KB of 544KB stack before reaching us!
    // With only ~44KB remaining, even our check logic can overflow the stack.
    // Delegate when less than 400KB remains to be safe.
    #define WORKER_DELEGATION_THRESHOLD 400000  // 400KB - delegate early!

    // DISABLED: Worker delegation has deadlock issues. The recursion prevention
    // fix (using real_sqlite3_* via dlsym) should be enough now.
    // TODO: Fix worker thread synchronization if still needed
    #if 0
    if (!from_worker && stack_remaining < WORKER_DELEGATION_THRESHOLD && worker_running) {
        LOG_INFO("WORKER DELEGATION: stack_remaining=%ld bytes < %d, delegating to worker",
                 (long)stack_remaining, WORKER_DELEGATION_THRESHOLD);
        prepare_v2_depth--;  // Worker will increment again
        return delegate_prepare_to_worker(db, zSql, nByte, ppStmt, pzTail);
    }
    #endif

    // If we're on worker thread, we have 8MB stack so only use minimal threshold
    int stack_threshold = from_worker ? 32000 : 32000;  // 32KB minimum

    if (stack_remaining < stack_threshold) {
        LOG_ERROR("STACK PROTECTION TRIGGERED: stack_used=%ld/%ld bytes, remaining=%ld bytes (from_worker=%d)",
                 (long)stack_used, (long)stack_size, (long)stack_remaining, from_worker);
        LOG_ERROR("  SQLite needs ~400KB stack. Query rejected to prevent crash.");
        LOG_ERROR("  Query: %.200s", zSql ? zSql : "NULL");

        // CRITICAL FIX: Set error state on db handle for SOCI compatibility
        // When we return an error code without calling real sqlite3_prepare_v2,
        // the db's internal error state is not set, causing SOCI to call
        // sqlite3_errmsg() and get "not an error" instead of our error message.
        // We need to track this error state ourselves.
        pg_connection_t *pg_conn = pg_find_connection(db);
        if (pg_conn) {
            pg_conn->last_error_code = SQLITE_NOMEM;
            snprintf(pg_conn->last_error, sizeof(pg_conn->last_error),
                     "Stack protection: insufficient stack space (used=%ld/%ld, remaining=%ld). "
                     "Query too complex for available stack.",
                     (long)stack_used, (long)stack_size, (long)stack_remaining);
        }

        // Return error immediately - do NOT call sqlite3_prepare_v2
        prepare_v2_depth--;  // Decrement before return
        if (ppStmt) *ppStmt = NULL;
        if (pzTail) *pzTail = NULL;
        return SQLITE_NOMEM;  // Out of memory (stack is memory!)
    }

    // Skip complex processing only if stack is really tight (not on worker)
    int skip_complex_processing = 0;
    if (!from_worker && stack_remaining < 64000) {
        skip_complex_processing = 1;
        LOG_INFO("STACK CAUTION: stack_used=%ld/%ld bytes, remaining=%ld - skipping complex processing",
                 (long)stack_used, (long)stack_size, (long)stack_remaining);
    }

    // CRITICAL FIX: NULL check to prevent crash in strcasestr
    if (!zSql) {
        LOG_ERROR("prepare_v2 called with NULL SQL");
        int rc;
        if (real_sqlite3_prepare_v2) {
            rc = real_sqlite3_prepare_v2(db, zSql, nByte, ppStmt, pzTail);
        } else {
            rc = SQLITE_ERROR;
            if (ppStmt) *ppStmt = NULL;
        }
        prepare_v2_depth--;  // Decrement before return
        return rc;
    }

    // DEBUG: Log queries with backticks (the failing OnDeck query pattern)
    if (strchr(zSql, '`')) {
        LOG_INFO("BACKTICK_QUERY: skip_complex=%d len=%d sql=%.200s",
                 skip_complex_processing, (int)strlen(zSql), zSql);
    }

    // Debug: log INSERT INTO metadata_items
    if (!skip_complex_processing && strncasecmp(zSql, "INSERT", 6) == 0 && strcasestr(zSql, "metadata_items")) {
        LOG_INFO("PREPARE_V2 INSERT metadata_items: %.300s", zSql);
        if (strcasestr(zSql, "icu_root")) {
            LOG_INFO("PREPARE_V2 has icu_root - will clean!");
        }
    }

    pg_connection_t *pg_conn = skip_complex_processing ? NULL : pg_find_connection(db);
    int is_write = is_write_operation(zSql);
    int is_read = is_read_operation(zSql);

    // Clean SQL for SQLite (remove icu_root and FTS references)
    char *cleaned_sql = NULL;
    const char *sql_for_sqlite = zSql;

    // ALWAYS simplify FTS queries for SQLite, even without PG connection
    // because SQLite shadow DB doesn't have FTS virtual tables
    // BUT skip if we're in deep recursion to save stack
    if (!skip_complex_processing && strcasestr(zSql, "fts4_")) {
        cleaned_sql = simplify_fts_for_sqlite(zSql);
        if (cleaned_sql) {
            sql_for_sqlite = cleaned_sql;
            LOG_INFO("FTS query simplified for SQLite: %.1000s", zSql);
        }
    }

    // ALWAYS remove "collate icu_root" since SQLite shadow DB doesn't support it
    // BUT skip if we're in deep recursion to save stack
    if (!skip_complex_processing && strcasestr(sql_for_sqlite, "collate icu_root")) {
        char *temp = malloc(strlen(sql_for_sqlite) + 1);
        if (temp) {
            strcpy(temp, sql_for_sqlite);
            char *pos;
            // First try with leading space
            while ((pos = strcasestr(temp, " collate icu_root")) != NULL) {
                memmove(pos, pos + 17, strlen(pos + 17) + 1);
            }
            // Also try without leading space (e.g. after parens)
            while ((pos = strcasestr(temp, "collate icu_root")) != NULL) {
                memmove(pos, pos + 16, strlen(pos + 16) + 1);
            }
            if (cleaned_sql) free(cleaned_sql);
            cleaned_sql = temp;
            sql_for_sqlite = cleaned_sql;
        }
    }

    // CRITICAL: Use real_sqlite3_prepare_v2 to bypass DYLD_INTERPOSE
    // Otherwise we get infinite recursion since sqlite3_prepare_v2 calls us again!
    int rc;
    if (real_sqlite3_prepare_v2) {
        rc = real_sqlite3_prepare_v2(db, sql_for_sqlite, cleaned_sql ? -1 : nByte, ppStmt, pzTail);
    } else {
        // Fallback - this will likely cause recursion but better than crash
        LOG_ERROR("CRITICAL: real_sqlite3_prepare_v2 not initialized!");
        rc = SQLITE_ERROR;
        if (ppStmt) *ppStmt = NULL;
    }

    // CRITICAL FIX: Clear our tracked error state on success
    // This ensures sqlite3_errmsg/errcode return correct values
    pg_connection_t *pg_conn_for_clear = pg_find_connection(db);
    if (pg_conn_for_clear) {
        if (rc == SQLITE_OK) {
            pg_conn_for_clear->last_error_code = SQLITE_OK;
            pg_conn_for_clear->last_error[0] = '\0';
        } else {
            // Track actual SQLite error for consistency
            pg_conn_for_clear->last_error_code = rc;
            const char *sqlite_err = sqlite3_errmsg(db);
            if (sqlite_err) {
                snprintf(pg_conn_for_clear->last_error, sizeof(pg_conn_for_clear->last_error),
                         "%s", sqlite_err);
            }
        }
    }

    if (rc != SQLITE_OK || !*ppStmt) {
        if (cleaned_sql) free(cleaned_sql);
        prepare_v2_depth--;  // Decrement before return
        return rc;
    }

    if (pg_conn && pg_conn->conn && pg_conn->is_pg_active && (is_write || is_read)) {
        pg_stmt_t *pg_stmt = pg_stmt_create(pg_conn, zSql, *ppStmt);
        if (pg_stmt) {
            if (should_skip_sql(zSql)) {
                pg_stmt->is_pg = 3;  // skip
            } else {
                pg_stmt->is_pg = is_write ? 1 : 2;

                sql_translation_t trans = sql_translate(zSql);
                if (!trans.success) {
                       LOG_ERROR("Translation failed for SQL: %s. Error: %s", zSql, trans.error);
                }

                // Use parameter count from SQL translator (already counted during placeholder translation)
                // The translator always returns param_count even if translation failed
                if (trans.param_count > 0) {
                    pg_stmt->param_count = trans.param_count;
                } else {
                    // Fallback: count ? in original SQL if translator didn't provide count
                    const char *p = zSql;
                    while (*p) {
                        if (*p == '?') pg_stmt->param_count++;
                        p++;
                    }
                }

                // Store parameter names for mapping named parameters
                if (trans.param_names && trans.param_count > 0) {
                    pg_stmt->param_names = malloc(trans.param_count * sizeof(char*));
                    if (pg_stmt->param_names) {
                        for (int i = 0; i < trans.param_count; i++) {
                            pg_stmt->param_names[i] = trans.param_names[i] ? strdup(trans.param_names[i]) : NULL;
                        }
                    }
                    // Debug: log parameter names for metadata_items INSERT
                    if (strcasestr(zSql, "INSERT") && strcasestr(zSql, "metadata_items")) {
                        LOG_ERROR("PREPARE INSERT metadata_items: param_count=%d", trans.param_count);
                        LOG_ERROR("  First 15 params in SQL order:");
                        for (int i = 0; i < trans.param_count && i < 15; i++) {
                            LOG_ERROR("    pg_idx[%d] = param_name='%s'", i, trans.param_names[i] ? trans.param_names[i] : "NULL");
                        }
                        if (trans.param_count > 15) {
                            LOG_ERROR("  ... (%d total params)", trans.param_count);
                        }
                        LOG_ERROR("  Original SQL (first 500 chars): %.500s", zSql);
                    }
                }

                if (trans.success && trans.sql) {
                    pg_stmt->pg_sql = strdup(trans.sql);

                    // Add RETURNING id to INSERT statements for proper ID retrieval
                    if (is_write && strncasecmp(zSql, "INSERT", 6) == 0 &&
                        pg_stmt->pg_sql && !strstr(pg_stmt->pg_sql, "RETURNING")) {
                        size_t len = strlen(pg_stmt->pg_sql);
                        char *with_returning = malloc(len + 20);
                        if (with_returning) {
                            snprintf(with_returning, len + 20, "%s RETURNING id", pg_stmt->pg_sql);
                            if (strstr(pg_stmt->pg_sql, "play_queue_generators")) {
                                LOG_INFO("PREPARE play_queue_generators INSERT with RETURNING: %s", with_returning);
                            }
                            free(pg_stmt->pg_sql);
                            pg_stmt->pg_sql = with_returning;
                        }
                    }

                    // Calculate hash and statement name for prepared statement support
                    if (pg_stmt->pg_sql) {
                        pg_stmt->sql_hash = pg_hash_sql(pg_stmt->pg_sql);
                        snprintf(pg_stmt->stmt_name, sizeof(pg_stmt->stmt_name),
                                 "ps_%llx", (unsigned long long)pg_stmt->sql_hash);
                        pg_stmt->use_prepared = 1;  // Use prepared statements for better caching
                    }
                }
                sql_translation_free(&trans);
            }

            pg_register_stmt(*ppStmt, pg_stmt);
        }
    }

    if (cleaned_sql) free(cleaned_sql);
    prepare_v2_depth--;  // Decrement before return
    return rc;
}

// Lazy init for real SQLite functions (in case called before constructor)
static void ensure_real_sqlite_loaded(void) {
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

// Public wrapper - delegates to worker thread if stack is low
static int my_sqlite3_prepare_v2(sqlite3 *db, const char *zSql, int nByte,
                                  sqlite3_stmt **ppStmt, const char **pzTail) {
    // CRITICAL: Ensure real SQLite is loaded (may be called before constructor!)
    ensure_real_sqlite_loaded();

    // CRITICAL FIX: Prevent infinite recursion when our internal code calls sqlite3_prepare_v2
    // With DYLD_INTERPOSE, ALL calls to sqlite3_prepare_v2 come through here, including
    // our own internal calls on lines 711 and 770. Use thread-local flag to detect this.
    if (in_interpose_call) {
        // We're already inside our shim - this is a recursive call from our own code.
        // Call the REAL sqlite3_prepare_v2 directly via our resolved function pointer.
        // This bypasses DYLD_INTERPOSE and prevents infinite recursion.
        if (real_sqlite3_prepare_v2) {
            return real_sqlite3_prepare_v2(db, zSql, nByte, ppStmt, pzTail);
        } else {
            // Fallback if real function pointer wasn't resolved (should never happen)
            LOG_ERROR("CRITICAL: real_sqlite3_prepare_v2 is NULL during recursive call!");
            return SQLITE_ERROR;
        }
    }

    in_interpose_call = 1;
    int result = my_sqlite3_prepare_v2_internal(db, zSql, nByte, ppStmt, pzTail, 0);
    in_interpose_call = 0;
    return result;
}

static int my_sqlite3_prepare(sqlite3 *db, const char *zSql, int nByte,
                              sqlite3_stmt **ppStmt, const char **pzTail) {
    // Route through my_sqlite3_prepare_v2 to get icu_root cleanup and PG handling
    return my_sqlite3_prepare_v2(db, zSql, nByte, ppStmt, pzTail);
}

static int my_sqlite3_prepare16_v2(sqlite3 *db, const void *zSql, int nByte,
                                    sqlite3_stmt **ppStmt, const void **pzTail) {
    // Convert UTF-16 to UTF-8 for icu_root cleanup
    // This is rarely used but we need to handle it for completeness
    if (zSql) {
        // Get UTF-16 length
        int utf16_len = 0;
        if (nByte < 0) {
            const uint16_t *p = (const uint16_t *)zSql;
            while (*p) { p++; utf16_len++; }
            utf16_len *= 2;
        } else {
            utf16_len = nByte;
        }

        // Convert to UTF-8 using a simple approach
        char *utf8_sql = malloc(utf16_len * 2 + 1);
        if (utf8_sql) {
            const uint16_t *src = (const uint16_t *)zSql;
            char *dst = utf8_sql;
            int i;
            for (i = 0; i < utf16_len / 2 && src[i]; i++) {
                if (src[i] < 0x80) {
                    *dst++ = (char)src[i];
                } else if (src[i] < 0x800) {
                    *dst++ = 0xC0 | (src[i] >> 6);
                    *dst++ = 0x80 | (src[i] & 0x3F);
                } else {
                    *dst++ = 0xE0 | (src[i] >> 12);
                    *dst++ = 0x80 | ((src[i] >> 6) & 0x3F);
                    *dst++ = 0x80 | (src[i] & 0x3F);
                }
            }
            *dst = '\0';

            // Check for icu_root and route through UTF-8 handler if found
            if (strcasestr(utf8_sql, "collate icu_root")) {
                LOG_INFO("UTF-16 query with icu_root, routing to UTF-8 handler: %.200s", utf8_sql);
                const char *tail8 = NULL;
                int rc = my_sqlite3_prepare_v2(db, utf8_sql, -1, ppStmt, &tail8);
                free(utf8_sql);
                if (pzTail) *pzTail = NULL;  // Tail not accurate after conversion
                return rc;
            }
            free(utf8_sql);
        }
    }

    return sqlite3_prepare16_v2(db, zSql, nByte, ppStmt, pzTail);
}

static int my_sqlite3_prepare_v3(sqlite3 *db, const char *zSql, int nByte,
                                  unsigned int prepFlags, sqlite3_stmt **ppStmt,
                                  const char **pzTail) {
    // Log that prepare_v3 is being used
    if (zSql && strcasestr(zSql, "metadata_items")) {
        LOG_INFO("PREPARE_V3 metadata_items query: %.200s", zSql);
    }
    // Route through my_sqlite3_prepare_v2 to get icu_root cleanup and PG handling
    // We ignore prepFlags for now as they're SQLite-specific optimizations
    (void)prepFlags;
    return my_sqlite3_prepare_v2(db, zSql, nByte, ppStmt, pzTail);
}

// ============================================================================
// Interposed SQLite Functions - Bind
// ============================================================================

// Helper to map SQLite parameter index to PostgreSQL parameter index
// Handles named parameters (:name) by finding the correct position
static int pg_map_param_index(pg_stmt_t *pg_stmt, sqlite3_stmt *pStmt, int sqlite_idx) {
    if (!pg_stmt) {
        LOG_DEBUG("pg_map_param_index: no pg_stmt, using direct mapping idx=%d -> %d", sqlite_idx, sqlite_idx - 1);
        return sqlite_idx - 1;
    }

    // If we have named parameters, we need to map them
    if (pg_stmt->param_names && pg_stmt->param_count > 0) {
        // Get the parameter name from SQLite
        const char *param_name = sqlite3_bind_parameter_name(pStmt, sqlite_idx);
        LOG_DEBUG("pg_map_param_index: sqlite_idx=%d, param_name=%s, param_count=%d",
                 sqlite_idx, param_name ? param_name : "NULL", pg_stmt->param_count);

        if (param_name) {
            // Remove the : prefix if present
            const char *clean_name = param_name;
            if (param_name[0] == ':') clean_name = param_name + 1;

            // Debug: show all param names
            for (int i = 0; i < pg_stmt->param_count && i < 5; i++) {
                LOG_DEBUG("  param_names[%d] = %s", i, pg_stmt->param_names[i] ? pg_stmt->param_names[i] : "NULL");
            }

            // Find this name in our param_names array
            for (int i = 0; i < pg_stmt->param_count; i++) {
                if (pg_stmt->param_names[i] && strcmp(pg_stmt->param_names[i], clean_name) == 0) {
                    LOG_DEBUG("  -> Found match at pg_idx=%d", i);
                    return i;  // Found it! Return the PostgreSQL position
                }
            }
            LOG_ERROR("Named parameter '%s' not found in translation (sqlite_idx=%d)", clean_name, sqlite_idx);
        } else {
            LOG_DEBUG("  -> No parameter name, using direct mapping");
        }
    } else {
        LOG_DEBUG("pg_map_param_index: no param_names (count=%d), using direct mapping idx=%d -> %d",
                 pg_stmt->param_count, sqlite_idx, sqlite_idx - 1);
    }

    // For positional parameters (?) or if name not found, use direct mapping
    return sqlite_idx - 1;
}

static int my_sqlite3_bind_int(sqlite3_stmt *pStmt, int idx, int val) {
    int rc = sqlite3_bind_int(pStmt, idx, val);

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS) {
        pthread_mutex_lock(&pg_stmt->mutex);  // CRITICAL FIX: Protect bind operations
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            // Use pre-allocated buffer instead of strdup
            snprintf(pg_stmt->param_buffers[pg_idx], 32, "%d", val);
            pg_stmt->param_values[pg_idx] = pg_stmt->param_buffers[pg_idx];
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
    }

    return rc;
}

static int my_sqlite3_bind_int64(sqlite3_stmt *pStmt, int idx, sqlite3_int64 val) {
    int rc = sqlite3_bind_int64(pStmt, idx, val);

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS) {
        pthread_mutex_lock(&pg_stmt->mutex);  // CRITICAL FIX: Protect bind operations
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            // Use pre-allocated buffer instead of strdup
            snprintf(pg_stmt->param_buffers[pg_idx], 32, "%lld", val);
            pg_stmt->param_values[pg_idx] = pg_stmt->param_buffers[pg_idx];
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
    }

    return rc;
}

static int my_sqlite3_bind_double(sqlite3_stmt *pStmt, int idx, double val) {
    int rc = sqlite3_bind_double(pStmt, idx, val);

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS) {
        pthread_mutex_lock(&pg_stmt->mutex);  // CRITICAL FIX: Protect bind operations
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            // Use pre-allocated buffer instead of strdup
            snprintf(pg_stmt->param_buffers[pg_idx], 32, "%.17g", val);
            pg_stmt->param_values[pg_idx] = pg_stmt->param_buffers[pg_idx];
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
    }

    return rc;
}

// Helper: check if param_value points to pre-allocated buffer
static inline int is_preallocated_buffer(pg_stmt_t *stmt, int idx) {
    return stmt->param_values[idx] >= stmt->param_buffers[idx] &&
           stmt->param_values[idx] < stmt->param_buffers[idx] + 32;
}

// Helper: check if data contains binary bytes (non-UTF8 safe)
// Returns 1 if data contains bytes that would be invalid in UTF-8 text
static int contains_binary_bytes(const unsigned char *data, size_t len) {
    if (!data || len == 0) return 0;

    for (size_t i = 0; i < len; i++) {
        unsigned char c = data[i];
        // Control characters (except tab, newline, carriage return)
        if (c < 0x20 && c != 0x09 && c != 0x0A && c != 0x0D) {
            return 1;
        }
        // Check for invalid UTF-8 lead bytes or 0x7F (DEL)
        if (c == 0x7F || c == 0xC0 || c == 0xC1 || c >= 0xF5) {
            return 1;
        }
        // Gzip magic bytes (0x1f 0x8b) - common binary data
        if (i == 0 && len >= 2 && c == 0x1f && data[1] == 0x8b) {
            return 1;
        }
    }
    return 0;
}

// Helper: convert binary data to PostgreSQL hex format (\xHEXHEX...)
// Caller must free the returned string
static char* bytes_to_pg_hex(const unsigned char *data, size_t len) {
    if (!data || len == 0) return strdup("");

    // Format: \x followed by 2 hex chars per byte
    size_t hex_len = 2 + (len * 2) + 1;  // \x + hex + null
    char *hex = malloc(hex_len);
    if (!hex) return NULL;

    hex[0] = '\\';
    hex[1] = 'x';

    static const char hex_chars[] = "0123456789abcdef";
    for (size_t i = 0; i < len; i++) {
        hex[2 + i*2] = hex_chars[(data[i] >> 4) & 0x0F];
        hex[2 + i*2 + 1] = hex_chars[data[i] & 0x0F];
    }
    hex[hex_len - 1] = '\0';

    return hex;
}

static int my_sqlite3_bind_text(sqlite3_stmt *pStmt, int idx, const char *val,
                                 int nBytes, void (*destructor)(void*)) {
    int rc = sqlite3_bind_text(pStmt, idx, val, nBytes, destructor);

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS && val) {
        pthread_mutex_lock(&pg_stmt->mutex);  // CRITICAL FIX: Protect bind operations
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);

        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            // Free old value only if it was dynamically allocated
            if (pg_stmt->param_values[pg_idx] && !is_preallocated_buffer(pg_stmt, pg_idx)) {
                free(pg_stmt->param_values[pg_idx]);
                pg_stmt->param_values[pg_idx] = NULL;  // Prevent dangling pointer
            }

            size_t actual_len = (nBytes < 0) ? strlen(val) : (size_t)nBytes;

            // Check if data contains binary bytes (non-UTF8)
            // If so, convert to PostgreSQL hex format for BYTEA columns
            if (contains_binary_bytes((const unsigned char*)val, actual_len)) {
                LOG_DEBUG("bind_text: detected binary data at idx=%d, len=%zu, converting to hex", idx, actual_len);
                pg_stmt->param_values[pg_idx] = bytes_to_pg_hex((const unsigned char*)val, actual_len);
            } else if (nBytes < 0) {
                pg_stmt->param_values[pg_idx] = strdup(val);
            } else {
                pg_stmt->param_values[pg_idx] = malloc(nBytes + 1);
                if (pg_stmt->param_values[pg_idx]) {
                    memcpy(pg_stmt->param_values[pg_idx], val, nBytes);
                    pg_stmt->param_values[pg_idx][nBytes] = '\0';
                }
            }
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
    }

    return rc;
}

static int my_sqlite3_bind_blob(sqlite3_stmt *pStmt, int idx, const void *val,
                                 int nBytes, void (*destructor)(void*)) {
    int rc = sqlite3_bind_blob(pStmt, idx, val, nBytes, destructor);

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS && val && nBytes > 0) {
        pthread_mutex_lock(&pg_stmt->mutex);  // CRITICAL FIX: Protect bind operations
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            if (pg_stmt->param_values[pg_idx] && !is_preallocated_buffer(pg_stmt, pg_idx)) {
                free(pg_stmt->param_values[pg_idx]);
                pg_stmt->param_values[pg_idx] = NULL;  // Prevent dangling pointer
            }
            // Convert binary data to PostgreSQL hex format for BYTEA columns
            // This works in text mode (paramFormats=NULL means text mode)
            LOG_DEBUG("bind_blob: converting %d bytes to hex at idx=%d", nBytes, idx);
            pg_stmt->param_values[pg_idx] = bytes_to_pg_hex((const unsigned char*)val, (size_t)nBytes);
            pg_stmt->param_lengths[pg_idx] = 0;  // Use strlen for text mode
            pg_stmt->param_formats[pg_idx] = 0;  // text mode (hex string)
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
    }

    return rc;
}

// sqlite3_bind_blob64 - 64-bit version for large blobs
static int my_sqlite3_bind_blob64(sqlite3_stmt *pStmt, int idx, const void *val,
                                   sqlite3_uint64 nBytes, void (*destructor)(void*)) {
    int rc = sqlite3_bind_blob64(pStmt, idx, val, nBytes, destructor);

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS && val && nBytes > 0) {
        pthread_mutex_lock(&pg_stmt->mutex);  // CRITICAL FIX: Protect bind operations
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            if (pg_stmt->param_values[pg_idx] && !is_preallocated_buffer(pg_stmt, pg_idx)) {
                free(pg_stmt->param_values[pg_idx]);
                pg_stmt->param_values[pg_idx] = NULL;  // Prevent dangling pointer
            }
            // Convert binary data to PostgreSQL hex format for BYTEA columns
            LOG_DEBUG("bind_blob64: converting %llu bytes to hex at idx=%d", (unsigned long long)nBytes, idx);
            pg_stmt->param_values[pg_idx] = bytes_to_pg_hex((const unsigned char*)val, (size_t)nBytes);
            pg_stmt->param_lengths[pg_idx] = 0;  // Use strlen for text mode
            pg_stmt->param_formats[pg_idx] = 0;  // text mode (hex string)
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
    }

    return rc;
}

// sqlite3_bind_text64 - critical for Plex which uses this for text values!
static int my_sqlite3_bind_text64(sqlite3_stmt *pStmt, int idx, const char *val,
                                   sqlite3_uint64 nBytes, void (*destructor)(void*),
                                   unsigned char encoding) {
    int rc = sqlite3_bind_text64(pStmt, idx, val, nBytes, destructor, encoding);

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS && val) {
        pthread_mutex_lock(&pg_stmt->mutex);  // CRITICAL FIX: Protect bind operations
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            if (pg_stmt->param_values[pg_idx] && !is_preallocated_buffer(pg_stmt, pg_idx)) {
                free(pg_stmt->param_values[pg_idx]);
                pg_stmt->param_values[pg_idx] = NULL;  // Prevent dangling pointer
            }

            size_t actual_len = (nBytes == (sqlite3_uint64)-1) ? strlen(val) : (size_t)nBytes;

            // Check if data contains binary bytes (non-UTF8)
            // If so, convert to PostgreSQL hex format for BYTEA columns
            if (contains_binary_bytes((const unsigned char*)val, actual_len)) {
                LOG_DEBUG("bind_text64: detected binary data at idx=%d, len=%zu, converting to hex", idx, actual_len);
                pg_stmt->param_values[pg_idx] = bytes_to_pg_hex((const unsigned char*)val, actual_len);
            } else if (nBytes == (sqlite3_uint64)-1) {
                pg_stmt->param_values[pg_idx] = strdup(val);
            } else {
                pg_stmt->param_values[pg_idx] = malloc((size_t)nBytes + 1);
                if (pg_stmt->param_values[pg_idx]) {
                    memcpy(pg_stmt->param_values[pg_idx], val, (size_t)nBytes);
                    pg_stmt->param_values[pg_idx][(size_t)nBytes] = '\0';
                }
            }
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
    }

    return rc;
}

// sqlite3_bind_value - copies value from another sqlite3_value
static int my_sqlite3_bind_value(sqlite3_stmt *pStmt, int idx, const sqlite3_value *pValue) {
    int rc = sqlite3_bind_value(pStmt, idx, pValue);

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS && pValue) {
        pthread_mutex_lock(&pg_stmt->mutex);  // CRITICAL FIX: Protect bind operations
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            // Get value type and extract appropriately
            int vtype = sqlite3_value_type(pValue);
            if (pg_stmt->param_values[pg_idx] && !is_preallocated_buffer(pg_stmt, pg_idx)) {
                free(pg_stmt->param_values[pg_idx]);
                pg_stmt->param_values[pg_idx] = NULL;
            }

            switch (vtype) {
                case SQLITE_INTEGER: {
                    sqlite3_int64 v = sqlite3_value_int64(pValue);
                    char buf[32];
                    snprintf(buf, sizeof(buf), "%lld", v);
                    pg_stmt->param_values[pg_idx] = strdup(buf);
                    break;
                }
                case SQLITE_FLOAT: {
                    double v = sqlite3_value_double(pValue);
                    char buf[64];
                    snprintf(buf, sizeof(buf), "%.17g", v);
                    pg_stmt->param_values[pg_idx] = strdup(buf);
                    break;
                }
                case SQLITE_TEXT: {
                    const char *v = (const char *)sqlite3_value_text(pValue);
                    if (v) pg_stmt->param_values[pg_idx] = strdup(v);
                    break;
                }
                case SQLITE_BLOB: {
                    int len = sqlite3_value_bytes(pValue);
                    const void *v = sqlite3_value_blob(pValue);
                    if (v && len > 0) {
                        pg_stmt->param_values[pg_idx] = malloc(len);
                        if (pg_stmt->param_values[pg_idx]) {
                            memcpy(pg_stmt->param_values[pg_idx], v, len);
                        }
                        pg_stmt->param_lengths[pg_idx] = len;
                        pg_stmt->param_formats[pg_idx] = 1;  // binary
                    }
                    break;
                }
                case SQLITE_NULL:
                default:
                    // Leave as NULL
                    break;
            }
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
    }

    return rc;
}

static int my_sqlite3_bind_null(sqlite3_stmt *pStmt, int idx) {
    int rc = sqlite3_bind_null(pStmt, idx);

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS) {
        pthread_mutex_lock(&pg_stmt->mutex);  // CRITICAL FIX: Protect bind operations
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            if (pg_stmt->param_values[pg_idx] && !is_preallocated_buffer(pg_stmt, pg_idx)) {
                free(pg_stmt->param_values[pg_idx]);
                pg_stmt->param_values[pg_idx] = NULL;
            }
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
    }

    return rc;
}

// ============================================================================
// Interposed SQLite Functions - Step
// ============================================================================

static int my_sqlite3_step(sqlite3_stmt *pStmt) {
    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);

    // Skip statements
    if (pg_stmt && pg_stmt->is_pg == 3) {
        return SQLITE_DONE;
    }

    // Handle cached statements (prepared before our shim)
    if (!pg_stmt) {
        sqlite3 *db = sqlite3_db_handle(pStmt);
        pg_connection_t *pg_conn = pg_find_connection(db);
        if (!pg_conn) pg_conn = pg_find_any_library_connection();

        if (pg_conn && pg_conn->is_pg_active && pg_conn->conn) {
            char *expanded_sql = sqlite3_expanded_sql(pStmt);
            const char *sql = expanded_sql ? expanded_sql : sqlite3_sql(pStmt);

            // Handle cached WRITE
            // Check both expanded SQL and original SQL for skip patterns
            const char *orig_sql = sqlite3_sql(pStmt);
            if (sql && is_write_operation(sql) && !should_skip_sql(sql) && !should_skip_sql(orig_sql)) {
                // Debug: log cached INSERT for metadata_items
                if (sql && strcasestr(sql, "INSERT") && strcasestr(sql, "metadata_items")) {
                    LOG_ERROR("CACHED INSERT metadata_items:");
                    LOG_ERROR("  expanded_sql=%s", expanded_sql ? "YES" : "NO");
                    LOG_ERROR("  sql (first 300): %.300s", sql ? sql : "(null)");
                }
                // CRITICAL FIX: Check if this cached write was already executed
                pg_stmt_t *cached = pg_find_cached_stmt(pStmt);
                if (cached && cached->write_executed) {
                    // Already executed, prevent duplicate execution
                    if (expanded_sql) sqlite3_free(expanded_sql);
                    return SQLITE_DONE;
                }

                // For cached statements, also use thread-local connection
                pg_connection_t *cached_exec_conn = pg_conn;
                if (is_library_db_path(pg_conn->db_path)) {
                    pg_connection_t *thread_conn = pg_get_thread_connection(pg_conn->db_path);
                    if (thread_conn && thread_conn->is_pg_active && thread_conn->conn) {
                        cached_exec_conn = thread_conn;
                    }
                }

                sql_translation_t trans = sql_translate(sql);
                if (trans.success && trans.sql) {
                    char *exec_sql = trans.sql;
                    char *insert_sql = convert_metadata_settings_insert_to_upsert(trans.sql);
                    if (insert_sql) {
                        exec_sql = insert_sql;
                    } else if (strncasecmp(sql, "INSERT", 6) == 0 && !strstr(trans.sql, "RETURNING")) {
                        size_t len = strlen(trans.sql);
                        insert_sql = malloc(len + 20);
                        if (insert_sql) {
                            snprintf(insert_sql, len + 20, "%s RETURNING id", trans.sql);
                            exec_sql = insert_sql;
                        }
                    }

                    // Log cached INSERT on play_queue_generators
                    if (strstr(sql, "play_queue_generators")) {
                        LOG_INFO("CACHED INSERT play_queue_generators on thread %p conn %p",
                                (void*)pthread_self(), (void*)cached_exec_conn);
                    }

                    // CRITICAL: Touch connection to prevent pool from releasing it during query
                    pg_pool_touch_connection(cached_exec_conn);

                    // CRITICAL: Lock connection mutex to prevent concurrent libpq access
                    pthread_mutex_lock(&cached_exec_conn->mutex);

                    // Drain any pending results before executing
                    PQsetnonblocking(cached_exec_conn->conn, 0);
                    while (PQisBusy(cached_exec_conn->conn)) {
                        PQconsumeInput(cached_exec_conn->conn);
                    }
                    PGresult *pending;
                    while ((pending = PQgetResult(cached_exec_conn->conn)) != NULL) {
                        LOG_ERROR("CACHED EXEC: Drained orphaned result from connection %p", (void*)cached_exec_conn);
                        PQclear(pending);
                    }

                    PGresult *res = PQexec(cached_exec_conn->conn, exec_sql);
                    pthread_mutex_unlock(&cached_exec_conn->mutex);
                    ExecStatusType status = PQresultStatus(res);

                    if (status == PGRES_COMMAND_OK || status == PGRES_TUPLES_OK) {
                        pg_conn->last_changes = atoi(PQcmdTuples(res) ?: "1");

                        if (strncasecmp(sql, "INSERT", 6) == 0 && status == PGRES_TUPLES_OK && PQntuples(res) > 0) {
                            const char *id_str = PQgetvalue(res, 0, 0);
                            if (id_str && *id_str) {
                                sqlite3_int64 meta_id = extract_metadata_id_from_generator_sql(sql);
                                if (meta_id > 0) pg_set_global_metadata_id(meta_id);
                            }
                        }
                    } else {
                        const char *err = (pg_conn && pg_conn->conn) ? PQerrorMessage(pg_conn->conn) : "NULL connection";
                        log_sql_fallback(sql, exec_sql, err, "CACHED WRITE");
                        // CRITICAL: Check if connection is corrupted and needs reset
                        pg_pool_check_connection_health(cached_exec_conn);
                    }

                    if (insert_sql) free(insert_sql);
                    PQclear(res);

                    // CRITICAL FIX: Create cached stmt entry and mark as executed
                    if (!cached) {
                        cached = pg_stmt_create(cached_exec_conn, sql, pStmt);
                        if (cached) {
                            cached->is_pg = 1;  // WRITE
                            cached->is_cached = 1;
                            cached->write_executed = 1;  // Mark as executed
                            pg_register_cached_stmt(pStmt, cached);
                        }
                    } else {
                        cached->write_executed = 1;  // Mark as executed
                    }
                }
                sql_translation_free(&trans);
                if (expanded_sql) sqlite3_free(expanded_sql);
                return SQLITE_DONE;
            }

            // Handle cached READ
            if (sql && is_read_operation(sql) && !should_skip_sql(sql)) {
                // For cached statements, also use thread-local connection
                pg_connection_t *cached_read_conn = pg_conn;
                if (is_library_db_path(pg_conn->db_path)) {
                    pg_connection_t *thread_conn = pg_get_thread_connection(pg_conn->db_path);
                    if (thread_conn && thread_conn->is_pg_active && thread_conn->conn) {
                        cached_read_conn = thread_conn;
                    }
                }

                pg_stmt_t *cached = pg_find_cached_stmt(pStmt);
                int sqlite_result = sqlite3_step(pStmt);

                if (sqlite_result == SQLITE_ROW || sqlite_result == SQLITE_DONE) {
                    // For cached statements:
                    // - First call (no result): execute PostgreSQL query
                    // - Subsequent calls: advance through results
                    if (cached && cached->result) {
                        // Already have results, advance to next row
                        cached->current_row++;
                        if (cached->current_row >= cached->num_rows) {
                            // CRITICAL FIX: Free PGresult immediately when done
                            // Prevents memory accumulation when Plex doesn't call reset()
                            PQclear(cached->result);
                            cached->result = NULL;
                            if (expanded_sql) sqlite3_free(expanded_sql);
                            return SQLITE_DONE;
                        }
                        if (expanded_sql) sqlite3_free(expanded_sql);
                        return SQLITE_ROW;
                    }

                    // No result yet - execute PostgreSQL query
                    sql_translation_t trans = sql_translate(sql);
                    if (trans.success && trans.sql) {
                        // Verbose query logging disabled for performance

                        pg_stmt_t *new_stmt = cached;
                        if (!new_stmt) {
                            new_stmt = pg_stmt_create(cached_read_conn, sql, pStmt);
                            if (new_stmt) {
                                new_stmt->pg_sql = strdup(trans.sql);
                                new_stmt->is_pg = 2;
                                new_stmt->is_cached = 1;
                                pg_register_cached_stmt(pStmt, new_stmt);
                            }
                        }
                        if (new_stmt) {
                            // CRITICAL: Touch connection to prevent pool from releasing it during query
                            pg_pool_touch_connection(cached_read_conn);

                            // CRITICAL: Lock connection mutex to prevent concurrent libpq access
                            pthread_mutex_lock(&cached_read_conn->mutex);

                            // Drain any pending results before executing
                            PQsetnonblocking(cached_read_conn->conn, 0);
                            while (PQisBusy(cached_read_conn->conn)) {
                                PQconsumeInput(cached_read_conn->conn);
                            }
                            PGresult *pending_read;
                            while ((pending_read = PQgetResult(cached_read_conn->conn)) != NULL) {
                                LOG_ERROR("CACHED READ: Drained orphaned result from connection %p", (void*)cached_read_conn);
                                PQclear(pending_read);
                            }

                            LOG_INFO("PQEXEC CACHED READ: conn=%p sql_len=%zu sql=%.60s",
                                     (void*)cached_read_conn, strlen(trans.sql), trans.sql);
                            new_stmt->result = PQexec(cached_read_conn->conn, trans.sql);
                            LOG_INFO("PQEXEC CACHED READ DONE: conn=%p result=%p",
                                     (void*)cached_read_conn, (void*)new_stmt->result);
                            pthread_mutex_unlock(&cached_read_conn->mutex);
                            if (PQresultStatus(new_stmt->result) == PGRES_TUPLES_OK) {
                                new_stmt->num_rows = PQntuples(new_stmt->result);
                                new_stmt->num_cols = PQnfields(new_stmt->result);
                                new_stmt->current_row = 0;
                                // Verbose result logging disabled for performance
                                sql_translation_free(&trans);
                                if (expanded_sql) sqlite3_free(expanded_sql);
                                return (new_stmt->num_rows > 0) ? SQLITE_ROW : SQLITE_DONE;
                            } else {
                                const char *err = (cached_read_conn && cached_read_conn->conn) ? PQerrorMessage(cached_read_conn->conn) : "NULL connection";
                                log_sql_fallback(sql, trans.sql, err, "CACHED READ");
                                PQclear(new_stmt->result);
                                new_stmt->result = NULL;
                                // CRITICAL: Check if connection is corrupted and needs reset
                                pg_pool_check_connection_health(cached_read_conn);
                            }
                        }
                    }
                    sql_translation_free(&trans);
                }

                if (expanded_sql) sqlite3_free(expanded_sql);
                return sqlite_result;
            }

            if (expanded_sql) sqlite3_free(expanded_sql);
        }
    }

    // Execute prepared statement on PostgreSQL
    // IMPORTANT: Use thread-local connection, not the one stored at prepare time
    // This ensures INSERT and SELECT on the same thread use the same connection
    pg_connection_t *exec_conn = pg_stmt ? pg_stmt->conn : NULL;
    if (pg_stmt && is_library_db_path(pg_stmt->conn ? pg_stmt->conn->db_path : NULL)) {
        pg_connection_t *thread_conn = pg_get_thread_connection(pg_stmt->conn->db_path);
        if (thread_conn && thread_conn->is_pg_active && thread_conn->conn) {
            exec_conn = thread_conn;
        } else if (thread_conn) {
            LOG_ERROR("STEP: thread_conn not usable (is_pg_active=%d, conn=%p)",
                     thread_conn->is_pg_active, (void*)thread_conn->conn);
        } else {
            LOG_ERROR("STEP: pg_get_thread_connection returned NULL");
        }
    }

    if (pg_stmt && pg_stmt->pg_sql && exec_conn && exec_conn->conn) {
        // Lock statement mutex to protect statement state
        // NOTE: exec_conn->mutex is NOT needed because each thread has its own
        // connection from the pool (per-thread connection model)
        pthread_mutex_lock(&pg_stmt->mutex);

        const char *paramValues[MAX_PARAMS] = {NULL};  // Initialize to prevent garbage access
        for (int i = 0; i < pg_stmt->param_count && i < MAX_PARAMS; i++) {
            paramValues[i] = pg_stmt->param_values[i];
        }

        if (pg_stmt->is_pg == 2) {  // READ
            // CRITICAL FIX: Prevent re-execution after SQLITE_DONE was returned
            // Without this, Plex calling step() after DONE would re-execute the query
            if (pg_stmt->read_done) {
                pthread_mutex_unlock(&pg_stmt->mutex);
                return SQLITE_DONE;
            }

            // Only log when result is NULL (new query) to reduce log spam
            if (!pg_stmt->result) {
                LOG_DEBUG("STEP READ: thread=%p stmt=%p exec_conn=%p",
                         (void*)pthread_self(), (void*)pg_stmt, (void*)exec_conn);
            }

            // CRITICAL FIX: Check if result belongs to a different connection
            // If statement is being used by a different thread/connection, we must
            // re-execute the query on THIS thread's connection to avoid protocol desync
            if (pg_stmt->result && pg_stmt->result_conn != exec_conn) {
                LOG_ERROR("STEP: Result from different connection! Clearing result (result_conn=%p exec_conn=%p)",
                         (void*)pg_stmt->result_conn, (void*)exec_conn);
                PQclear(pg_stmt->result);
                pg_stmt->result = NULL;
                pg_stmt->result_conn = NULL;
                pg_stmt->current_row = 0;
            }

            if (!pg_stmt->result) {
                // Track which thread is executing this statement
                pthread_t current = pthread_self();
                pg_stmt->executing_thread = current;

                // Validate connection before use to prevent crash
                if (!exec_conn || !exec_conn->conn || PQstatus(exec_conn->conn) != CONNECTION_OK) {
                    LOG_ERROR("STEP SELECT: Invalid connection (exec_conn=%p, conn=%p, status=%d)",
                             (void*)exec_conn,
                             exec_conn ? (void*)exec_conn->conn : NULL,
                             exec_conn && exec_conn->conn ? (int)PQstatus(exec_conn->conn) : -1);
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return SQLITE_ERROR;
                }

                // CRITICAL: Touch connection to prevent pool from releasing it during long queries
                pg_pool_touch_connection(exec_conn);

                // CRITICAL: Lock connection mutex for ENTIRE query lifecycle
                // This prevents protocol desync - PQprepare and PQexecPrepared must be atomic
                pthread_mutex_lock(&exec_conn->mutex);

                // CRITICAL: Check connection status before query
                // If connection is in a bad state, reset it
                ConnStatusType conn_status = PQstatus(exec_conn->conn);
                if (conn_status != CONNECTION_OK) {
                    LOG_ERROR("STEP READ: Connection bad (status=%d), resetting...", (int)conn_status);
                    PQreset(exec_conn->conn);
                    if (PQstatus(exec_conn->conn) != CONNECTION_OK) {
                        LOG_ERROR("STEP READ: Reset failed, connection lost");
                        pthread_mutex_unlock(&exec_conn->mutex);
                        pthread_mutex_unlock(&pg_stmt->mutex);
                        return SQLITE_ERROR;
                    }
                    // Re-apply settings after reset
                    pg_conn_config_t *cfg = pg_config_get();
                    if (cfg) {
                        char schema_cmd[256];
                        snprintf(schema_cmd, sizeof(schema_cmd), "SET search_path TO %s, public", cfg->schema);
                        PGresult *r = PQexec(exec_conn->conn, schema_cmd);
                        PQclear(r);
                        r = PQexec(exec_conn->conn, "SET statement_timeout = '10s'");
                        PQclear(r);
                    }
                }

                // Ensure connection is in blocking mode and consume any pending data
                PQsetnonblocking(exec_conn->conn, 0);  // Force blocking mode
                while (PQisBusy(exec_conn->conn)) {
                    PQconsumeInput(exec_conn->conn);
                }
                PGresult *pending;
                while ((pending = PQgetResult(exec_conn->conn)) != NULL) {
                    LOG_ERROR("STEP: Drained orphaned result from connection %p", (void*)exec_conn);
                    PQclear(pending);
                }

                // Use prepared statements for better performance (skip parse/plan overhead)
                if (pg_stmt->use_prepared && pg_stmt->stmt_name[0]) {
                    LOG_INFO("PREPARED PATH: use_prepared=%d stmt_name=%s sql=%.60s",
                             pg_stmt->use_prepared, pg_stmt->stmt_name, pg_stmt->pg_sql);
                    const char *cached_name = NULL;
                    int is_cached = pg_stmt_cache_lookup(exec_conn, pg_stmt->sql_hash, &cached_name);

                    if (!is_cached) {
                        // Prepare statement on this connection (mutex already held)
                        PGresult *prep_res = PQprepare(exec_conn->conn, pg_stmt->stmt_name,
                                                        pg_stmt->pg_sql, pg_stmt->param_count, NULL);
                        if (PQresultStatus(prep_res) == PGRES_COMMAND_OK) {
                            pg_stmt_cache_add(exec_conn, pg_stmt->sql_hash, pg_stmt->stmt_name, pg_stmt->param_count);
                            cached_name = pg_stmt->stmt_name;
                            is_cached = 1;
                            LOG_DEBUG("PREPARED_STMT: New statement %s (params=%d)", pg_stmt->stmt_name, pg_stmt->param_count);
                        } else {
                            // Prepare failed - fall back to PQexecParams
                            LOG_DEBUG("PQprepare failed for %s: %s", pg_stmt->stmt_name, PQerrorMessage(exec_conn->conn));
                        }
                        PQclear(prep_res);
                    } else {
                        LOG_DEBUG("PREPARED_STMT: Cache hit for %s", cached_name);
                    }

                    if (is_cached && cached_name) {
                        // Execute prepared statement
                        pg_stmt->result = PQexecPrepared(exec_conn->conn, cached_name,
                            pg_stmt->param_count, paramValues, NULL, NULL, 0);
                    } else {
                        // Fallback to PQexecParams
                        pg_stmt->result = PQexecParams(exec_conn->conn, pg_stmt->pg_sql,
                            pg_stmt->param_count, NULL, paramValues, NULL, NULL, 0);
                    }
                } else {
                    // No prepared statement support for this query
                    LOG_INFO("EXEC_PARAMS READ: conn=%p params=%d sql=%.60s",
                             (void*)exec_conn, pg_stmt->param_count, pg_stmt->pg_sql);
                    pg_stmt->result = PQexecParams(exec_conn->conn, pg_stmt->pg_sql,
                        pg_stmt->param_count, NULL, paramValues, NULL, NULL, 0);
                    LOG_INFO("EXEC_PARAMS READ DONE: conn=%p result=%p",
                             (void*)exec_conn, (void*)pg_stmt->result);
                }

                pthread_mutex_unlock(&exec_conn->mutex);

                // Check for query errors
                ExecStatusType status = PQresultStatus(pg_stmt->result);
                if (status != PGRES_TUPLES_OK && status != PGRES_COMMAND_OK) {
                    const char *err = (exec_conn && exec_conn->conn) ? PQerrorMessage(exec_conn->conn) : "NULL connection";
                    LOG_ERROR("PostgreSQL query failed: %s", err);
                    LOG_ERROR("Failed query: %.500s", pg_stmt->pg_sql);
                }

                if (PQresultStatus(pg_stmt->result) == PGRES_TUPLES_OK) {
                    pg_stmt->num_rows = PQntuples(pg_stmt->result);
                    pg_stmt->num_cols = PQnfields(pg_stmt->result);
                    pg_stmt->current_row = 0;
                    pg_stmt->result_conn = exec_conn;  // Track which connection owns this result

                    // Verbose query logging disabled for performance
                } else {
                    const char *err = (exec_conn && exec_conn->conn) ? PQerrorMessage(exec_conn->conn) : "NULL connection";
                    log_sql_fallback(pg_stmt->sql, pg_stmt->pg_sql,
                                     err, "PREPARED READ");
                    PQclear(pg_stmt->result);
                    pg_stmt->result = NULL;
                    pg_stmt->result_conn = NULL;
                    // CRITICAL: Check if connection is corrupted and needs reset
                    pg_pool_check_connection_health(exec_conn);
                }
            } else {
                pg_stmt->current_row++;
            }

            if (pg_stmt->result) {
                if (pg_stmt->current_row >= pg_stmt->num_rows) {
                    // CRITICAL FIX: Free PGresult immediately when done
                    // Prevents memory accumulation when Plex doesn't call reset()
                    PQclear(pg_stmt->result);
                    pg_stmt->result = NULL;
                    pg_stmt->result_conn = NULL;
                    pg_stmt->read_done = 1;  // Prevent re-execution on next step() call
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return SQLITE_DONE;
                }
                pthread_mutex_unlock(&pg_stmt->mutex);
                return SQLITE_ROW;
            }
        } else if (pg_stmt->is_pg == 1) {  // WRITE
            // CRITICAL FIX: Prevent duplicate execution of the same write
            // If Plex calls step() multiple times without reset(), only execute once
            if (pg_stmt->write_executed) {
                // Already executed this write, just return DONE
                // This prevents the statistics_media INSERT storm bug
                pthread_mutex_unlock(&pg_stmt->mutex);
                return SQLITE_DONE;
            }

            // Log INSERT on play_queue_generators for debugging
            if (pg_stmt->pg_sql && strstr(pg_stmt->pg_sql, "play_queue_generators")) {
                LOG_INFO("INSERT play_queue_generators on thread %p conn %p",
                        (void*)pthread_self(), (void*)exec_conn);
            }

            // Debug: log INSERT params for troubleshooting
            if (pg_stmt->sql && strcasestr(pg_stmt->sql, "INSERT INTO metadata_items")) {
                LOG_ERROR("STEP metadata_items INSERT: param_count=%d", pg_stmt->param_count);
                // CRITICAL FIX: Only access paramValues within bounds
                LOG_ERROR("  PARAMS: [0]=%s [1]=%s [2]=%s [8]=%s [9]=%s",
                         (pg_stmt->param_count > 0 && paramValues[0]) ? paramValues[0] : "NULL",
                         (pg_stmt->param_count > 1 && paramValues[1]) ? paramValues[1] : "NULL",
                         (pg_stmt->param_count > 2 && paramValues[2]) ? paramValues[2] : "NULL",
                         (pg_stmt->param_count > 8 && paramValues[8]) ? paramValues[8] : "NULL",  // title
                         (pg_stmt->param_count > 9 && paramValues[9]) ? paramValues[9] : "NULL"); // title_sort
            }
            // Debug: log play_queue_generators INSERT params
            if (pg_stmt->sql && strcasestr(pg_stmt->sql, "play_queue_generators")) {
                LOG_ERROR("STEP play_queue_generators INSERT: param_count=%d", pg_stmt->param_count);
                // CRITICAL FIX: Only access paramValues within bounds
                LOG_ERROR("  PARAMS: [0]=%s [1]=%s [2]=%s [3]=%s",
                         (pg_stmt->param_count > 0 && paramValues[0]) ? paramValues[0] : "NULL",  // playlist_id
                         (pg_stmt->param_count > 1 && paramValues[1]) ? paramValues[1] : "NULL",  // metadata_item_id
                         (pg_stmt->param_count > 2 && paramValues[2]) ? paramValues[2] : "NULL",  // uri
                         (pg_stmt->param_count > 3 && paramValues[3]) ? paramValues[3] : "NULL"); // limit
                LOG_ERROR("  SQL: %.300s", pg_stmt->pg_sql ? pg_stmt->pg_sql : "NULL");
            }

            // VALIDATION: Skip statistics_media INSERTs with empty count AND duration
            // This prevents the 310M empty rows bug
            if (pg_stmt->pg_sql && strcasestr(pg_stmt->pg_sql, "statistics_media")) {
                // Check if count (param 6) and duration (param 7) are both 0 or NULL
                const char *count_val = (pg_stmt->param_count > 6) ? paramValues[6] : NULL;
                const char *duration_val = (pg_stmt->param_count > 7) ? paramValues[7] : NULL;
                int count_empty = !count_val || strcmp(count_val, "0") == 0;
                int duration_empty = !duration_val || strcmp(duration_val, "0") == 0;
                if (count_empty && duration_empty) {
                    LOG_INFO("SKIP statistics_media INSERT: count=%s duration=%s (empty)",
                            count_val ? count_val : "NULL", duration_val ? duration_val : "NULL");
                    pg_stmt->write_executed = 1;
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return SQLITE_DONE;
                }
            }

            // Validate connection before use to prevent crash
            if (!exec_conn || !exec_conn->conn || PQstatus(exec_conn->conn) != CONNECTION_OK) {
                LOG_ERROR("STEP: Invalid connection, reconnecting...");
                pg_stmt->write_executed = 1;
                pthread_mutex_unlock(&pg_stmt->mutex);
                return SQLITE_ERROR;
            }

            // CRITICAL: Touch connection to prevent pool from releasing it during query
            pg_pool_touch_connection(exec_conn);

            // CRITICAL: Lock connection mutex to ensure atomic query execution
            pthread_mutex_lock(&exec_conn->mutex);

            // CRITICAL: Ensure connection is in blocking mode and consume any pending data
            PQsetnonblocking(exec_conn->conn, 0);
            while (PQisBusy(exec_conn->conn)) {
                PQconsumeInput(exec_conn->conn);
            }
            PGresult *pending;
            while ((pending = PQgetResult(exec_conn->conn)) != NULL) {
                LOG_ERROR("STEP WRITE: Drained orphaned result from connection %p", (void*)exec_conn);
                PQclear(pending);
            }

            // Execute write
            PGresult *res = NULL;

            // Use prepared statements for better performance (skip parse/plan overhead)
            if (pg_stmt->use_prepared && pg_stmt->stmt_name[0]) {
                const char *cached_name = NULL;
                int is_cached = pg_stmt_cache_lookup(exec_conn, pg_stmt->sql_hash, &cached_name);

                if (!is_cached) {
                    // Prepare statement on this connection
                    PGresult *prep_res = PQprepare(exec_conn->conn, pg_stmt->stmt_name,
                                                    pg_stmt->pg_sql, pg_stmt->param_count, NULL);
                    if (PQresultStatus(prep_res) == PGRES_COMMAND_OK) {
                        pg_stmt_cache_add(exec_conn, pg_stmt->sql_hash, pg_stmt->stmt_name, pg_stmt->param_count);
                        cached_name = pg_stmt->stmt_name;
                        is_cached = 1;
                    } else {
                        // Prepare failed - fall back to PQexecParams
                        LOG_DEBUG("PQprepare (write) failed for %s: %s", pg_stmt->stmt_name, PQerrorMessage(exec_conn->conn));
                    }
                    PQclear(prep_res);
                }

                if (is_cached && cached_name) {
                    // Execute prepared statement
                    res = PQexecPrepared(exec_conn->conn, cached_name,
                        pg_stmt->param_count, paramValues, NULL, NULL, 0);
                } else {
                    // Fallback to PQexecParams
                    res = PQexecParams(exec_conn->conn, pg_stmt->pg_sql,
                        pg_stmt->param_count, NULL, paramValues, NULL, NULL, 0);
                }
            } else {
                // No prepared statement support for this query
                res = PQexecParams(exec_conn->conn, pg_stmt->pg_sql,
                    pg_stmt->param_count, NULL, paramValues, NULL, NULL, 0);
            }

            pthread_mutex_unlock(&exec_conn->mutex);

            ExecStatusType status = PQresultStatus(res);
            if (status == PGRES_COMMAND_OK || status == PGRES_TUPLES_OK) {
                exec_conn->last_changes = atoi(PQcmdTuples(res) ?: "1");

                // Extract metadata_id for play_queue_generators (for IN(NULL) fix)
                if (status == PGRES_TUPLES_OK && PQntuples(res) > 0) {
                    const char *id_str = PQgetvalue(res, 0, 0);
                    if (id_str && *id_str) {
                        if (pg_stmt->pg_sql && strstr(pg_stmt->pg_sql, "play_queue_generators")) {
                            LOG_INFO("STEP play_queue_generators: RETURNING id = %s on thread %p conn %p",
                                    id_str, (void*)pthread_self(), (void*)exec_conn);
                        }
                        sqlite3_int64 meta_id = extract_metadata_id_from_generator_sql(pg_stmt->sql);
                        if (meta_id > 0) pg_set_global_metadata_id(meta_id);
                    }
                }
            } else {
                const char *err = (exec_conn && exec_conn->conn) ? PQerrorMessage(exec_conn->conn) : "NULL connection";
                LOG_ERROR("STEP PG write error: %s", err);
                // CRITICAL: Check if connection is corrupted and needs reset
                pg_pool_check_connection_health(exec_conn);
            }

            // Mark as executed to prevent re-execution on subsequent step() calls
            pg_stmt->write_executed = 1;
            PQclear(res);
        }

        pthread_mutex_unlock(&pg_stmt->mutex);
    }

    if (pg_stmt && pg_stmt->is_pg) {
        if (pg_stmt->is_pg == 1) return SQLITE_DONE;
    }

    return sqlite3_step(pStmt);
}

// ============================================================================
// Interposed SQLite Functions - Reset/Finalize
// ============================================================================

static int my_sqlite3_reset(sqlite3_stmt *pStmt) {
    // Clear prepared statements
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt) {
        for (int i = 0; i < MAX_PARAMS; i++) {
            if (pg_stmt->param_values[i] && !is_preallocated_buffer(pg_stmt, i)) {
                free(pg_stmt->param_values[i]);
                pg_stmt->param_values[i] = NULL;
            }
        }
        pg_stmt_clear_result(pg_stmt);  // This also resets write_executed
    }

    // Also clear cached statements - these use a separate registry
    pg_stmt_t *cached = pg_find_cached_stmt(pStmt);
    if (cached) {
        pg_stmt_clear_result(cached);  // This also resets write_executed
    }

    return sqlite3_reset(pStmt);
}

static int my_sqlite3_finalize(sqlite3_stmt *pStmt) {
    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt) {
        // Statement is in global registry
        // CRITICAL FIX: Use weak clear for TLS (in case same stmt is also cached there)
        // This prevents double-free: global registry owns the reference
        pg_clear_cached_stmt_weak(pStmt);
        pg_unregister_stmt(pStmt);
        pg_stmt_unref(pg_stmt);
    } else {
        // Statement might only be in TLS cache - use normal clear which unrefs
        pg_clear_cached_stmt(pStmt);
    }
    return sqlite3_finalize(pStmt);
}

static int my_sqlite3_clear_bindings(sqlite3_stmt *pStmt) {
    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt) {
        for (int i = 0; i < MAX_PARAMS; i++) {
            if (pg_stmt->param_values[i] && !is_preallocated_buffer(pg_stmt, i)) {
                free(pg_stmt->param_values[i]);
                pg_stmt->param_values[i] = NULL;
            }
        }
    }
    return sqlite3_clear_bindings(pStmt);
}

// ============================================================================
// Interposed SQLite Functions - Column Values
// ============================================================================

static int my_sqlite3_column_count(sqlite3_stmt *pStmt) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);
        if (!pg_stmt->result) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return sqlite3_column_count(pStmt);
        }
        int count = pg_stmt->num_cols;
        pthread_mutex_unlock(&pg_stmt->mutex);
        return count;
    }
    return sqlite3_column_count(pStmt);
}

static int my_sqlite3_column_type(sqlite3_stmt *pStmt, int idx) {
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
    return sqlite3_column_type(pStmt, idx);
}

static int my_sqlite3_column_int(sqlite3_stmt *pStmt, int idx) {
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
    return sqlite3_column_int(pStmt, idx);
}

static sqlite3_int64 my_sqlite3_column_int64(sqlite3_stmt *pStmt, int idx) {
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
    return sqlite3_column_int64(pStmt, idx);
}

static double my_sqlite3_column_double(sqlite3_stmt *pStmt, int idx) {
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
    return sqlite3_column_double(pStmt, idx);
}

static const unsigned char* my_sqlite3_column_text(sqlite3_stmt *pStmt, int idx) {
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
    return sqlite3_column_text(pStmt, idx);
}

// Helper to decode PostgreSQL hex-encoded BYTEA to binary
// PostgreSQL BYTEA hex format: \x followed by hex digits (2 per byte)
// Returns decoded data and sets out_length. Caller must NOT free the result.
static const void* pg_decode_bytea(pg_stmt_t *pg_stmt, int row, int col, int *out_length) {
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

static const void* my_sqlite3_column_blob(sqlite3_stmt *pStmt, int idx) {
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
    return sqlite3_column_blob(pStmt, idx);
}

static int my_sqlite3_column_bytes(sqlite3_stmt *pStmt, int idx) {
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
    return sqlite3_column_bytes(pStmt, idx);
}

static const char* my_sqlite3_column_name(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);
        if (!pg_stmt->result) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return sqlite3_column_name(pStmt, idx);
        }
        if (idx >= 0 && idx < pg_stmt->num_cols) {
            const char *name = PQfname(pg_stmt->result, idx);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return name;
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
    }
    return sqlite3_column_name(pStmt, idx);
}

// sqlite3_column_value returns a pointer to a sqlite3_value for a column.
// For PostgreSQL statements, we return a fake sqlite3_value that encodes the pg_stmt and column.
// The sqlite3_value_* functions will decode this to return proper PostgreSQL data.
static sqlite3_value* my_sqlite3_column_value(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);
        if (!pg_stmt->result) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return sqlite3_column_value(pStmt, idx);
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
    return sqlite3_column_value(pStmt, idx);
}

// Intercept sqlite3_value_type to handle our fake values
static int my_sqlite3_value_type(sqlite3_value *pVal) {
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
    return sqlite3_value_type(pVal);
}

// Intercept sqlite3_value_text to handle our fake values
static const unsigned char* my_sqlite3_value_text(sqlite3_value *pVal) {
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
    return sqlite3_value_text(pVal);
}

// Intercept sqlite3_value_int to handle our fake values
static int my_sqlite3_value_int(sqlite3_value *pVal) {
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
    return sqlite3_value_int(pVal);
}

// Intercept sqlite3_value_int64 to handle our fake values
static sqlite3_int64 my_sqlite3_value_int64(sqlite3_value *pVal) {
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
    return sqlite3_value_int64(pVal);
}

// Intercept sqlite3_value_double to handle our fake values
static double my_sqlite3_value_double(sqlite3_value *pVal) {
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
    return sqlite3_value_double(pVal);
}

// Intercept sqlite3_value_bytes to handle our fake values
static int my_sqlite3_value_bytes(sqlite3_value *pVal) {
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
    return sqlite3_value_bytes(pVal);
}

// Intercept sqlite3_value_blob to handle our fake values
static const void* my_sqlite3_value_blob(sqlite3_value *pVal) {
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
    return sqlite3_value_blob(pVal);
}

static int my_sqlite3_data_count(sqlite3_stmt *pStmt) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        pthread_mutex_lock(&pg_stmt->mutex);
        if (!pg_stmt->result) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return sqlite3_data_count(pStmt);
        }
        int count = (pg_stmt->current_row < pg_stmt->num_rows) ? pg_stmt->num_cols : 0;
        pthread_mutex_unlock(&pg_stmt->mutex);
        return count;
    }
    return sqlite3_data_count(pStmt);
}

// ============================================================================
// Interposed SQLite Functions - Changes/Last Insert Rowid
// ============================================================================

static int my_sqlite3_changes(sqlite3 *db) {
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

static sqlite3_int64 my_sqlite3_changes64(sqlite3 *db) {
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

static sqlite3_int64 my_sqlite3_last_insert_rowid(sqlite3 *db) {
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
// Interposed SQLite Functions - Error Handling
// ============================================================================

// CRITICAL FIX for SOCI "not an error" exception:
// When we abort prepare_v2 early (e.g., stack protection), we return an error
// code BUT never call the real sqlite3_prepare_v2, so SQLite's internal error
// state remains SQLITE_OK. SOCI then calls sqlite3_errmsg() and gets "not an
// error" instead of our actual error message. We must intercept errmsg/errcode
// to return our tracked error state when we've set it.

static const char* my_sqlite3_errmsg(sqlite3 *db) {
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
    return sqlite3_errmsg(db);
}

static int my_sqlite3_errcode(sqlite3 *db) {
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
    return sqlite3_errcode(db);
}

static int my_sqlite3_extended_errcode(sqlite3 *db) {
    // For extended error codes, we just use the basic error code
    // since we don't track extended codes
    pg_connection_t *pg_conn = pg_find_connection(db);
    if (pg_conn && pg_conn->last_error_code != SQLITE_OK) {
        return pg_conn->last_error_code;
    }
    return sqlite3_extended_errcode(db);
}

static int my_sqlite3_get_table(sqlite3 *db, const char *sql, char ***pazResult,
                                 int *pnRow, int *pnColumn, char **pzErrMsg) {
    // CRITICAL FIX: NULL check to prevent crash
    if (!sql) {
        return sqlite3_get_table(db, sql, pazResult, pnRow, pnColumn, pzErrMsg);
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

    return sqlite3_get_table(db, sql, pazResult, pnRow, pnColumn, pzErrMsg);
}

// ============================================================================
// DYLD Interpose Registrations
// ============================================================================

DYLD_INTERPOSE(my_sqlite3_open, sqlite3_open)
DYLD_INTERPOSE(my_sqlite3_open_v2, sqlite3_open_v2)
DYLD_INTERPOSE(my_sqlite3_close, sqlite3_close)
DYLD_INTERPOSE(my_sqlite3_close_v2, sqlite3_close_v2)
DYLD_INTERPOSE(my_sqlite3_exec, sqlite3_exec)
DYLD_INTERPOSE(my_sqlite3_changes, sqlite3_changes)
DYLD_INTERPOSE(my_sqlite3_changes64, sqlite3_changes64)
DYLD_INTERPOSE(my_sqlite3_last_insert_rowid, sqlite3_last_insert_rowid)
DYLD_INTERPOSE(my_sqlite3_get_table, sqlite3_get_table)

// Error handling - CRITICAL for SOCI compatibility
DYLD_INTERPOSE(my_sqlite3_errmsg, sqlite3_errmsg)
DYLD_INTERPOSE(my_sqlite3_errcode, sqlite3_errcode)
DYLD_INTERPOSE(my_sqlite3_extended_errcode, sqlite3_extended_errcode)

DYLD_INTERPOSE(my_sqlite3_prepare, sqlite3_prepare)
DYLD_INTERPOSE(my_sqlite3_prepare_v2, sqlite3_prepare_v2)
DYLD_INTERPOSE(my_sqlite3_prepare_v3, sqlite3_prepare_v3)
DYLD_INTERPOSE(my_sqlite3_prepare16_v2, sqlite3_prepare16_v2)
DYLD_INTERPOSE(my_sqlite3_bind_int, sqlite3_bind_int)
DYLD_INTERPOSE(my_sqlite3_bind_int64, sqlite3_bind_int64)
DYLD_INTERPOSE(my_sqlite3_bind_double, sqlite3_bind_double)
DYLD_INTERPOSE(my_sqlite3_bind_text, sqlite3_bind_text)
DYLD_INTERPOSE(my_sqlite3_bind_text64, sqlite3_bind_text64)
DYLD_INTERPOSE(my_sqlite3_bind_blob, sqlite3_bind_blob)
DYLD_INTERPOSE(my_sqlite3_bind_blob64, sqlite3_bind_blob64)
DYLD_INTERPOSE(my_sqlite3_bind_value, sqlite3_bind_value)
DYLD_INTERPOSE(my_sqlite3_bind_null, sqlite3_bind_null)
DYLD_INTERPOSE(my_sqlite3_step, sqlite3_step)
DYLD_INTERPOSE(my_sqlite3_reset, sqlite3_reset)
DYLD_INTERPOSE(my_sqlite3_finalize, sqlite3_finalize)
DYLD_INTERPOSE(my_sqlite3_clear_bindings, sqlite3_clear_bindings)

DYLD_INTERPOSE(my_sqlite3_column_count, sqlite3_column_count)
DYLD_INTERPOSE(my_sqlite3_column_type, sqlite3_column_type)
DYLD_INTERPOSE(my_sqlite3_column_int, sqlite3_column_int)
DYLD_INTERPOSE(my_sqlite3_column_int64, sqlite3_column_int64)
DYLD_INTERPOSE(my_sqlite3_column_double, sqlite3_column_double)
DYLD_INTERPOSE(my_sqlite3_column_text, sqlite3_column_text)
DYLD_INTERPOSE(my_sqlite3_column_blob, sqlite3_column_blob)
DYLD_INTERPOSE(my_sqlite3_column_bytes, sqlite3_column_bytes)
DYLD_INTERPOSE(my_sqlite3_column_name, sqlite3_column_name)
DYLD_INTERPOSE(my_sqlite3_column_value, sqlite3_column_value)
DYLD_INTERPOSE(my_sqlite3_value_type, sqlite3_value_type)
DYLD_INTERPOSE(my_sqlite3_value_text, sqlite3_value_text)
DYLD_INTERPOSE(my_sqlite3_value_int, sqlite3_value_int)
DYLD_INTERPOSE(my_sqlite3_value_int64, sqlite3_value_int64)
DYLD_INTERPOSE(my_sqlite3_value_double, sqlite3_value_double)
DYLD_INTERPOSE(my_sqlite3_value_bytes, sqlite3_value_bytes)
DYLD_INTERPOSE(my_sqlite3_value_blob, sqlite3_value_blob)
DYLD_INTERPOSE(my_sqlite3_data_count, sqlite3_data_count)

// ============================================================================
// Collation interpose - pretend icu_root is registered
// ============================================================================

static int my_sqlite3_create_collation(
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
    return sqlite3_create_collation(db, zName, eTextRep, pArg, xCompare);
}

static int my_sqlite3_create_collation_v2(
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
    return sqlite3_create_collation_v2(db, zName, eTextRep, pArg, xCompare, xDestroy);
}

DYLD_INTERPOSE(my_sqlite3_create_collation, sqlite3_create_collation)
DYLD_INTERPOSE(my_sqlite3_create_collation_v2, sqlite3_create_collation_v2)

// ============================================================================
// Constructor/Destructor
// ============================================================================

__attribute__((constructor))
static void shim_init(void) {
    // Write to stderr first to verify constructor runs
    fprintf(stderr, "[SHIM_INIT] Constructor starting...\n");
    fflush(stderr);

    pg_logging_init();
    LOG_INFO("=== Plex PostgreSQL Interpose Shim loaded ===");
    LOG_ERROR("SHIM_CONSTRUCTOR: Initialization complete");

    // Also write to stderr after logging init
    fprintf(stderr, "[SHIM_INIT] Logging initialized\n");
    fflush(stderr);

    // CRITICAL FIX: Load SYSTEM SQLite library to bypass our DYLD_INTERPOSE
    // We need to call the REAL sqlite3_prepare_v2 from our internal code without recursion.
    // RTLD_NEXT doesn't work reliably with DYLD_INTERPOSE, so we explicitly load
    // the system libsqlite3.dylib or Plex's bundled version.
    const char *sqlite_paths[] = {
        "/Applications/Plex Media Server.app/Contents/Frameworks/libsqlite3_orig.dylib",
        "/Applications/Plex Media Server.app/Contents/Frameworks/libsqlite3.dylib",
        "/usr/lib/libsqlite3.dylib",
        NULL
    };

    for (int i = 0; sqlite_paths[i] != NULL && sqlite_handle == NULL; i++) {
        sqlite_handle = dlopen(sqlite_paths[i], RTLD_LAZY | RTLD_LOCAL);
        if (sqlite_handle) {
            fprintf(stderr, "[SHIM_INIT] Loaded SQLite from: %s\n", sqlite_paths[i]);
            break;
        }
    }

    if (!sqlite_handle) {
        fprintf(stderr, "[SHIM_INIT] ERROR: Failed to load any SQLite library: %s\n", dlerror());
        LOG_ERROR("CRITICAL: Failed to load SQLite library - recursion prevention will not work!");
    } else {
        // Resolve ALL SQLite functions we might call from within our shim
        real_sqlite3_prepare_v2 = dlsym(sqlite_handle, "sqlite3_prepare_v2");
        real_sqlite3_errmsg = dlsym(sqlite_handle, "sqlite3_errmsg");
        real_sqlite3_errcode = dlsym(sqlite_handle, "sqlite3_errcode");

        if (!real_sqlite3_prepare_v2 || !real_sqlite3_errmsg || !real_sqlite3_errcode) {
            fprintf(stderr, "[SHIM_INIT] ERROR: Failed to resolve SQLite functions:\n");
            fprintf(stderr, "  prepare_v2: %p, errmsg: %p, errcode: %p\n",
                    (void*)real_sqlite3_prepare_v2, (void*)real_sqlite3_errmsg, (void*)real_sqlite3_errcode);
            LOG_ERROR("CRITICAL: Failed to resolve all real SQLite functions!");
        } else {
            fprintf(stderr, "[SHIM_INIT] Resolved real SQLite functions:\n");
            fprintf(stderr, "  prepare_v2: %p\n  errmsg: %p\n  errcode: %p\n",
                    (void*)real_sqlite3_prepare_v2, (void*)real_sqlite3_errmsg, (void*)real_sqlite3_errcode);
            LOG_INFO("Resolved all real SQLite functions for recursion prevention");
        }
    }

    pg_config_init();
    pg_client_init();
    pg_statement_init();
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
