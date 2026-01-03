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

// Statement registry with hash table for O(1) lookup
typedef struct stmt_entry {
    sqlite3_stmt *sqlite_stmt;
    pg_stmt_t *pg_stmt;
    struct stmt_entry *next;  // For hash collision chaining
} stmt_entry_t;

#define HASH_BUCKETS 256  // Power of 2 for fast modulo

static stmt_entry_t *stmt_hash[HASH_BUCKETS];  // Hash table buckets
static stmt_entry_t stmt_pool[MAX_STATEMENTS]; // Pre-allocated entries
static int stmt_pool_next = 0;
static pthread_rwlock_t stmt_map_rwlock = PTHREAD_RWLOCK_INITIALIZER;

// Simple hash function for pointers
static inline unsigned int hash_ptr(void *ptr) {
    uintptr_t p = (uintptr_t)ptr;
    return (unsigned int)((p >> 4) ^ (p >> 12)) & (HASH_BUCKETS - 1);
}

// TLS key for cached statements
static pthread_key_t cached_stmts_key;
static pthread_once_t cached_stmts_key_once = PTHREAD_ONCE_INIT;
static volatile int cached_stmts_key_valid = 0;

// Fake sqlite3_value pool (lock-free with atomic index)
#define MAX_PG_VALUES 4096
static pg_value_t pg_values[MAX_PG_VALUES];
static atomic_int pg_value_idx = 0;

static volatile int statement_initialized = 0;
static pthread_once_t statement_init_once = PTHREAD_ONCE_INIT;

// ============================================================================
// TLS Setup
// ============================================================================

static void free_thread_cached_stmts(void *ptr) {
    thread_cached_stmts_t *tcs = (thread_cached_stmts_t *)ptr;
    if (tcs) {
        for (int i = 0; i < tcs->count; i++) {
            pg_stmt_t *pg_stmt = tcs->entries[i].pg_stmt;
            if (pg_stmt) {
                // CRITICAL FIX: Use unref to handle reference counting properly
                pg_stmt_unref(pg_stmt);
            }
        }
        free(tcs);
    }
}

static void create_cached_stmts_key(void) {
    int rc = pthread_key_create(&cached_stmts_key, free_thread_cached_stmts);
    if (rc != 0) {
        LOG_ERROR("pthread_key_create failed with error %d", rc);
        cached_stmts_key_valid = 0;
    } else {
        cached_stmts_key_valid = 1;
    }
}

