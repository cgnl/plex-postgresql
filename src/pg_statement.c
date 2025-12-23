/*
 * PostgreSQL Shim - Statement Module
 * Statement tracking, TLS caching, and helper functions
 */

#include "pg_statement.h"
#include "pg_logging.h"
#include "pg_config.h"
#include "sql_translator.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <ctype.h>

// ============================================================================
// Static State
// ============================================================================

// Statement registry
typedef struct {
    sqlite3_stmt *sqlite_stmt;
    pg_stmt_t *pg_stmt;
} stmt_entry_t;

static stmt_entry_t stmt_map[MAX_STATEMENTS];
static pthread_mutex_t stmt_map_mutex = PTHREAD_MUTEX_INITIALIZER;

// TLS key for cached statements
static pthread_key_t cached_stmts_key;
static pthread_once_t cached_stmts_key_once = PTHREAD_ONCE_INIT;

// Fake sqlite3_value pool
#define MAX_PG_VALUES 4096
static pg_value_t pg_values[MAX_PG_VALUES];
static int pg_value_idx = 0;
static pthread_mutex_t pg_value_mutex = PTHREAD_MUTEX_INITIALIZER;

static int statement_initialized = 0;

// ============================================================================
// TLS Setup
// ============================================================================

static void free_thread_cached_stmts(void *ptr) {
    thread_cached_stmts_t *tcs = (thread_cached_stmts_t *)ptr;
    if (tcs) {
        for (int i = 0; i < tcs->count; i++) {
            pg_stmt_t *pg_stmt = tcs->entries[i].pg_stmt;
            if (pg_stmt) {
                pg_stmt_free(pg_stmt);
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

// ============================================================================
// Initialization
// ============================================================================

void pg_statement_init(void) {
    if (statement_initialized) return;
    memset(stmt_map, 0, sizeof(stmt_map));
    statement_initialized = 1;
    LOG_DEBUG("pg_statement initialized");
}

void pg_statement_cleanup(void) {
    pthread_mutex_lock(&stmt_map_mutex);
    for (int i = 0; i < MAX_STATEMENTS; i++) {
        if (stmt_map[i].pg_stmt) {
            pg_stmt_free(stmt_map[i].pg_stmt);
            stmt_map[i].pg_stmt = NULL;
            stmt_map[i].sqlite_stmt = NULL;
        }
    }
    pthread_mutex_unlock(&stmt_map_mutex);
    statement_initialized = 0;
}

// ============================================================================
// Statement Registry
// ============================================================================

void pg_register_stmt(sqlite3_stmt *sqlite_stmt, pg_stmt_t *pg_stmt) {
    if (!sqlite_stmt || !pg_stmt) return;

    pthread_mutex_lock(&stmt_map_mutex);
    for (int i = 0; i < MAX_STATEMENTS; i++) {
        if (stmt_map[i].sqlite_stmt == NULL) {
            stmt_map[i].sqlite_stmt = sqlite_stmt;
            stmt_map[i].pg_stmt = pg_stmt;
            pthread_mutex_unlock(&stmt_map_mutex);
            return;
        }
    }
    pthread_mutex_unlock(&stmt_map_mutex);
    LOG_ERROR("Statement registry full! MAX_STATEMENTS=%d", MAX_STATEMENTS);
}

void pg_unregister_stmt(sqlite3_stmt *sqlite_stmt) {
    if (!sqlite_stmt) return;

    pthread_mutex_lock(&stmt_map_mutex);
    for (int i = 0; i < MAX_STATEMENTS; i++) {
        if (stmt_map[i].sqlite_stmt == sqlite_stmt) {
            stmt_map[i].sqlite_stmt = NULL;
            stmt_map[i].pg_stmt = NULL;
            break;
        }
    }
    pthread_mutex_unlock(&stmt_map_mutex);
}

pg_stmt_t* pg_find_stmt(sqlite3_stmt *stmt) {
    if (!stmt) return NULL;

    pthread_mutex_lock(&stmt_map_mutex);
    for (int i = 0; i < MAX_STATEMENTS; i++) {
        if (stmt_map[i].sqlite_stmt == stmt) {
            pg_stmt_t *result = stmt_map[i].pg_stmt;
            pthread_mutex_unlock(&stmt_map_mutex);
            return result;
        }
    }
    pthread_mutex_unlock(&stmt_map_mutex);
    return NULL;
}

pg_stmt_t* pg_find_any_stmt(sqlite3_stmt *stmt) {
    // First try direct lookup
    pg_stmt_t *pg_stmt = pg_find_stmt(stmt);
    if (pg_stmt) return pg_stmt;

    // Then try TLS cache
    pg_stmt = pg_find_cached_stmt(stmt);
    if (pg_stmt) return pg_stmt;

    return NULL;
}

int pg_is_our_stmt(void *ptr) {
    if (!ptr) return 0;

    pthread_mutex_lock(&stmt_map_mutex);
    for (int i = 0; i < MAX_STATEMENTS; i++) {
        if (stmt_map[i].pg_stmt == ptr) {
            pthread_mutex_unlock(&stmt_map_mutex);
            return 1;
        }
    }
    pthread_mutex_unlock(&stmt_map_mutex);
    return 0;
}

// ============================================================================
// TLS Cached Statement Management
// ============================================================================

void pg_register_cached_stmt(sqlite3_stmt *sqlite_stmt, pg_stmt_t *pg_stmt) {
    thread_cached_stmts_t *tcs = get_thread_cached_stmts();
    if (!tcs) return;

    // Check if already registered - replace
    for (int i = 0; i < tcs->count; i++) {
        if (tcs->entries[i].sqlite_stmt == sqlite_stmt) {
            pg_stmt_t *old = tcs->entries[i].pg_stmt;
            if (old && old != pg_stmt) {
                pg_stmt_free(old);
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
        // Evict oldest entry
        pg_stmt_t *old = tcs->entries[0].pg_stmt;
        if (old) pg_stmt_free(old);

        memmove(&tcs->entries[0], &tcs->entries[1],
                (MAX_CACHED_STMTS_PER_THREAD - 1) * sizeof(cached_stmt_entry_t));
        tcs->entries[MAX_CACHED_STMTS_PER_THREAD - 1].sqlite_stmt = sqlite_stmt;
        tcs->entries[MAX_CACHED_STMTS_PER_THREAD - 1].pg_stmt = pg_stmt;
    }
}

pg_stmt_t* pg_find_cached_stmt(sqlite3_stmt *sqlite_stmt) {
    thread_cached_stmts_t *tcs = get_thread_cached_stmts();
    if (!tcs) return NULL;

    for (int i = 0; i < tcs->count; i++) {
        if (tcs->entries[i].sqlite_stmt == sqlite_stmt) {
            return tcs->entries[i].pg_stmt;
        }
    }
    return NULL;
}

void pg_clear_cached_stmt(sqlite3_stmt *sqlite_stmt) {
    thread_cached_stmts_t *tcs = get_thread_cached_stmts();
    if (!tcs) return;

    for (int i = 0; i < tcs->count; i++) {
        if (tcs->entries[i].sqlite_stmt == sqlite_stmt) {
            pg_stmt_t *old = tcs->entries[i].pg_stmt;
            if (old) pg_stmt_free(old);

            // Shift remaining entries
            for (int j = i; j < tcs->count - 1; j++) {
                tcs->entries[j] = tcs->entries[j + 1];
            }
            tcs->count--;
            return;
        }
    }
}

// ============================================================================
// Statement Lifecycle
// ============================================================================

pg_stmt_t* pg_stmt_create(pg_connection_t *conn, const char *sql, sqlite3_stmt *shadow_stmt) {
    pg_stmt_t *stmt = calloc(1, sizeof(pg_stmt_t));
    if (!stmt) return NULL;

    stmt->conn = conn;
    stmt->shadow_stmt = shadow_stmt;
    stmt->sql = sql ? strdup(sql) : NULL;
    stmt->current_row = -1;
    stmt->write_executed = 0;  // Initialize write execution guard

    return stmt;
}

void pg_stmt_free(pg_stmt_t *stmt) {
    if (!stmt) return;

    if (stmt->sql) free(stmt->sql);
    if (stmt->pg_sql && stmt->pg_sql != stmt->sql) free(stmt->pg_sql);
    if (stmt->result) PQclear(stmt->result);

    for (int i = 0; i < stmt->param_count; i++) {
        if (stmt->param_values[i]) free(stmt->param_values[i]);
    }

    // Free decoded blob cache
    for (int i = 0; i < MAX_PARAMS; i++) {
        if (stmt->decoded_blobs[i]) {
            free(stmt->decoded_blobs[i]);
            stmt->decoded_blobs[i] = NULL;
        }
    }

    free(stmt);
}

void pg_stmt_clear_result(pg_stmt_t *stmt) {
    if (!stmt) return;
    if (stmt->result) {
        PQclear(stmt->result);
        stmt->result = NULL;
    }
    stmt->current_row = -1;
    stmt->num_rows = 0;
    stmt->num_cols = 0;
    stmt->write_executed = 0;  // Reset write execution guard

    // Clear decoded blob cache
    for (int i = 0; i < MAX_PARAMS; i++) {
        if (stmt->decoded_blobs[i]) {
            free(stmt->decoded_blobs[i]);
            stmt->decoded_blobs[i] = NULL;
            stmt->decoded_blob_lens[i] = 0;
        }
    }
    stmt->decoded_blob_row = -1;
}

// ============================================================================
// SQL Transformation Helpers
// ============================================================================

char* convert_metadata_settings_insert_to_upsert(const char *sql) {
    if (!sql) return NULL;
    if (!strcasestr(sql, "INSERT INTO")) return NULL;
    if (!strcasestr(sql, "metadata_item_settings")) return NULL;
    if (strcasestr(sql, "ON CONFLICT")) return NULL;
    if (strcasestr(sql, "RETURNING")) return NULL;

    static const char *on_conflict =
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

    size_t len = strlen(sql) + strlen(on_conflict) + 1;
    char *result = malloc(len);
    if (result) {
        snprintf(result, len, "%s%s", sql, on_conflict);
    }
    return result;
}

sqlite3_int64 extract_metadata_id_from_generator_sql(const char *sql) {
    if (!sql) return 0;
    if (!strcasestr(sql, "play_queue_generators")) return 0;
    if (!strcasestr(sql, "INSERT")) return 0;

    // Look for URL-encoded /metadata/ pattern
    const char *pattern = "%2Fmetadata%2F";
    const char *pos = strstr(sql, pattern);
    if (!pos) {
        pattern = "/metadata/";
        pos = strstr(sql, pattern);
    }
    if (!pos) return 0;

    pos += strlen(pattern);
    sqlite3_int64 id = 0;
    while (*pos >= '0' && *pos <= '9') {
        id = id * 10 + (*pos - '0');
        pos++;
    }
    return id;
}

// ============================================================================
// Fake sqlite3_value Helpers
// ============================================================================

int pg_oid_to_sqlite_type(Oid oid) {
    switch (oid) {
        case 16:   // BOOL
        case 20:   // INT8
        case 21:   // INT2
        case 23:   // INT4
            return SQLITE_INTEGER;
        case 700:  // FLOAT4
        case 701:  // FLOAT8
        case 1700: // NUMERIC
            return SQLITE_FLOAT;
        case 17:   // BYTEA
            return SQLITE_BLOB;
        case 25:   // TEXT
        case 1042: // BPCHAR
        case 1043: // VARCHAR
        default:
            return SQLITE_TEXT;
    }
}

sqlite3_value* pg_create_column_value(pg_stmt_t *stmt, int col_idx) {
    pthread_mutex_lock(&pg_value_mutex);
    pg_value_t *pv = &pg_values[pg_value_idx++ % MAX_PG_VALUES];

    pv->magic = PG_VALUE_MAGIC;
    pv->stmt = stmt;
    pv->col_idx = col_idx;

    // Determine type
    if (!stmt || !stmt->result || stmt->current_row < 0 ||
        stmt->current_row >= stmt->num_rows ||
        PQgetisnull(stmt->result, stmt->current_row, col_idx)) {
        pv->type = SQLITE_NULL;
    } else {
        pv->type = pg_oid_to_sqlite_type(PQftype(stmt->result, col_idx));
    }

    pthread_mutex_unlock(&pg_value_mutex);
    return (sqlite3_value*)pv;
}

int pg_is_our_value(sqlite3_value *val) {
    if (!val) return 0;
    pg_value_t *pv = (pg_value_t*)val;
    return pv->magic == PG_VALUE_MAGIC;
}
