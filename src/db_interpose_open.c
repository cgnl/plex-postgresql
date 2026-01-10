/*
 * Plex PostgreSQL Interposing Shim - Open/Close Operations
 *
 * Handles sqlite3_open, sqlite3_open_v2, sqlite3_close, sqlite3_close_v2.
 * Also includes helpers for dropping ICU indexes and FTS triggers.
 */

#include "db_interpose.h"

// ============================================================================
// Helper Functions
// ============================================================================

// Helper to drop indexes that use icu_root collation
// These indexes cause "no such collation sequence: icu_root" errors
void drop_icu_root_indexes(sqlite3 *db) {
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
void drop_fts_triggers(sqlite3 *db) {
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

// ============================================================================
// Open Functions
// ============================================================================

int my_sqlite3_open(const char *filename, sqlite3 **ppDb) {
    LOG_INFO("OPEN: %s (redirect=%d)", filename ? filename : "(null)", should_redirect(filename));

    int rc = orig_sqlite3_open ? orig_sqlite3_open(filename, ppDb) : SQLITE_ERROR;

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

int my_sqlite3_open_v2(const char *filename, sqlite3 **ppDb, int flags, const char *zVfs) {
    LOG_INFO("OPEN_V2: %s flags=0x%x (redirect=%d)",
             filename ? filename : "(null)", flags, should_redirect(filename));

    int rc = orig_sqlite3_open_v2 ? orig_sqlite3_open_v2(filename, ppDb, flags, zVfs) : SQLITE_ERROR;

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

// ============================================================================
// Close Functions
// ============================================================================

int my_sqlite3_close(sqlite3 *db) {
    // Get the handle connection (NOT pool connection) for this db
    pg_connection_t *handle_conn = pg_find_handle_connection(db);
    if (handle_conn) {
        LOG_INFO("CLOSE: PostgreSQL connection for %s", handle_conn->db_path);

        // If this is a library.db, release pool connection back to pool (don't free it!)
        if (strstr(handle_conn->db_path, "com.plexapp.plugins.library.db")) {
            pg_close_pool_for_db(db);
        }

        // Unregister and close the handle connection (not the pool connection)
        pg_unregister_connection(handle_conn);
        pg_close(handle_conn);
    }
    return orig_sqlite3_close ? orig_sqlite3_close(db) : SQLITE_ERROR;
}

int my_sqlite3_close_v2(sqlite3 *db) {
    // Get the handle connection (NOT pool connection) for this db
    pg_connection_t *handle_conn = pg_find_handle_connection(db);
    if (handle_conn) {
        // If this is a library.db, release pool connection back to pool (don't free it!)
        if (strstr(handle_conn->db_path, "com.plexapp.plugins.library.db")) {
            pg_close_pool_for_db(db);
        }

        // Unregister and close the handle connection (not the pool connection)
        pg_unregister_connection(handle_conn);
        pg_close(handle_conn);
    }
    return orig_sqlite3_close_v2 ? orig_sqlite3_close_v2(db) : SQLITE_ERROR;
}