static thread_cached_stmts_t* get_thread_cached_stmts(void) {
    pthread_once(&cached_stmts_key_once, create_cached_stmts_key);

    // Check if key creation was successful
    if (!cached_stmts_key_valid) {
        return NULL;
    }

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

static void do_statement_init(void) {
    memset(stmt_hash, 0, sizeof(stmt_hash));
    memset(stmt_pool, 0, sizeof(stmt_pool));
    stmt_pool_next = 0;
    statement_initialized = 1;
    LOG_DEBUG("pg_statement initialized with hash table");
}

void pg_statement_init(void) {
    pthread_once(&statement_init_once, do_statement_init);
}

void pg_statement_cleanup(void) {
    pthread_rwlock_wrlock(&stmt_map_rwlock);
    for (int i = 0; i < stmt_pool_next; i++) {
        if (stmt_pool[i].pg_stmt) {
            // CRITICAL FIX: Use unref for consistent reference counting
            pg_stmt_unref(stmt_pool[i].pg_stmt);
            stmt_pool[i].pg_stmt = NULL;
        }
    }
    memset(stmt_hash, 0, sizeof(stmt_hash));
    memset(stmt_pool, 0, sizeof(stmt_pool));
    stmt_pool_next = 0;
    pthread_rwlock_unlock(&stmt_map_rwlock);
    statement_initialized = 0;
}

// ============================================================================
// Statement Registry (Hash Table)
// ============================================================================

void pg_register_stmt(sqlite3_stmt *sqlite_stmt, pg_stmt_t *pg_stmt) {
    if (!sqlite_stmt || !pg_stmt) return;

    pthread_rwlock_wrlock(&stmt_map_rwlock);

    stmt_entry_t *entry = NULL;

    // First, try to find a freed slot (sqlite_stmt == NULL)
    for (int i = 0; i < stmt_pool_next; i++) {
        if (stmt_pool[i].sqlite_stmt == NULL) {
            entry = &stmt_pool[i];
            break;
        }
    }

    // If no freed slot, get a new one from the pool
    if (!entry) {
        if (stmt_pool_next >= MAX_STATEMENTS) {
            pthread_rwlock_unlock(&stmt_map_rwlock);
            LOG_ERROR("Statement pool full! MAX_STATEMENTS=%d", MAX_STATEMENTS);
            return;
        }
        entry = &stmt_pool[stmt_pool_next++];
    }

    entry->sqlite_stmt = sqlite_stmt;
    entry->pg_stmt = pg_stmt;

    // Insert into hash bucket
    unsigned int bucket = hash_ptr(sqlite_stmt);
    entry->next = stmt_hash[bucket];
    stmt_hash[bucket] = entry;

    pthread_rwlock_unlock(&stmt_map_rwlock);
}

void pg_unregister_stmt(sqlite3_stmt *sqlite_stmt) {
    if (!sqlite_stmt) return;

    pthread_rwlock_wrlock(&stmt_map_rwlock);

    unsigned int bucket = hash_ptr(sqlite_stmt);
    stmt_entry_t **prev = &stmt_hash[bucket];
    stmt_entry_t *entry = stmt_hash[bucket];

    while (entry) {
        if (entry->sqlite_stmt == sqlite_stmt) {
            // Remove from hash chain (mark as deleted, don't actually remove from pool)
            *prev = entry->next;
            entry->sqlite_stmt = NULL;
            entry->pg_stmt = NULL;
            break;
        }
        prev = &entry->next;
        entry = entry->next;
    }

    pthread_rwlock_unlock(&stmt_map_rwlock);
}

pg_stmt_t* pg_find_stmt(sqlite3_stmt *stmt) {
    if (!stmt) return NULL;

    pthread_rwlock_rdlock(&stmt_map_rwlock);  // Read lock - multiple readers allowed

    unsigned int bucket = hash_ptr(stmt);
    stmt_entry_t *entry = stmt_hash[bucket];

    while (entry) {
        if (entry->sqlite_stmt == stmt) {
            pg_stmt_t *result = entry->pg_stmt;
            pthread_rwlock_unlock(&stmt_map_rwlock);
            return result;
        }
        entry = entry->next;
    }

    pthread_rwlock_unlock(&stmt_map_rwlock);
    return NULL;
}

pg_stmt_t* pg_find_any_stmt(sqlite3_stmt *stmt) {
    // First try direct lookup (fast hash lookup)
    pg_stmt_t *pg_stmt = pg_find_stmt(stmt);
    if (pg_stmt) return pg_stmt;

    // Then try TLS cache
    pg_stmt = pg_find_cached_stmt(stmt);
    if (pg_stmt) return pg_stmt;

    return NULL;
}

int pg_is_our_stmt(void *ptr) {
    if (!ptr) return 0;

    // Note: This still needs O(n) scan since we're searching by pg_stmt, not sqlite_stmt
    // But it's called much less frequently than pg_find_stmt
    pthread_rwlock_rdlock(&stmt_map_rwlock);  // Read lock
    for (int i = 0; i < stmt_pool_next; i++) {
        if (stmt_pool[i].pg_stmt == ptr) {
            pthread_rwlock_unlock(&stmt_map_rwlock);
            return 1;
        }
    }
    pthread_rwlock_unlock(&stmt_map_rwlock);
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
                // CRITICAL FIX: Use unref instead of free for proper refcounting
                pg_stmt_unref(old);
            }
            // CRITICAL FIX: Increment ref count when caching
            pg_stmt_ref(pg_stmt);
            tcs->entries[i].pg_stmt = pg_stmt;
            return;
        }
    }

    // CRITICAL FIX: Increment ref count when caching new entry
    pg_stmt_ref(pg_stmt);

    // Add new entry
    if (tcs->count < MAX_CACHED_STMTS_PER_THREAD) {
        tcs->entries[tcs->count].sqlite_stmt = sqlite_stmt;
        tcs->entries[tcs->count].pg_stmt = pg_stmt;
        tcs->count++;
    } else {
        // Evict oldest entry
        pg_stmt_t *old = tcs->entries[0].pg_stmt;
        // CRITICAL FIX: Use unref instead of free for proper refcounting
        if (old) pg_stmt_unref(old);

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

            // Clear entry pointer FIRST
            tcs->entries[i].pg_stmt = NULL;

            // Shift remaining entries
            for (int j = i; j < tcs->count - 1; j++) {
                tcs->entries[j] = tcs->entries[j + 1];
            }
            tcs->count--;

            // Unref AFTER removing from list (for TLS destructor ownership)
            if (old) pg_stmt_unref(old);
            return;
        }
    }
}

