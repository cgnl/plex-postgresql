/*
 * Plex PostgreSQL Interposing Shim
 *
 * Uses macOS DYLD_INTERPOSE to intercept SQLite calls and redirect to PostgreSQL.
 * This approach allows the original SQLite to handle all functions we don't override.
 *
 * Build:
 *   clang -dynamiclib -o db_interpose_pg.dylib db_interpose_pg.c sql_translator.o \
 *         -I/opt/homebrew/opt/postgresql@15/include -L/opt/homebrew/opt/postgresql@15/lib \
 *         -lpq -lsqlite3 -flat_namespace
 *
 * Usage:
 *   export DYLD_INSERT_LIBRARIES=/path/to/db_interpose_pg.dylib
 *   export PLEX_PG_HOST=localhost
 *   ... other env vars
 *   open /Applications/Plex\ Media\ Server.app
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <stdint.h>
#include <stdarg.h>
#include <sqlite3.h>
#include <libpq-fe.h>
#include <dlfcn.h>

#include "sql_translator.h"

// PostgreSQL-only mode: reads and writes go to PostgreSQL, SQLite is bypassed
#define PG_READ_ENABLED 1

// DYLD interposing structure
#define DYLD_INTERPOSE(_replacement, _original) \
    __attribute__((used)) static struct { \
        const void* replacement; \
        const void* original; \
    } _interpose_##_original __attribute__((section("__DATA,__interpose"))) = { \
        (const void*)(unsigned long)&_replacement, \
        (const void*)(unsigned long)&_original \
    };

// ============================================================================
// Configuration
// ============================================================================

#define LOG_FILE "/tmp/plex_redirect_pg.log"
#define MAX_CONNECTIONS 16
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
    int last_changes;           // Track rows affected by last write
    sqlite3_int64 last_insert_rowid;  // Track last inserted row ID
    sqlite3_int64 last_generator_metadata_id;  // Track metadata_item_id from last generator URI
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
    int is_cached;  // 1 if this is from TLS (cached stmt), uses expanded_sql

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
// Used for statements prepared through our shim
#define MAX_STATEMENTS 1024
static struct {
    sqlite3_stmt *sqlite_stmt;
    pg_stmt_t *pg_stmt;
} stmt_map[MAX_STATEMENTS];
static int stmt_count = 0;
static pthread_mutex_t stmt_mutex = PTHREAD_MUTEX_INITIALIZER;

// Thread-local storage for cached statement results
// This is needed because cached statements (prepared before our shim) can be
// executed concurrently by multiple threads, each needing its own result set
#define MAX_CACHED_STMTS_PER_THREAD 64
typedef struct {
    sqlite3_stmt *sqlite_stmt;
    pg_stmt_t *pg_stmt;
} cached_stmt_entry_t;

typedef struct {
    cached_stmt_entry_t entries[MAX_CACHED_STMTS_PER_THREAD];
    int count;
} thread_cached_stmts_t;

static pthread_key_t cached_stmts_key;
static pthread_once_t cached_stmts_key_once = PTHREAD_ONCE_INIT;

static void free_thread_cached_stmts(void *ptr) {
    thread_cached_stmts_t *tcs = (thread_cached_stmts_t *)ptr;
    if (tcs) {
        for (int i = 0; i < tcs->count; i++) {
            pg_stmt_t *pg_stmt = tcs->entries[i].pg_stmt;
            if (pg_stmt) {
                if (pg_stmt->sql) free(pg_stmt->sql);
                if (pg_stmt->pg_sql) free(pg_stmt->pg_sql);
                if (pg_stmt->result) PQclear(pg_stmt->result);
                free(pg_stmt);
            }
        }
        free(tcs);
    }
}

static void create_cached_stmts_key(void) {
    pthread_key_create(&cached_stmts_key, free_thread_cached_stmts);
}

static thread_cached_stmts_t* get_thread_cached_stmts(void) {
    pthread_once(&cached_stmts_key_once, create_cached_stmts_key);
    thread_cached_stmts_t *tcs = pthread_getspecific(cached_stmts_key);
    if (!tcs) {
        tcs = calloc(1, sizeof(thread_cached_stmts_t));
        if (tcs) {
            pthread_setspecific(cached_stmts_key, tcs);
        }
    }
    return tcs;
}

// Find a cached statement result for this thread
static pg_stmt_t* find_cached_pg_stmt(sqlite3_stmt *sqlite_stmt) {
    thread_cached_stmts_t *tcs = get_thread_cached_stmts();
    if (!tcs) return NULL;
    for (int i = 0; i < tcs->count; i++) {
        if (tcs->entries[i].sqlite_stmt == sqlite_stmt) {
            return tcs->entries[i].pg_stmt;
        }
    }
    return NULL;
}

// Register a cached statement result for this thread
static void register_cached_pg_stmt(sqlite3_stmt *sqlite_stmt, pg_stmt_t *pg_stmt) {
    thread_cached_stmts_t *tcs = get_thread_cached_stmts();
    if (!tcs) return;

    // Check if already registered (shouldn't happen, but be safe)
    for (int i = 0; i < tcs->count; i++) {
        if (tcs->entries[i].sqlite_stmt == sqlite_stmt) {
            // Replace existing
            pg_stmt_t *old = tcs->entries[i].pg_stmt;
            if (old && old != pg_stmt) {
                if (old->sql) free(old->sql);
                if (old->pg_sql) free(old->pg_sql);
                if (old->result) PQclear(old->result);
                free(old);
            }
            tcs->entries[i].pg_stmt = pg_stmt;
            return;
        }
    }

    // Add new entry
    if (tcs->count < MAX_CACHED_STMTS_PER_THREAD) {
        tcs->entries[tcs->count].sqlite_stmt = sqlite_stmt;
        tcs->entries[tcs->count].pg_stmt = pg_stmt;
        tcs->count++;
    } else {
        // Evict oldest entry (simple FIFO)
        pg_stmt_t *old = tcs->entries[0].pg_stmt;
        if (old) {
            if (old->sql) free(old->sql);
            if (old->pg_sql) free(old->pg_sql);
            if (old->result) PQclear(old->result);
            free(old);
        }
        memmove(&tcs->entries[0], &tcs->entries[1],
                (MAX_CACHED_STMTS_PER_THREAD - 1) * sizeof(cached_stmt_entry_t));
        tcs->entries[MAX_CACHED_STMTS_PER_THREAD - 1].sqlite_stmt = sqlite_stmt;
        tcs->entries[MAX_CACHED_STMTS_PER_THREAD - 1].pg_stmt = pg_stmt;
    }
}

// Clear cached statement result for this thread
static void clear_cached_pg_stmt(sqlite3_stmt *sqlite_stmt) {
    thread_cached_stmts_t *tcs = get_thread_cached_stmts();
    if (!tcs) return;

    for (int i = 0; i < tcs->count; i++) {
        if (tcs->entries[i].sqlite_stmt == sqlite_stmt) {
            pg_stmt_t *pg_stmt = tcs->entries[i].pg_stmt;
            if (pg_stmt) {
                // Just clear the result, keep the pg_stmt for reuse
                if (pg_stmt->result) {
                    PQclear(pg_stmt->result);
                    pg_stmt->result = NULL;
                }
                pg_stmt->current_row = 0;
                pg_stmt->num_rows = 0;
            }
            return;
        }
    }
}

// Remove cached statement from thread-local storage (called on finalize)
static void unregister_cached_pg_stmt(sqlite3_stmt *sqlite_stmt) {
    thread_cached_stmts_t *tcs = get_thread_cached_stmts();
    if (!tcs) return;

    for (int i = 0; i < tcs->count; i++) {
        if (tcs->entries[i].sqlite_stmt == sqlite_stmt) {
            pg_stmt_t *pg_stmt = tcs->entries[i].pg_stmt;
            if (pg_stmt) {
                if (pg_stmt->sql) free(pg_stmt->sql);
                if (pg_stmt->pg_sql) free(pg_stmt->pg_sql);
                if (pg_stmt->result) PQclear(pg_stmt->result);
                free(pg_stmt);
            }
            // Remove by moving last entry here
            tcs->entries[i] = tcs->entries[--tcs->count];
            return;
        }
    }
}

// Global metadata_id tracking - shared across all connections
static sqlite3_int64 global_generator_metadata_id = 0;

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

// SQLite-specific commands that should be SKIPPED entirely (no-op, return success)
// These are SQLite internals that don't exist in PostgreSQL and don't affect data
static const char *SQLITE_SKIP_PATTERNS[] = {
    "icu_load_collation",
    "fts3_tokenizer",    // SQLite FTS3 tokenizer setup
    "SELECT load_extension",
    "VACUUM",
    "PRAGMA",
    "REINDEX",
    "ANALYZE sqlite_",
    "ATTACH DATABASE",
    "DETACH DATABASE",
    "ROLLBACK",          // Avoid "no transaction in progress" warnings
    "SAVEPOINT",
    "RELEASE SAVEPOINT",
    "sqlite_schema",     // SQLite internal schema table
    "sqlite_master",     // SQLite internal master table (alias for sqlite_schema)
    "typeof(",           // SQLite typeof() function (used in schema migrations)
    "last_insert_rowid()", // SQLite-specific function for last inserted row ID
    NULL
};

// Check if SQL should be skipped entirely (SQLite internals, no-op for PostgreSQL)
static int should_skip_sql(const char *sql) {
    if (!sql) return 0;

    // Skip whitespace at start
    while (*sql && (*sql == ' ' || *sql == '\t' || *sql == '\n')) sql++;

    for (int i = 0; SQLITE_SKIP_PATTERNS[i]; i++) {
        if (strncasecmp(sql, SQLITE_SKIP_PATTERNS[i], strlen(SQLITE_SKIP_PATTERNS[i])) == 0) {
            return 1;
        }
        // Also check if it appears anywhere in the SQL
        if (strcasestr(sql, SQLITE_SKIP_PATTERNS[i])) {
            return 1;
        }
    }
    return 0;
}

// Log SQL translation fallback for analysis
static void log_sql_fallback(const char *original_sql, const char *translated_sql, const char *error_msg, const char *context) {
    // Log to main log
    log_message("=== SQL FALLBACK TO SQLITE ===");
    log_message("Context: %s", context);
    log_message("Original SQL: %.500s", original_sql);
    if (translated_sql) {
        log_message("Translated SQL: %.500s", translated_sql);
    }
    log_message("PostgreSQL Error: %s", error_msg);
    log_message("=== END FALLBACK ===");

    // Also log to separate fallback analysis file
    FILE *fallback_log = fopen("/tmp/plex_pg_fallbacks.log", "a");
    if (fallback_log) {
        time_t now = time(NULL);
        char timestamp[64];
        strftime(timestamp, sizeof(timestamp), "%Y-%m-%d %H:%M:%S", localtime(&now));

        fprintf(fallback_log, "\n[%s] %s\n", timestamp, context);
        fprintf(fallback_log, "ORIGINAL: %s\n", original_sql);
        if (translated_sql) {
            fprintf(fallback_log, "TRANSLATED: %s\n", translated_sql);
        }
        fprintf(fallback_log, "ERROR: %s\n", error_msg);
        fprintf(fallback_log, "---\n");
        fclose(fallback_log);
    }
}

// Check if a PostgreSQL error is a known translation limitation
static int is_known_translation_limitation(const char *error_msg) {
    if (!error_msg) return 0;

    // Known translation limitations (but we want to log these for improvement)
    if (strstr(error_msg, "operator does not exist: integer = json")) return 1;
    if (strstr(error_msg, "must appear in the GROUP BY clause")) return 1;
    if (strstr(error_msg, "syntax error")) return 1;
    if (strstr(error_msg, "no unique or exclusion constraint matching the ON CONFLICT")) return 1;

    return 0;
}

static pg_connection_t* find_pg_connection(sqlite3 *db) {
    pthread_mutex_lock(&conn_mutex);
    for (int i = 0; i < connection_count; i++) {
        if (connections[i] && connections[i]->shadow_db == db) {
            pthread_mutex_unlock(&conn_mutex);
            return connections[i];
        }
    }
    // Debug: log if we have connections but couldn't find a match
    if (connection_count > 0) {
        // Don't log for every call - only when we might have expected a match
        // This is done in the caller now
    }
    pthread_mutex_unlock(&conn_mutex);
    return NULL;
}

// Find connection by checking if db path matches (fallback for cached stmts)
static pg_connection_t* find_any_pg_connection_for_library(void) {
    pthread_mutex_lock(&conn_mutex);
    for (int i = 0; i < connection_count; i++) {
        if (connections[i] && connections[i]->is_pg_active &&
            strstr(connections[i]->db_path, "com.plexapp.plugins.library.db")) {
            pthread_mutex_unlock(&conn_mutex);
            return connections[i];
        }
    }
    pthread_mutex_unlock(&conn_mutex);
    return NULL;
}

// Check if SQL is a write operation (INSERT, UPDATE, DELETE)
static int is_write_operation(const char *sql) {
    if (!sql) return 0;

    // Skip whitespace
    while (*sql && (*sql == ' ' || *sql == '\t' || *sql == '\n')) sql++;

    // Check for write keywords (case insensitive)
    if (strncasecmp(sql, "INSERT", 6) == 0) return 1;
    if (strncasecmp(sql, "UPDATE", 6) == 0) return 1;
    if (strncasecmp(sql, "DELETE", 6) == 0) return 1;
    if (strncasecmp(sql, "REPLACE", 7) == 0) return 1;

    return 0;
}

// Check if SQL is a read operation (SELECT)
static int is_read_operation(const char *sql) {
    if (!sql) return 0;

    // Skip whitespace
    while (*sql && (*sql == ' ' || *sql == '\t' || *sql == '\n')) sql++;

    if (strncasecmp(sql, "SELECT", 6) == 0) return 1;

    return 0;
}

// Simple string replace for fixing IN clauses
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

// Extract metadata_item_id from play_queue_generators URI
// URI format: library://x/item/%2Flibrary%2Fmetadata%2F<ID>
static sqlite3_int64 extract_metadata_id_from_generator_sql(const char *sql) {
    if (!sql) return 0;

    // Look for play_queue_generators INSERT with URI
    if (!strcasestr(sql, "play_queue_generators")) return 0;
    if (!strcasestr(sql, "INSERT")) return 0;

    // Find the URI pattern: %2Fmetadata%2F followed by digits
    const char *pattern = "%2Fmetadata%2F";
    const char *pos = strstr(sql, pattern);
    if (!pos) {
        // Also try URL-decoded version
        pattern = "/metadata/";
        pos = strstr(sql, pattern);
    }
    if (!pos) return 0;

    // Move past the pattern
    pos += strlen(pattern);

    // Extract the numeric ID
    sqlite3_int64 id = 0;
    while (*pos >= '0' && *pos <= '9') {
        id = id * 10 + (*pos - '0');
        pos++;
    }

    return id;
}

// Convert INSERT INTO metadata_item_settings to upsert with ON CONFLICT
// Also changes view_count from 0 to 1 for marking as watched
// Returns malloc'd string that caller must free, or NULL if not applicable
static char* convert_metadata_settings_insert_to_upsert(const char *sql) {
    if (!sql) return NULL;

    // Check if this is an INSERT INTO metadata_item_settings
    const char *table_name = "metadata_item_settings";

    // Find INSERT INTO
    if (!strcasestr(sql, "INSERT INTO")) return NULL;

    // Check if it's metadata_item_settings
    if (!strcasestr(sql, table_name)) return NULL;

    // Check if already has ON CONFLICT or RETURNING
    if (strcasestr(sql, "ON CONFLICT")) return NULL;
    if (strcasestr(sql, "RETURNING")) return NULL;

    // ON CONFLICT clause to handle duplicates
    // view_count logic:
    // - If existing view_count > 0 and EXCLUDED is 0: this is "mark as unwatched" → use 0
    // - Otherwise: use at least 1 (mark as watched)
    // Note: We DON'T modify VALUES, so EXCLUDED.view_count = what Plex sent (0)
    const char *on_conflict =
        " ON CONFLICT (account_id, guid) DO UPDATE SET "
        "rating = COALESCE(EXCLUDED.rating, plex.metadata_item_settings.rating), "
        "view_offset = EXCLUDED.view_offset, "
        "view_count = CASE WHEN plex.metadata_item_settings.view_count > 0 AND EXCLUDED.view_count = 0 "
                     "THEN 0 ELSE GREATEST(EXCLUDED.view_count, plex.metadata_item_settings.view_count, 1) END, "
        "last_viewed_at = CASE WHEN plex.metadata_item_settings.view_count > 0 AND EXCLUDED.view_count = 0 "
                         "THEN NULL ELSE COALESCE(EXCLUDED.last_viewed_at, EXTRACT(EPOCH FROM NOW())::bigint) END, "
        "updated_at = COALESCE(EXCLUDED.updated_at, EXTRACT(EPOCH FROM NOW())::bigint), "
        "skip_count = EXCLUDED.skip_count, "
        "last_skipped_at = EXCLUDED.last_skipped_at, "
        "changed_at = COALESCE(EXCLUDED.changed_at, EXTRACT(EPOCH FROM NOW())::bigint), "
        "extra_data = COALESCE(EXCLUDED.extra_data, plex.metadata_item_settings.extra_data), "
        "last_rated_at = COALESCE(EXCLUDED.last_rated_at, plex.metadata_item_settings.last_rated_at) "
        "RETURNING id";

    // Calculate size and build result
    size_t sql_len = strlen(sql);
    size_t total_len = sql_len + strlen(on_conflict) + 1;

    char *result = malloc(total_len);
    if (!result) return NULL;

    // DON'T modify VALUES - let Plex's view_count=0 pass through
    // ON CONFLICT will handle the logic based on existing row state
    snprintf(result, total_len, "%s%s", sql, on_conflict);

    return result;
}

// Statement tracking functions
static void register_stmt(sqlite3_stmt *sqlite_stmt, pg_stmt_t *pg_stmt) {
    pthread_mutex_lock(&stmt_mutex);
    if (stmt_count < MAX_STATEMENTS) {
        stmt_map[stmt_count].sqlite_stmt = sqlite_stmt;
        stmt_map[stmt_count].pg_stmt = pg_stmt;
        stmt_count++;
    }
    pthread_mutex_unlock(&stmt_mutex);
}

// Find pg_stmt in global stmt_map ONLY (for statements prepared through our shim)
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

// Find pg_stmt in BOTH stmt_map and TLS (for column functions that need to read data)
static pg_stmt_t* find_any_pg_stmt(sqlite3_stmt *sqlite_stmt) {
    // First check stmt_map
    pg_stmt_t *result = find_pg_stmt(sqlite_stmt);
    if (result) return result;

    // Fallback: check thread-local storage (for cached statements)
    return find_cached_pg_stmt(sqlite_stmt);
}

static void unregister_stmt(sqlite3_stmt *sqlite_stmt) {
    pthread_mutex_lock(&stmt_mutex);
    for (int i = 0; i < stmt_count; i++) {
        if (stmt_map[i].sqlite_stmt == sqlite_stmt) {
            // Free pg_stmt resources
            pg_stmt_t *pg_stmt = stmt_map[i].pg_stmt;
            if (pg_stmt) {
                if (pg_stmt->sql) free(pg_stmt->sql);
                if (pg_stmt->pg_sql) free(pg_stmt->pg_sql);
                if (pg_stmt->result) PQclear(pg_stmt->result);
                for (int j = 0; j < pg_stmt->param_count; j++) {
                    if (pg_stmt->param_values[j]) free(pg_stmt->param_values[j]);
                }
                free(pg_stmt);
            }
            // Remove from map by moving last entry here
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

    char conninfo[1024];
    snprintf(conninfo, sizeof(conninfo),
             "host=%s port=%d dbname=%s user=%s password=%s "
             "connect_timeout=10 application_name=plex_shim",
             pg_config.host, pg_config.port, pg_config.database,
             pg_config.user, pg_config.password);

    PGconn *conn = PQconnectdb(conninfo);

    if (PQstatus(conn) != CONNECTION_OK) {
        log_message("PostgreSQL connection failed: %s", PQerrorMessage(conn));
        PQfinish(conn);
        return NULL;
    }

    // Set search path
    char schema_sql[256];
    snprintf(schema_sql, sizeof(schema_sql),
             "SET search_path TO %s, public", pg_config.schema);
    PGresult *res = PQexec(conn, schema_sql);
    PQclear(res);

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

// Renamed versions that call the originals
static int my_sqlite3_open(const char *filename, sqlite3 **ppDb) {
    log_message("OPEN: %s (redirect=%d)", filename ? filename : "(null)", should_redirect(filename));

    // Always call real SQLite first
    int rc = sqlite3_open(filename, ppDb);

    if (rc == SQLITE_OK && should_redirect(filename)) {
        // Also connect to PostgreSQL
        pg_connection_t *pg_conn = pg_connect(filename, *ppDb);
        if (pg_conn) {
            log_message("PostgreSQL shadow connection established for: %s", filename);
        } else {
            log_message("PostgreSQL connection failed, using SQLite only");
        }
    }

    return rc;
}

static int my_sqlite3_open_v2(const char *filename, sqlite3 **ppDb, int flags, const char *zVfs) {
    log_message("OPEN_V2: %s flags=0x%x (redirect=%d)",
                filename ? filename : "(null)", flags, should_redirect(filename));

    // Always call real SQLite first
    int rc = sqlite3_open_v2(filename, ppDb, flags, zVfs);

    if (rc == SQLITE_OK && should_redirect(filename)) {
        pg_connection_t *pg_conn = pg_connect(filename, *ppDb);
        if (pg_conn) {
            log_message("PostgreSQL shadow connection established for: %s", filename);
        }
    }

    return rc;
}

static int my_sqlite3_close(sqlite3 *db) {
    pg_connection_t *pg_conn = find_pg_connection(db);
    if (pg_conn) {
        log_message("CLOSE: PostgreSQL connection for %s", pg_conn->db_path);

        // Close PostgreSQL connection
        if (pg_conn->conn) {
            PQfinish(pg_conn->conn);
            pg_conn->conn = NULL;
        }

        // Remove from list
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
    }

    return sqlite3_close(db);
}

static int my_sqlite3_close_v2(sqlite3 *db) {
    pg_connection_t *pg_conn = find_pg_connection(db);
    if (pg_conn) {
        if (pg_conn->conn) {
            PQfinish(pg_conn->conn);
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
    }

    return sqlite3_close_v2(db);
}

static int my_sqlite3_exec(sqlite3 *db, const char *sql,
                          int (*callback)(void*, int, char**, char**),
                          void *arg, char **errmsg) {
    // Debug: log all exec calls
    if (!should_skip_sql(sql)) {
        log_message("EXEC: %.300s", sql);
    }
    // Log all play_queue related queries
    if (strstr(sql, "play_queue")) {
        log_message("EXEC (play_queue): %.500s", sql);
    }

    pg_connection_t *pg_conn = find_pg_connection(db);

    if (pg_conn && pg_conn->conn && pg_conn->is_pg_active) {
        // Skip SQLite-only commands
        if (should_skip_sql(sql)) {
            // Just log and skip PostgreSQL execution
            // log_message("SKIP SQLite-only: %.50s...", sql);
        } else {
            // Execute on both PostgreSQL and SQLite (dual-write)
            sql_translation_t trans = sql_translate(sql);
            if (trans.success && trans.sql) {
                log_message("EXEC PG: %s", trans.sql);
                PGresult *res = PQexec(pg_conn->conn, trans.sql);
                ExecStatusType status = PQresultStatus(res);
                if (status != PGRES_COMMAND_OK && status != PGRES_TUPLES_OK) {
                    log_message("PostgreSQL exec error: %s", PQerrorMessage(pg_conn->conn));
                }
                PQclear(res);
            }
            sql_translation_free(&trans);
        }

        // PostgreSQL-only mode: skip SQLite for redirected databases
        return SQLITE_OK;
    }

    // Execute on SQLite for non-redirected DBs only
    return sqlite3_exec(db, sql, callback, arg, errmsg);
}

// ============================================================================
// Prepared Statement Interception
// ============================================================================

static int my_sqlite3_prepare_v2(sqlite3 *db, const char *zSql, int nByte,
                                  sqlite3_stmt **ppStmt, const char **pzTail) {
    // Check if this is a redirected database
    pg_connection_t *pg_conn = find_pg_connection(db);
    int is_write = is_write_operation(zSql);
    int is_read = is_read_operation(zSql);

    // Debug: log ALL SQL to catch anything we might miss
    if (is_write) {
        log_message("PREPARE_V2 WRITE: %.300s", zSql);
    }
    if (is_read) {
        // Log reads, especially those with empty IN clauses or play_queue tables
        if (strstr(zSql, " in ()") || strstr(zSql, " IN ()")) {
            log_message("PREPARE_V2 READ (EMPTY IN!): %.500s", zSql);
        } else if (strstr(zSql, "metadata_items.id in")) {
            log_message("PREPARE_V2 READ (has IN): %.500s", zSql);
        }
        // Always log play_queue queries
        if (strstr(zSql, "play_queue") || strstr(zSql, "PLAY_QUEUE")) {
            log_message("PREPARE_V2 READ (play_queue): %.500s", zSql);
        }
    }

    // For redirected databases in PostgreSQL-only mode, clean up SQL for SQLite
    // Only remove problematic things like "collate icu_root", NOT full PG translation
    char *cleaned_sql = NULL;
    const char *sql_for_sqlite = zSql;

    // Clean "collate icu_root" for SQLite (it doesn't have ICU loaded)
    if (pg_conn && pg_conn->conn && pg_conn->is_pg_active && (is_write || is_read)) {
        if (!should_skip_sql(zSql) && strcasestr(zSql, "collate icu_root")) {
            size_t len = strlen(zSql);
            cleaned_sql = malloc(len + 1);
            if (cleaned_sql) {
                strcpy(cleaned_sql, zSql);
                char *pos;
                while ((pos = strcasestr(cleaned_sql, " collate icu_root")) != NULL) {
                    memmove(pos, pos + 17, strlen(pos + 17) + 1);
                }
                sql_for_sqlite = cleaned_sql;
            }
        }
    }

    // Prepare on SQLite (with cleaned SQL if needed)
    int rc = sqlite3_prepare_v2(db, sql_for_sqlite,
                                cleaned_sql ? -1 : nByte,
                                ppStmt, pzTail);

    if (rc != SQLITE_OK || !*ppStmt) {
        if (cleaned_sql) free(cleaned_sql);
        return rc;
    }

    if (pg_conn && pg_conn->conn && pg_conn->is_pg_active && (is_write || is_read)) {
        // Create pg_stmt_t to track this statement
        pg_stmt_t *pg_stmt = calloc(1, sizeof(pg_stmt_t));
        if (pg_stmt) {
            pg_stmt->conn = pg_conn;
            pg_stmt->sql = strdup(zSql);

            // Check if this should be skipped (SQLite internals)
            if (should_skip_sql(zSql)) {
                pg_stmt->is_pg = 3;  // 3 = skip (no-op)
                log_message("SKIP (no-op): %.200s", zSql);
            } else {
                pg_stmt->is_pg = is_write ? 1 : 2;  // 1 = write, 2 = read

                // Translate SQL for PostgreSQL (full translation)
                sql_translation_t trans = sql_translate(zSql);
                if (trans.success && trans.sql) {
                    // Check if this has empty IN () and we have a saved metadata_id
                    if ((strstr(trans.sql, "IN (NULL)") || strstr(trans.sql, "in (NULL)")) &&
                        global_generator_metadata_id > 0 &&
                        strstr(trans.sql, "metadata_items")) {
                        // Substitute the saved metadata_id for IN (NULL)
                        char id_str[32];
                        snprintf(id_str, sizeof(id_str), "IN (%lld)", global_generator_metadata_id);
                        char *fixed = simple_str_replace(trans.sql, "IN (NULL)", id_str);
                        if (!fixed) fixed = simple_str_replace(trans.sql, "in (NULL)", id_str);
                        if (fixed) {
                            pg_stmt->pg_sql = fixed;
                            log_message("PREPARE: Fixed empty IN with metadata_id=%lld (global)", global_generator_metadata_id);
                        } else {
                            pg_stmt->pg_sql = strdup(trans.sql);
                        }
                    } else {
                        pg_stmt->pg_sql = strdup(trans.sql);
                    }
                }
                sql_translation_free(&trans);
            }

            // Count parameters (number of ? in SQL)
            const char *p = zSql;
            pg_stmt->param_count = 0;
            while (*p) {
                if (*p == '?') pg_stmt->param_count++;
                p++;
            }

            // Register the statement mapping
            register_stmt(*ppStmt, pg_stmt);

            if (is_write) {
                log_message("PREPARE WRITE: %.200s", zSql);
            }
        }
    }

    if (cleaned_sql) free(cleaned_sql);
    return rc;
}

// Also intercept sqlite3_prepare (the older API without _v2)
// Note: We call sqlite3_prepare_v2 internally which will be intercepted
static int my_sqlite3_prepare(sqlite3 *db, const char *zSql, int nByte,
                              sqlite3_stmt **ppStmt, const char **pzTail) {
    // Log to see if this path is used
    if (is_write_operation(zSql)) {
        log_message("PREPARE (non-v2) WRITE: %.300s", zSql);
    }
    // Use sqlite3_prepare_v2 which will go through our interpose
    // The _v2 version has better error handling anyway
    return sqlite3_prepare_v2(db, zSql, nByte, ppStmt, pzTail);
}

// Helper to convert UTF-16 to UTF-8 for logging
static char* utf16_to_utf8(const void *zSql16) {
    if (!zSql16) return NULL;
    // Count UTF-16 length
    const uint16_t *p = (const uint16_t*)zSql16;
    size_t len = 0;
    while (p[len]) len++;
    if (len == 0) return strdup("");

    // Simple conversion (ASCII only for logging)
    char *utf8 = malloc(len + 1);
    if (!utf8) return NULL;
    for (size_t i = 0; i < len; i++) {
        utf8[i] = (p[i] < 128) ? (char)p[i] : '?';
    }
    utf8[len] = '\0';
    return utf8;
}

// Intercept sqlite3_prepare16_v2 (UTF-16 version)
static int my_sqlite3_prepare16_v2(sqlite3 *db, const void *zSql, int nByte,
                                    sqlite3_stmt **ppStmt, const void **pzTail) {
    // Convert to UTF-8 for logging and checking
    char *utf8_sql = utf16_to_utf8(zSql);
    if (utf8_sql && is_write_operation(utf8_sql)) {
        log_message("PREPARE16_V2 WRITE: %.300s", utf8_sql);
    }
    if (utf8_sql) free(utf8_sql);

    // Call the original - we don't redirect UTF-16 queries for now, just log them
    return sqlite3_prepare16_v2(db, zSql, nByte, ppStmt, pzTail);
}

static int my_sqlite3_bind_int(sqlite3_stmt *pStmt, int idx, int val) {
    // Always bind on SQLite
    int rc = sqlite3_bind_int(pStmt, idx, val);

    // Also track for PostgreSQL
    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS) {
        char buf[32];
        snprintf(buf, sizeof(buf), "%d", val);
        if (pg_stmt->param_values[idx-1]) free(pg_stmt->param_values[idx-1]);
        pg_stmt->param_values[idx-1] = strdup(buf);
        pg_stmt->param_lengths[idx-1] = 0;  // text format
        pg_stmt->param_formats[idx-1] = 0;  // text format
        if (idx > pg_stmt->param_count) pg_stmt->param_count = idx;
    }

    return rc;
}

static int my_sqlite3_bind_int64(sqlite3_stmt *pStmt, int idx, sqlite3_int64 val) {
    int rc = sqlite3_bind_int64(pStmt, idx, val);

    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS) {
        char buf[32];
        snprintf(buf, sizeof(buf), "%lld", val);
        if (pg_stmt->param_values[idx-1]) free(pg_stmt->param_values[idx-1]);
        pg_stmt->param_values[idx-1] = strdup(buf);
        pg_stmt->param_lengths[idx-1] = 0;
        pg_stmt->param_formats[idx-1] = 0;
        if (idx > pg_stmt->param_count) pg_stmt->param_count = idx;
    }

    return rc;
}

static int my_sqlite3_bind_double(sqlite3_stmt *pStmt, int idx, double val) {
    int rc = sqlite3_bind_double(pStmt, idx, val);

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

static int my_sqlite3_bind_text(sqlite3_stmt *pStmt, int idx, const char *val,
                                 int nBytes, void (*destructor)(void*)) {
    int rc = sqlite3_bind_text(pStmt, idx, val, nBytes, destructor);

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

static int my_sqlite3_bind_blob(sqlite3_stmt *pStmt, int idx, const void *val,
                                 int nBytes, void (*destructor)(void*)) {
    int rc = sqlite3_bind_blob(pStmt, idx, val, nBytes, destructor);

    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS && val && nBytes > 0) {
        if (pg_stmt->param_values[idx-1]) free(pg_stmt->param_values[idx-1]);
        pg_stmt->param_values[idx-1] = malloc(nBytes);
        if (pg_stmt->param_values[idx-1]) {
            memcpy(pg_stmt->param_values[idx-1], val, nBytes);
        }
        pg_stmt->param_lengths[idx-1] = nBytes;
        pg_stmt->param_formats[idx-1] = 1;  // binary format
        if (idx > pg_stmt->param_count) pg_stmt->param_count = idx;
    }

    return rc;
}

static int my_sqlite3_bind_null(sqlite3_stmt *pStmt, int idx) {
    int rc = sqlite3_bind_null(pStmt, idx);

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

static int my_sqlite3_step(sqlite3_stmt *pStmt) {
    pg_stmt_t *pg_stmt = find_pg_stmt(pStmt);

    // Debug: log all step calls
    const char *step_sql = sqlite3_sql(pStmt);
    if (step_sql && !strstr(step_sql, "PRAGMA") && !strstr(step_sql, "fts3_tokenizer")) {
        sqlite3 *db_dbg = sqlite3_db_handle(pStmt);
        pg_connection_t *pg_dbg = find_pg_connection(db_dbg);
        log_message("STEP called: pg_stmt=%s, pg_conn=%s, sql=%.100s",
                    pg_stmt ? "yes" : "no",
                    (pg_dbg && pg_dbg->is_pg_active) ? "active" : "none",
                    step_sql);
    }

    // For skipped statements (is_pg == 3), just return SQLITE_DONE
    if (pg_stmt && pg_stmt->is_pg == 3) {
        return SQLITE_DONE;
    }

    // Handle statements that weren't prepared through our shim (e.g., cached at startup)
    if (!pg_stmt) {
        sqlite3 *db = sqlite3_db_handle(pStmt);
        const char *sql_check = sqlite3_sql(pStmt);

        pg_connection_t *pg_conn = find_pg_connection(db);

        // Fallback: if db doesn't match, try to find any active library connection
        if (!pg_conn && sql_check) {
            pg_conn = find_any_pg_connection_for_library();
        }

        if (pg_conn && pg_conn->is_pg_active && pg_conn->conn) {
            // Get the SQL - try expanded first (includes bound values)
            char *expanded_sql = sqlite3_expanded_sql(pStmt);
            const char *sql = expanded_sql ? expanded_sql : sqlite3_sql(pStmt);

            if (sql && is_write_operation(sql) && !should_skip_sql(sql)) {

                // Translate and execute on PostgreSQL
                sql_translation_t trans = sql_translate(sql);
                if (trans.success && trans.sql) {
                    // For INSERTs, handle upsert for metadata_item_settings or add RETURNING id
                    char *exec_sql = trans.sql;
                    char *insert_sql = NULL;
                    if (strncasecmp(sql, "INSERT", 6) == 0) {
                        // First try to convert to upsert for metadata_item_settings
                        insert_sql = convert_metadata_settings_insert_to_upsert(trans.sql);
                        if (insert_sql) {
                            exec_sql = insert_sql;
                            log_message("STEP CACHED INSERT converted to UPSERT: %.300s", exec_sql);
                        } else if (!strstr(trans.sql, "RETURNING")) {
                            // Add RETURNING id if not already present
                            size_t len = strlen(trans.sql);
                            insert_sql = malloc(len + 20);
                            if (insert_sql) {
                                snprintf(insert_sql, len + 20, "%s RETURNING id", trans.sql);
                                exec_sql = insert_sql;
                            }
                        }
                    }

                    PGresult *res = PQexec(pg_conn->conn, exec_sql);
                    ExecStatusType status = PQresultStatus(res);
                    sqlite3_int64 this_insert_id = 0;  // Track ID from THIS specific insert

                    if (status != PGRES_COMMAND_OK && status != PGRES_TUPLES_OK) {
                        const char *pg_error = PQerrorMessage(pg_conn->conn);

                        // Log all fallbacks for analysis and improvement
                        if (is_known_translation_limitation(pg_error)) {
                            log_sql_fallback(sql, exec_sql, pg_error, "CACHED WRITE");
                        } else {
                            // Unexpected error - log with more detail
                            log_message("STEP CACHED WRITE PG error: %s\nSQL: %s", pg_error, exec_sql);
                            log_sql_fallback(sql, exec_sql, pg_error, "CACHED WRITE (UNEXPECTED)");
                        }
                        pg_conn->last_changes = 0;
                    } else {
                        const char *cmd_tuples = PQcmdTuples(res);
                        int affected = cmd_tuples && cmd_tuples[0] ? atoi(cmd_tuples) : 1;
                        pg_conn->last_changes = affected;

                        // For INSERTs with RETURNING, get the new id
                        if (strncasecmp(sql, "INSERT", 6) == 0 &&
                            status == PGRES_TUPLES_OK && PQntuples(res) > 0) {
                            const char *id_str = PQgetvalue(res, 0, 0);
                            if (id_str && *id_str) {
                                this_insert_id = atoll(id_str);
                                pg_conn->last_insert_rowid = this_insert_id;
                                log_message("STEP CACHED INSERT OK (id=%lld): %.200s",
                                            this_insert_id, trans.sql);

                                // For play_queue_generators, extract metadata_id from URI (save globally for cross-connection access)
                                sqlite3_int64 meta_id = extract_metadata_id_from_generator_sql(sql);
                                if (meta_id > 0) {
                                    global_generator_metadata_id = meta_id;
                                    log_message("  -> Extracted metadata_id=%lld from generator URI (global)", meta_id);
                                }
                            }
                        } else {
                            log_message("STEP CACHED WRITE OK (%d rows): %.200s", affected, trans.sql);
                        }
                    }

                    if (insert_sql) free(insert_sql);
                    PQclear(res);

                    // PostgreSQL-only mode
                    if (expanded_sql) sqlite3_free(expanded_sql);
                    sql_translation_free(&trans);
                    return SQLITE_DONE;
                }  // close: if (trans.success && trans.sql)
            }  // close: if (sql && is_write_operation(sql))

            // Handle cached READ statements - execute on SQLite but fetch PostgreSQL data for columns
            // This approach: let SQLite update its internal state, but use PostgreSQL data for column values
            if (sql && is_read_operation(sql) && !should_skip_sql(sql)) {
                // Check if we already have a result for this statement in this thread
                pg_stmt_t *cached_pg_stmt = find_cached_pg_stmt(pStmt);

                // First, let SQLite step to update its internal state
                int sqlite_result = sqlite3_step(pStmt);

                // Handle PostgreSQL data fetching
                if (sqlite_result == SQLITE_ROW || sqlite_result == SQLITE_DONE) {
                    if (cached_pg_stmt && cached_pg_stmt->result) {
                        // We already have PostgreSQL data - just advance current_row
                        if (sqlite_result == SQLITE_ROW) {
                            cached_pg_stmt->current_row++;
                        }
                        // log_message("STEP CACHED READ: advancing row to %d for: %.100s", cached_pg_stmt->current_row, sql);
                    } else {
                        // First step or no result yet - need to fetch from PostgreSQL
                        sql_translation_t trans = sql_translate(sql);
                        if (trans.success && trans.sql) {
                            pg_stmt_t *new_pg_stmt = cached_pg_stmt;
                            if (!new_pg_stmt) {
                                new_pg_stmt = calloc(1, sizeof(pg_stmt_t));
                                if (new_pg_stmt) {
                                    new_pg_stmt->conn = pg_conn;
                                    new_pg_stmt->sql = strdup(sql);
                                    new_pg_stmt->pg_sql = strdup(trans.sql);
                                    new_pg_stmt->is_pg = 2;  // read
                                    new_pg_stmt->is_cached = 1;  // mark as cached statement
                                    new_pg_stmt->param_count = 0;
                                    register_cached_pg_stmt(pStmt, new_pg_stmt);
                                }
                            }

                            if (new_pg_stmt) {
                                // Execute on PostgreSQL to get current data
                                new_pg_stmt->result = PQexec(pg_conn->conn, trans.sql);
                                ExecStatusType status = PQresultStatus(new_pg_stmt->result);

                                if (status == PGRES_TUPLES_OK) {
                                    new_pg_stmt->num_rows = PQntuples(new_pg_stmt->result);
                                    new_pg_stmt->num_cols = PQnfields(new_pg_stmt->result);
                                    new_pg_stmt->current_row = 0;

                                    log_message("STEP CACHED READ: sqlite=%d, pg_rows=%d for: %.200s",
                                                sqlite_result, new_pg_stmt->num_rows, trans.sql);
                                } else {
                                    const char *pg_error = PQerrorMessage(pg_conn->conn);

                                    // Log all fallbacks for analysis and improvement
                                    if (is_known_translation_limitation(pg_error)) {
                                        log_sql_fallback(sql, trans.sql, pg_error, "CACHED READ");
                                    } else {
                                        // Unexpected error - log with more detail
                                        log_message("STEP CACHED READ PG error: %s", pg_error);
                                        log_sql_fallback(sql, trans.sql, pg_error, "CACHED READ (UNEXPECTED)");
                                    }
                                    PQclear(new_pg_stmt->result);
                                    new_pg_stmt->result = NULL;
                                }
                            }
                        }
                        sql_translation_free(&trans);
                    }
                }

                if (expanded_sql) sqlite3_free(expanded_sql);
                // Return SQLite's result to maintain internal state consistency
                return sqlite_result;
            }

            if (expanded_sql) sqlite3_free(expanded_sql);
        }
    }

    // Execute on PostgreSQL
    if (pg_stmt && pg_stmt->pg_sql && pg_stmt->conn && pg_stmt->conn->conn) {
        // For reads (is_pg == 2), only execute once and store result
        if (pg_stmt->is_pg == 2) {
            // First call - execute query
            if (!pg_stmt->result) {
                const char *paramValues[MAX_PARAMS];
                int paramLengths[MAX_PARAMS];
                int paramFormats[MAX_PARAMS];

                for (int i = 0; i < pg_stmt->param_count && i < MAX_PARAMS; i++) {
                    paramValues[i] = pg_stmt->param_values[i];
                    paramLengths[i] = pg_stmt->param_lengths[i];
                    paramFormats[i] = pg_stmt->param_formats[i];
                }

                // Debug: log param count and first few param values
                log_message("STEP READ executing: param_count=%d", pg_stmt->param_count);
                for (int dbg = 0; dbg < pg_stmt->param_count && dbg < 4; dbg++) {
                    log_message("  param[%d] = %s", dbg, paramValues[dbg] ? paramValues[dbg] : "(NULL)");
                }

                pg_stmt->result = PQexecParams(
                    pg_stmt->conn->conn,
                    pg_stmt->pg_sql,
                    pg_stmt->param_count,
                    NULL,
                    paramValues,
                    paramLengths,
                    paramFormats,
                    0  // text format
                );

                ExecStatusType status = PQresultStatus(pg_stmt->result);
                if (status == PGRES_TUPLES_OK) {
                    pg_stmt->num_rows = PQntuples(pg_stmt->result);
                    pg_stmt->num_cols = PQnfields(pg_stmt->result);
                    pg_stmt->current_row = 0;
                    log_message("STEP READ: %d rows, %d cols for: %.500s",
                                pg_stmt->num_rows, pg_stmt->num_cols, pg_stmt->pg_sql);
                    // For the 173-col playback query that returns 0 rows, log more
                    if (pg_stmt->num_rows == 0 && pg_stmt->num_cols > 100) {
                        // Log the WHERE clause part of the SQL
                        const char *where = strstr(pg_stmt->pg_sql, "where ");
                        if (where) {
                            log_message("  WHERE clause: %.1000s", where);
                        }
                    }
                    
                } else if (status != PGRES_COMMAND_OK) {
                    const char *pg_error = PQerrorMessage(pg_stmt->conn->conn);

                    // Log all fallbacks for analysis and improvement
                    if (is_known_translation_limitation(pg_error)) {
                        log_sql_fallback(pg_stmt->sql, pg_stmt->pg_sql, pg_error, "PREPARED READ");
                    } else {
                        // Unexpected error - log with more detail
                        log_message("STEP PG read error: %s\nSQL: %s", pg_error, pg_stmt->pg_sql);
                        log_sql_fallback(pg_stmt->sql, pg_stmt->pg_sql, pg_error, "PREPARED READ (UNEXPECTED)");
                    }
                    PQclear(pg_stmt->result);
                    pg_stmt->result = NULL;
                }
            } else {
                // Subsequent calls - advance to next row
                pg_stmt->current_row++;
            }

#if PG_READ_ENABLED
            // When reads are enabled, return based on PostgreSQL state
            if (pg_stmt->result) {
                if (pg_stmt->current_row < pg_stmt->num_rows) {
                    return SQLITE_ROW;
                } else {
                    return SQLITE_DONE;
                }
            }
            // If result is NULL (error), fall through to SQLite
#endif
        }
        // For writes (is_pg == 1), execute every time
        else if (pg_stmt->is_pg == 1) {
            const char *paramValues[MAX_PARAMS];
            int paramLengths[MAX_PARAMS];
            int paramFormats[MAX_PARAMS];

            for (int i = 0; i < pg_stmt->param_count && i < MAX_PARAMS; i++) {
                paramValues[i] = pg_stmt->param_values[i];
                paramLengths[i] = pg_stmt->param_lengths[i];
                paramFormats[i] = pg_stmt->param_formats[i];
            }

            PGresult *res = PQexecParams(
                pg_stmt->conn->conn,
                pg_stmt->pg_sql,
                pg_stmt->param_count,
                NULL,
                paramValues,
                paramLengths,
                paramFormats,
                0
            );

            ExecStatusType status = PQresultStatus(res);
            if (status != PGRES_COMMAND_OK && status != PGRES_TUPLES_OK) {
                log_message("STEP PG write error: %s\nSQL: %s",
                            PQerrorMessage(pg_stmt->conn->conn), pg_stmt->pg_sql);
                pg_stmt->conn->last_changes = 0;
            } else {
                // Get number of affected rows
                const char *cmd_tuples = PQcmdTuples(res);
                int affected = cmd_tuples && cmd_tuples[0] ? atoi(cmd_tuples) : 1;
                pg_stmt->conn->last_changes = affected;

                // For INSERT, try to get the OID (last insert rowid)
                Oid oid = PQoidValue(res);
                if (oid != InvalidOid) {
                    pg_stmt->conn->last_insert_rowid = (sqlite3_int64)oid;
                }

                log_message("STEP WRITE OK (%d rows): %.200s", affected, pg_stmt->pg_sql);
            }

            PQclear(res);
        }
    }

    // PostgreSQL-only mode: don't execute on SQLite for redirected databases
    if (pg_stmt && pg_stmt->is_pg) {
        // For writes, return success (already executed on PostgreSQL)
        if (pg_stmt->is_pg == 1) {
            return SQLITE_DONE;
        }
        // For reads that failed on PG, fall through to SQLite as backup
    }

    // Execute on SQLite for non-redirected DBs only
    // Debug: log if we're falling through to SQLite for a redirected database
    sqlite3 *db_check = sqlite3_db_handle(pStmt);
    pg_connection_t *pg_check = find_pg_connection(db_check);
    const char *sql_dbg = sqlite3_sql(pStmt);
    if (pg_check && pg_check->is_pg_active) {
        log_message("WARNING: Falling through to SQLite for redirected DB: %.300s", sql_dbg ? sql_dbg : "(unknown)");
    }
    int rc = sqlite3_step(pStmt);
    // Log result for debugging
    if (sql_dbg && !strstr(sql_dbg, "PRAGMA")) {
        log_message("STEP result: %d (sql=%.100s)", rc, sql_dbg);
    }
    return rc;
}

static int my_sqlite3_reset(sqlite3_stmt *pStmt) {
    pg_stmt_t *pg_stmt = find_any_pg_stmt(pStmt);

    if (pg_stmt) {
        // Clear parameter values for reuse
        for (int i = 0; i < MAX_PARAMS; i++) {
            if (pg_stmt->param_values[i]) {
                free(pg_stmt->param_values[i]);
                pg_stmt->param_values[i] = NULL;
            }
            pg_stmt->param_lengths[i] = 0;
            pg_stmt->param_formats[i] = 0;
        }

        if (pg_stmt->result) {
            PQclear(pg_stmt->result);
            pg_stmt->result = NULL;
        }

        // Reset row position for next execution
        pg_stmt->current_row = 0;
        pg_stmt->num_rows = 0;
    }

    return sqlite3_reset(pStmt);
}

static int my_sqlite3_finalize(sqlite3_stmt *pStmt) {
    // Clean up our tracking before SQLite finalizes
    unregister_stmt(pStmt);

    // Also clean up thread-local storage for cached statements
    unregister_cached_pg_stmt(pStmt);

    return sqlite3_finalize(pStmt);
}

static int my_sqlite3_clear_bindings(sqlite3_stmt *pStmt) {
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

    return sqlite3_clear_bindings(pStmt);
}

// ============================================================================
// Column Value Interception (for reads from PostgreSQL)
// ============================================================================

static int my_sqlite3_column_count(sqlite3_stmt *pStmt) {
#if PG_READ_ENABLED
    pg_stmt_t *pg_stmt = find_any_pg_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result) {
        return pg_stmt->num_cols;
    }
#endif
    return sqlite3_column_count(pStmt);
}

static int my_sqlite3_column_type(sqlite3_stmt *pStmt, int idx) {
#if PG_READ_ENABLED
    pg_stmt_t *pg_stmt = find_any_pg_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result) {
        // Bounds check
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            return SQLITE_NULL;
        }
        // Allow reading last row even after SQLITE_DONE
        int row_to_read = pg_stmt->current_row;
        if (row_to_read >= pg_stmt->num_rows && pg_stmt->num_rows > 0) {
            row_to_read = pg_stmt->num_rows - 1;
        }
        if (row_to_read < pg_stmt->num_rows) {
            if (PQgetisnull(pg_stmt->result, row_to_read, idx)) {
                return SQLITE_NULL;
            }
            // Map PostgreSQL OIDs to SQLite types
            Oid oid = PQftype(pg_stmt->result, idx);
            switch (oid) {
                case 16:   // BOOL - treat as integer (0/1)
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
#endif
    return sqlite3_column_type(pStmt, idx);
}

// Helper to convert PostgreSQL value to int (handles boolean t/f)
static int pg_value_to_int(const char *val) {
    if (!val || !*val) return 0;
    // PostgreSQL boolean: 't' = true = 1, 'f' = false = 0
    if (val[0] == 't' && val[1] == '\0') return 1;
    if (val[0] == 'f' && val[1] == '\0') return 0;
    return atoi(val);
}

// Helper to convert PostgreSQL value to int64 (handles boolean t/f)
static sqlite3_int64 pg_value_to_int64(const char *val) {
    if (!val || !*val) return 0;
    // PostgreSQL boolean: 't' = true = 1, 'f' = false = 0
    if (val[0] == 't' && val[1] == '\0') return 1;
    if (val[0] == 'f' && val[1] == '\0') return 0;
    return atoll(val);
}

static int my_sqlite3_column_int(sqlite3_stmt *pStmt, int idx) {
#if PG_READ_ENABLED
    pg_stmt_t *pg_stmt = find_any_pg_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result) {
        // Bounds check
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            return 0;
        }
        // Allow reading last row even after SQLITE_DONE
        int row_to_read = pg_stmt->current_row;
        if (row_to_read >= pg_stmt->num_rows && pg_stmt->num_rows > 0) {
            row_to_read = pg_stmt->num_rows - 1;
        }
        if (row_to_read < pg_stmt->num_rows) {
            if (!PQgetisnull(pg_stmt->result, row_to_read, idx)) {
                const char *val = PQgetvalue(pg_stmt->result, row_to_read, idx);
                return pg_value_to_int(val);
            }
            return 0;
        }
        return 0;
    }
#endif
    return sqlite3_column_int(pStmt, idx);
}

static sqlite3_int64 my_sqlite3_column_int64(sqlite3_stmt *pStmt, int idx) {
#if PG_READ_ENABLED
    pg_stmt_t *pg_stmt = find_any_pg_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result) {
        // Bounds check
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            return 0;
        }
        // Allow reading last row even after SQLITE_DONE
        int row_to_read = pg_stmt->current_row;
        if (row_to_read >= pg_stmt->num_rows && pg_stmt->num_rows > 0) {
            row_to_read = pg_stmt->num_rows - 1;
        }
        if (row_to_read < pg_stmt->num_rows &&
            !PQgetisnull(pg_stmt->result, row_to_read, idx)) {
            return pg_value_to_int64(PQgetvalue(pg_stmt->result, row_to_read, idx));
        }
        return 0;
    }
#endif
    return sqlite3_column_int64(pStmt, idx);
}

// Helper to convert PostgreSQL value to double (handles boolean t/f)
static double pg_value_to_double(const char *val) {
    if (!val || !*val) return 0.0;
    // PostgreSQL boolean: 't' = true = 1.0, 'f' = false = 0.0
    if (val[0] == 't' && val[1] == '\0') return 1.0;
    if (val[0] == 'f' && val[1] == '\0') return 0.0;
    return atof(val);
}

static double my_sqlite3_column_double(sqlite3_stmt *pStmt, int idx) {
#if PG_READ_ENABLED
    pg_stmt_t *pg_stmt = find_any_pg_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result) {
        // Bounds check
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            return 0.0;
        }
        // Allow reading last row even after SQLITE_DONE
        int row_to_read = pg_stmt->current_row;
        if (row_to_read >= pg_stmt->num_rows && pg_stmt->num_rows > 0) {
            row_to_read = pg_stmt->num_rows - 1;
        }
        if (row_to_read < pg_stmt->num_rows &&
            !PQgetisnull(pg_stmt->result, row_to_read, idx)) {
            return pg_value_to_double(PQgetvalue(pg_stmt->result, row_to_read, idx));
        }
        return 0.0;
    }
#endif
    return sqlite3_column_double(pStmt, idx);
}

static const unsigned char* my_sqlite3_column_text(sqlite3_stmt *pStmt, int idx) {
#if PG_READ_ENABLED
    pg_stmt_t *pg_stmt = find_any_pg_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result) {
        // Bounds check
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            return NULL;
        }
        // IMPORTANT: Allow reading last row even after SQLITE_DONE (SQLite behavior)
        int row_to_read = pg_stmt->current_row;
        if (row_to_read >= pg_stmt->num_rows && pg_stmt->num_rows > 0) {
            row_to_read = pg_stmt->num_rows - 1;
        }
        if (row_to_read < pg_stmt->num_rows) {
            if (PQgetisnull(pg_stmt->result, row_to_read, idx)) {
                return NULL;
            }
            return (const unsigned char*)PQgetvalue(pg_stmt->result, row_to_read, idx);
        }
        return NULL;
    }
#endif
    return sqlite3_column_text(pStmt, idx);
}

static const void* my_sqlite3_column_blob(sqlite3_stmt *pStmt, int idx) {
#if PG_READ_ENABLED
    pg_stmt_t *pg_stmt = find_any_pg_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result) {
        // Bounds check
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            return NULL;
        }
        // Allow reading last row even after SQLITE_DONE
        int row_to_read = pg_stmt->current_row;
        if (row_to_read >= pg_stmt->num_rows && pg_stmt->num_rows > 0) {
            row_to_read = pg_stmt->num_rows - 1;
        }
        if (row_to_read < pg_stmt->num_rows &&
            !PQgetisnull(pg_stmt->result, row_to_read, idx)) {
            return PQgetvalue(pg_stmt->result, row_to_read, idx);
        }
        return NULL;
    }
#endif
    return sqlite3_column_blob(pStmt, idx);
}

static int my_sqlite3_column_bytes(sqlite3_stmt *pStmt, int idx) {
#if PG_READ_ENABLED
    pg_stmt_t *pg_stmt = find_any_pg_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result) {
        // Bounds check
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            return 0;
        }
        // Allow reading last row even after SQLITE_DONE
        int row_to_read = pg_stmt->current_row;
        if (row_to_read >= pg_stmt->num_rows && pg_stmt->num_rows > 0) {
            row_to_read = pg_stmt->num_rows - 1;
        }
        if (row_to_read < pg_stmt->num_rows &&
            !PQgetisnull(pg_stmt->result, row_to_read, idx)) {
            return PQgetlength(pg_stmt->result, row_to_read, idx);
        }
        return 0;
    }
#endif
    return sqlite3_column_bytes(pStmt, idx);
}

static const char* my_sqlite3_column_name(sqlite3_stmt *pStmt, int idx) {
#if PG_READ_ENABLED
    pg_stmt_t *pg_stmt = find_any_pg_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result) {
        // Bounds check
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            return NULL;
        }
        return PQfname(pg_stmt->result, idx);
    }
#endif
    return sqlite3_column_name(pStmt, idx);
}

static int my_sqlite3_data_count(sqlite3_stmt *pStmt) {
#if PG_READ_ENABLED
    pg_stmt_t *pg_stmt = find_any_pg_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result) {
        // If we have a valid row, return the column count
        if (pg_stmt->current_row < pg_stmt->num_rows) {
            return pg_stmt->num_cols;
        }
        return 0;  // No current row
    }
#endif
    return sqlite3_data_count(pStmt);
}

// ============================================================================
// Changes and Last Insert Rowid (for write verification)
// ============================================================================

static int my_sqlite3_changes(sqlite3 *db) {
    pg_connection_t *pg_conn = find_pg_connection(db);
    if (pg_conn && pg_conn->is_pg_active) {
        return pg_conn->last_changes;
    }
    return sqlite3_changes(db);
}

static sqlite3_int64 my_sqlite3_last_insert_rowid(sqlite3 *db) {
    pg_connection_t *pg_conn = find_pg_connection(db);
    // Fallback: try to find any library connection if exact match fails
    if (!pg_conn) {
        pg_conn = find_any_pg_connection_for_library();
    }
    if (pg_conn && pg_conn->is_pg_active) {
        // If we have a stored rowid, use it
        if (pg_conn->last_insert_rowid > 0) {
            sqlite3_int64 rowid = pg_conn->last_insert_rowid;
            log_message("LAST_INSERT_ROWID returning %lld", rowid);
            return rowid;
        }
        // Otherwise, query PostgreSQL for the last inserted ID
        PGresult *res = PQexec(pg_conn->conn, "SELECT lastval()");
        if (PQresultStatus(res) == PGRES_TUPLES_OK && PQntuples(res) > 0) {
            const char *val = PQgetvalue(res, 0, 0);
            sqlite3_int64 rowid = val ? atoll(val) : 0;
            PQclear(res);
            log_message("LAST_INSERT_ROWID (lastval) returning %lld", rowid);
            return rowid;
        }
        PQclear(res);
    }
    return sqlite3_last_insert_rowid(db);
}

// Intercept sqlite3_get_table for debugging
static int my_sqlite3_get_table(sqlite3 *db, const char *sql, char ***pazResult, int *pnRow, int *pnColumn, char **pzErrMsg) {
    pg_connection_t *pg_conn = find_pg_connection(db);

    if (pg_conn && pg_conn->is_pg_active && pg_conn->conn && is_read_operation(sql)) {
        log_message("GET_TABLE PG: %.500s", sql);

        // Translate and execute on PostgreSQL
        sql_translation_t trans = sql_translate(sql);
        if (trans.success && trans.sql) {
            PGresult *res = PQexec(pg_conn->conn, trans.sql);
            ExecStatusType status = PQresultStatus(res);

            if (status == PGRES_TUPLES_OK) {
                int nrows = PQntuples(res);
                int ncols = PQnfields(res);

                // Allocate result array: (nrows+1) * ncols + 1 for column names
                // Format: [col1_name, col2_name, ..., row1_col1, row1_col2, ..., row2_col1, ...]
                int total = (nrows + 1) * ncols + 1;
                char **result = malloc(total * sizeof(char*));
                if (result) {
                    // Column names first
                    for (int c = 0; c < ncols; c++) {
                        result[c] = strdup(PQfname(res, c));
                    }
                    // Then data rows
                    for (int r = 0; r < nrows; r++) {
                        for (int c = 0; c < ncols; c++) {
                            if (PQgetisnull(res, r, c)) {
                                result[(r + 1) * ncols + c] = NULL;
                            } else {
                                result[(r + 1) * ncols + c] = strdup(PQgetvalue(res, r, c));
                            }
                        }
                    }
                    result[total - 1] = NULL;

                    *pazResult = result;
                    *pnRow = nrows;
                    *pnColumn = ncols;
                    if (pzErrMsg) *pzErrMsg = NULL;

                    log_message("GET_TABLE PG: %d rows, %d cols", nrows, ncols);
                    PQclear(res);
                    sql_translation_free(&trans);
                    return SQLITE_OK;
                }
            } else {
                log_message("GET_TABLE PG error: %s", PQerrorMessage(pg_conn->conn));
            }
            PQclear(res);
        }
        sql_translation_free(&trans);
    }

    return sqlite3_get_table(db, sql, pazResult, pnRow, pnColumn, pzErrMsg);
}

// Register interpositions
DYLD_INTERPOSE(my_sqlite3_get_table, sqlite3_get_table)
DYLD_INTERPOSE(my_sqlite3_open, sqlite3_open)
DYLD_INTERPOSE(my_sqlite3_open_v2, sqlite3_open_v2)
DYLD_INTERPOSE(my_sqlite3_close, sqlite3_close)
DYLD_INTERPOSE(my_sqlite3_close_v2, sqlite3_close_v2)
DYLD_INTERPOSE(my_sqlite3_exec, sqlite3_exec)
DYLD_INTERPOSE(my_sqlite3_changes, sqlite3_changes)
DYLD_INTERPOSE(my_sqlite3_last_insert_rowid, sqlite3_last_insert_rowid)

// Prepared statement interpositions
DYLD_INTERPOSE(my_sqlite3_prepare, sqlite3_prepare)
DYLD_INTERPOSE(my_sqlite3_prepare_v2, sqlite3_prepare_v2)
DYLD_INTERPOSE(my_sqlite3_prepare16_v2, sqlite3_prepare16_v2)
DYLD_INTERPOSE(my_sqlite3_bind_int, sqlite3_bind_int)
DYLD_INTERPOSE(my_sqlite3_bind_int64, sqlite3_bind_int64)
DYLD_INTERPOSE(my_sqlite3_bind_double, sqlite3_bind_double)
DYLD_INTERPOSE(my_sqlite3_bind_text, sqlite3_bind_text)
DYLD_INTERPOSE(my_sqlite3_bind_blob, sqlite3_bind_blob)
DYLD_INTERPOSE(my_sqlite3_bind_null, sqlite3_bind_null)
DYLD_INTERPOSE(my_sqlite3_step, sqlite3_step)
DYLD_INTERPOSE(my_sqlite3_reset, sqlite3_reset)
DYLD_INTERPOSE(my_sqlite3_finalize, sqlite3_finalize)
DYLD_INTERPOSE(my_sqlite3_clear_bindings, sqlite3_clear_bindings)

// Column value interpositions (for reads)
DYLD_INTERPOSE(my_sqlite3_column_count, sqlite3_column_count)
DYLD_INTERPOSE(my_sqlite3_column_type, sqlite3_column_type)
DYLD_INTERPOSE(my_sqlite3_column_int, sqlite3_column_int)
DYLD_INTERPOSE(my_sqlite3_column_int64, sqlite3_column_int64)
DYLD_INTERPOSE(my_sqlite3_column_double, sqlite3_column_double)
DYLD_INTERPOSE(my_sqlite3_column_text, sqlite3_column_text)
DYLD_INTERPOSE(my_sqlite3_column_blob, sqlite3_column_blob)
DYLD_INTERPOSE(my_sqlite3_column_bytes, sqlite3_column_bytes)
DYLD_INTERPOSE(my_sqlite3_column_name, sqlite3_column_name)
DYLD_INTERPOSE(my_sqlite3_data_count, sqlite3_data_count)

// ============================================================================
// Constructor/Destructor
// ============================================================================

__attribute__((constructor))
static void shim_init(void) {
    log_message("=== Plex PostgreSQL Interpose Shim loaded ===");
    sql_translator_init();
    load_config();
    shim_initialized = 1;
}

__attribute__((destructor))
static void shim_cleanup(void) {
    log_message("=== Plex PostgreSQL Interpose Shim unloading ===");

    pthread_mutex_lock(&conn_mutex);
    for (int i = 0; i < connection_count; i++) {
        if (connections[i] && connections[i]->conn) {
            PQfinish(connections[i]->conn);
        }
    }
    connection_count = 0;
    pthread_mutex_unlock(&conn_mutex);

    sql_translator_cleanup();

    if (log_file) {
        fclose(log_file);
        log_file = NULL;
    }
}