// CRITICAL FIX: Weak clear - removes from cache without unreferencing
// Used by finalize() because global registry owns the reference
void pg_clear_cached_stmt_weak(sqlite3_stmt *sqlite_stmt) {
    thread_cached_stmts_t *tcs = get_thread_cached_stmts();
    if (!tcs) return;

    for (int i = 0; i < tcs->count; i++) {
        if (tcs->entries[i].sqlite_stmt == sqlite_stmt) {
            // Just remove from array - DON'T unref (weak reference)
            tcs->entries[i].pg_stmt = NULL;
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

    pthread_mutex_init(&stmt->mutex, NULL);
    atomic_store(&stmt->ref_count, 1);  // CRITICAL FIX: Initialize ref count
    stmt->conn = conn;
    stmt->shadow_stmt = shadow_stmt;
    stmt->sql = sql ? strdup(sql) : NULL;
    stmt->current_row = -1;
    stmt->write_executed = 0;  // Initialize write execution guard
    stmt->read_done = 0;       // Initialize read completion guard

    return stmt;
}

// CRITICAL FIX: Reference counting to prevent double-free
void pg_stmt_ref(pg_stmt_t *stmt) {
    if (!stmt) return;
    atomic_fetch_add(&stmt->ref_count, 1);
}

void pg_stmt_unref(pg_stmt_t *stmt) {
    if (!stmt) return;
    int old = atomic_fetch_sub(&stmt->ref_count, 1);
    if (old == 1) {
        // Last reference - actually free
        pg_stmt_free(stmt);
    } else if (old <= 0) {
        LOG_ERROR("pg_stmt_unref: ref_count was %d, possible double-free!", old);
    }
}

// Helper: check if param_value points to pre-allocated buffer
static inline int is_preallocated_buffer(pg_stmt_t *stmt, int idx) {
    return stmt->param_values[idx] >= stmt->param_buffers[idx] &&
           stmt->param_values[idx] < stmt->param_buffers[idx] + 32;
}

void pg_stmt_free(pg_stmt_t *stmt) {
    if (!stmt) return;

    if (stmt->sql) free(stmt->sql);
    if (stmt->pg_sql && stmt->pg_sql != stmt->sql) free(stmt->pg_sql);
    if (stmt->result) PQclear(stmt->result);

    for (int i = 0; i < stmt->param_count; i++) {
        // Only free if not pointing to pre-allocated buffer
        if (stmt->param_values[i] && !is_preallocated_buffer(stmt, i)) {
            free(stmt->param_values[i]);
        }
    }

    // Free parameter names (for named parameter mapping)
    if (stmt->param_names) {
        for (int i = 0; i < stmt->param_count; i++) {
            if (stmt->param_names[i]) free(stmt->param_names[i]);
        }
        free(stmt->param_names);
    }

    // Free decoded blob cache
    for (int i = 0; i < MAX_PARAMS; i++) {
        if (stmt->decoded_blobs[i]) {
            free(stmt->decoded_blobs[i]);
            stmt->decoded_blobs[i] = NULL;
        }
    }

    // Free cached text/blob values
    for (int i = 0; i < MAX_PARAMS; i++) {
        if (stmt->cached_text[i]) {
            free(stmt->cached_text[i]);
            stmt->cached_text[i] = NULL;
        }
        if (stmt->cached_blob[i]) {
            free(stmt->cached_blob[i]);
            stmt->cached_blob[i] = NULL;
        }
    }

    pthread_mutex_destroy(&stmt->mutex);
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
    stmt->read_done = 0;       // Reset read completion guard

    // Clear decoded blob cache
    for (int i = 0; i < MAX_PARAMS; i++) {
        if (stmt->decoded_blobs[i]) {
            free(stmt->decoded_blobs[i]);
            stmt->decoded_blobs[i] = NULL;
            stmt->decoded_blob_lens[i] = 0;
        }
    }
    stmt->decoded_blob_row = -1;

    // Clear cached text/blob values (these must persist until step/reset/finalize)
    for (int i = 0; i < MAX_PARAMS; i++) {
        if (stmt->cached_text[i]) {
            free(stmt->cached_text[i]);
            stmt->cached_text[i] = NULL;
        }
        if (stmt->cached_blob[i]) {
            free(stmt->cached_blob[i]);
            stmt->cached_blob[i] = NULL;
            stmt->cached_blob_len[i] = 0;
        }
    }
    stmt->cached_row = -1;
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
    // Lock-free slot allocation using atomic increment
    int slot = atomic_fetch_add(&pg_value_idx, 1) & (MAX_PG_VALUES - 1);
    pg_value_t *pv = &pg_values[slot];

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

    return (sqlite3_value*)pv;
}

int pg_is_our_value(sqlite3_value *val) {
    if (!val) return 0;
    pg_value_t *pv = (pg_value_t*)val;
    return pv->magic == PG_VALUE_MAGIC;
}
