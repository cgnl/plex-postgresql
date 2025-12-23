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
// Globals (minimal - most state is in modules)
// ============================================================================

static int shim_initialized = 0;

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
static int fake_value_next = 0;

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
                    LOG_ERROR("PostgreSQL exec error: %s", PQerrorMessage(pg_conn->conn));
                }

                if (insert_sql) free(insert_sql);
                PQclear(res);
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
    if (!strcasestr(sql, "fts4_")) return NULL;

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

static int my_sqlite3_prepare_v2(sqlite3 *db, const char *zSql, int nByte,
                                  sqlite3_stmt **ppStmt, const char **pzTail) {
    pg_connection_t *pg_conn = pg_find_connection(db);
    int is_write = is_write_operation(zSql);
    int is_read = is_read_operation(zSql);

    // Clean SQL for SQLite (remove icu_root and FTS references)
    char *cleaned_sql = NULL;
    const char *sql_for_sqlite = zSql;

    // ALWAYS simplify FTS queries for SQLite, even without PG connection
    // because SQLite shadow DB doesn't have FTS virtual tables
    if (strcasestr(zSql, "fts4_")) {
        cleaned_sql = simplify_fts_for_sqlite(zSql);
        if (cleaned_sql) {
            sql_for_sqlite = cleaned_sql;
            LOG_INFO("FTS query simplified for SQLite: %.1000s", zSql);
        }
    }

    // ALWAYS remove "collate icu_root" since SQLite shadow DB doesn't support it
    if (strcasestr(sql_for_sqlite, "collate icu_root")) {
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

    int rc = sqlite3_prepare_v2(db, sql_for_sqlite, cleaned_sql ? -1 : nByte, ppStmt, pzTail);

    if (rc != SQLITE_OK || !*ppStmt) {
        if (cleaned_sql) free(cleaned_sql);
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
                }
                sql_translation_free(&trans);
            }

            // Count parameters
            const char *p = zSql;
            while (*p) { if (*p == '?') pg_stmt->param_count++; p++; }

            pg_register_stmt(*ppStmt, pg_stmt);
        }
    }

    if (cleaned_sql) free(cleaned_sql);
    return rc;
}

static int my_sqlite3_prepare(sqlite3 *db, const char *zSql, int nByte,
                              sqlite3_stmt **ppStmt, const char **pzTail) {
    return sqlite3_prepare_v2(db, zSql, nByte, ppStmt, pzTail);
}

static int my_sqlite3_prepare16_v2(sqlite3 *db, const void *zSql, int nByte,
                                    sqlite3_stmt **ppStmt, const void **pzTail) {
    return sqlite3_prepare16_v2(db, zSql, nByte, ppStmt, pzTail);
}

// ============================================================================
// Interposed SQLite Functions - Bind
// ============================================================================

static int my_sqlite3_bind_int(sqlite3_stmt *pStmt, int idx, int val) {
    int rc = sqlite3_bind_int(pStmt, idx, val);

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS) {
        char buf[32];
        snprintf(buf, sizeof(buf), "%d", val);
        if (pg_stmt->param_values[idx-1]) free(pg_stmt->param_values[idx-1]);
        pg_stmt->param_values[idx-1] = strdup(buf);
        if (idx > pg_stmt->param_count) pg_stmt->param_count = idx;
    }

    return rc;
}

static int my_sqlite3_bind_int64(sqlite3_stmt *pStmt, int idx, sqlite3_int64 val) {
    int rc = sqlite3_bind_int64(pStmt, idx, val);

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS) {
        char buf[32];
        snprintf(buf, sizeof(buf), "%lld", val);
        if (pg_stmt->param_values[idx-1]) free(pg_stmt->param_values[idx-1]);
        pg_stmt->param_values[idx-1] = strdup(buf);
        if (idx > pg_stmt->param_count) pg_stmt->param_count = idx;
    }

    return rc;
}

static int my_sqlite3_bind_double(sqlite3_stmt *pStmt, int idx, double val) {
    int rc = sqlite3_bind_double(pStmt, idx, val);

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS) {
        char buf[64];
        snprintf(buf, sizeof(buf), "%.17g", val);
        if (pg_stmt->param_values[idx-1]) free(pg_stmt->param_values[idx-1]);
        pg_stmt->param_values[idx-1] = strdup(buf);
        if (idx > pg_stmt->param_count) pg_stmt->param_count = idx;
    }

    return rc;
}

static int my_sqlite3_bind_text(sqlite3_stmt *pStmt, int idx, const char *val,
                                 int nBytes, void (*destructor)(void*)) {
    int rc = sqlite3_bind_text(pStmt, idx, val, nBytes, destructor);

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
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
        if (idx > pg_stmt->param_count) pg_stmt->param_count = idx;
    }

    return rc;
}

static int my_sqlite3_bind_blob(sqlite3_stmt *pStmt, int idx, const void *val,
                                 int nBytes, void (*destructor)(void*)) {
    int rc = sqlite3_bind_blob(pStmt, idx, val, nBytes, destructor);

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS && val && nBytes > 0) {
        if (pg_stmt->param_values[idx-1]) free(pg_stmt->param_values[idx-1]);
        pg_stmt->param_values[idx-1] = malloc(nBytes);
        if (pg_stmt->param_values[idx-1]) {
            memcpy(pg_stmt->param_values[idx-1], val, nBytes);
        }
        pg_stmt->param_lengths[idx-1] = nBytes;
        pg_stmt->param_formats[idx-1] = 1;  // binary
        if (idx > pg_stmt->param_count) pg_stmt->param_count = idx;
    }

    return rc;
}

static int my_sqlite3_bind_null(sqlite3_stmt *pStmt, int idx) {
    int rc = sqlite3_bind_null(pStmt, idx);

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS) {
        if (pg_stmt->param_values[idx-1]) {
            free(pg_stmt->param_values[idx-1]);
            pg_stmt->param_values[idx-1] = NULL;
        }
        if (idx > pg_stmt->param_count) pg_stmt->param_count = idx;
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
            if (sql && is_write_operation(sql) && !should_skip_sql(sql)) {
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

                    PGresult *res = PQexec(cached_exec_conn->conn, exec_sql);
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
                        log_sql_fallback(sql, exec_sql, PQerrorMessage(pg_conn->conn), "CACHED WRITE");
                    }

                    if (insert_sql) free(insert_sql);
                    PQclear(res);
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
                        if (expanded_sql) sqlite3_free(expanded_sql);
                        return (cached->current_row < cached->num_rows) ? SQLITE_ROW : SQLITE_DONE;
                    }

                    // No result yet - execute PostgreSQL query
                    sql_translation_t trans = sql_translate(sql);
                    if (trans.success && trans.sql) {
                        // Log cached SELECT on play_queue_generators
                        if (strstr(sql, "play_queue_generators")) {
                            LOG_INFO("CACHED SELECT play_queue_generators on thread %p conn %p: %s",
                                    (void*)pthread_self(), (void*)cached_read_conn, trans.sql);
                        }
                        // Log cached media queries
                        if (strstr(trans.sql, "media_parts") || strstr(trans.sql, "media_items")) {
                            LOG_INFO("CACHED MEDIA QUERY: %s", trans.sql);
                        }

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
                            new_stmt->result = PQexec(cached_read_conn->conn, trans.sql);
                            if (PQresultStatus(new_stmt->result) == PGRES_TUPLES_OK) {
                                new_stmt->num_rows = PQntuples(new_stmt->result);
                                new_stmt->num_cols = PQnfields(new_stmt->result);
                                new_stmt->current_row = 0;
                                // Log result count for play_queue_generators
                                if (strstr(sql, "play_queue_generators")) {
                                    LOG_INFO("CACHED SELECT play_queue_generators returned %d rows", new_stmt->num_rows);
                                }
                                // Log cached media results
                                if (strstr(trans.sql, "media_parts") || strstr(trans.sql, "media_items")) {
                                    LOG_INFO("CACHED MEDIA QUERY returned %d rows", new_stmt->num_rows);
                                }
                                sql_translation_free(&trans);
                                if (expanded_sql) sqlite3_free(expanded_sql);
                                return (new_stmt->num_rows > 0) ? SQLITE_ROW : SQLITE_DONE;
                            } else {
                                log_sql_fallback(sql, trans.sql, PQerrorMessage(cached_read_conn->conn), "CACHED READ");
                                PQclear(new_stmt->result);
                                new_stmt->result = NULL;
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
        }
    }

    if (pg_stmt && pg_stmt->pg_sql && exec_conn && exec_conn->conn) {
        const char *paramValues[MAX_PARAMS];
        for (int i = 0; i < pg_stmt->param_count && i < MAX_PARAMS; i++) {
            paramValues[i] = pg_stmt->param_values[i];
        }

        if (pg_stmt->is_pg == 2) {  // READ
            if (!pg_stmt->result) {
                // Log SELECT on play_queue_generators for debugging
                if (strstr(pg_stmt->pg_sql, "play_queue_generators")) {
                    LOG_INFO("SELECT play_queue_generators on thread %p conn %p: %s",
                            (void*)pthread_self(), (void*)exec_conn, pg_stmt->pg_sql);
                }
                // Log media_parts queries with parameters
                if (strstr(pg_stmt->pg_sql, "media_parts") || strstr(pg_stmt->pg_sql, "media_items")) {
                    LOG_INFO("MEDIA QUERY (params=%d): %s", pg_stmt->param_count, pg_stmt->pg_sql);
                    for (int i = 0; i < pg_stmt->param_count && i < 5; i++) {
                        LOG_INFO("  PARAM[%d] = %s", i+1, paramValues[i] ? paramValues[i] : "NULL");
                    }
                    // Debug: check search_path
                    if (strstr(pg_stmt->pg_sql, "directories")) {
                        PGresult *sp = PQexec(exec_conn->conn, "SHOW search_path");
                        if (PQresultStatus(sp) == PGRES_TUPLES_OK && PQntuples(sp) > 0) {
                            LOG_INFO("  search_path = %s", PQgetvalue(sp, 0, 0));
                        }
                        PQclear(sp);
                    }
                }

                // Log translated query for debugging
                if (strstr(pg_stmt->pg_sql, "distinct")) {
                    LOG_INFO("[DISTINCT QUERY] %.200s...", pg_stmt->pg_sql);
                }

                pg_stmt->result = PQexecParams(exec_conn->conn, pg_stmt->pg_sql,
                    pg_stmt->param_count, NULL, paramValues, NULL, NULL, 0);

                // Check for query errors
                ExecStatusType status = PQresultStatus(pg_stmt->result);
                if (status != PGRES_TUPLES_OK && status != PGRES_COMMAND_OK) {
                    LOG_ERROR("PostgreSQL query failed: %s", PQerrorMessage(exec_conn->conn));
                    LOG_ERROR("Failed query: %.500s", pg_stmt->pg_sql);
                }

                if (PQresultStatus(pg_stmt->result) == PGRES_TUPLES_OK) {
                    pg_stmt->num_rows = PQntuples(pg_stmt->result);
                    pg_stmt->num_cols = PQnfields(pg_stmt->result);
                    pg_stmt->current_row = 0;

                    // Log result count for play_queue_generators
                    if (strstr(pg_stmt->pg_sql, "play_queue_generators")) {
                        LOG_INFO("SELECT play_queue_generators returned %d rows", PQntuples(pg_stmt->result));
                    }
                    // Log media_parts results
                    if (strstr(pg_stmt->pg_sql, "media_parts") || strstr(pg_stmt->pg_sql, "media_items")) {
                        LOG_INFO("MEDIA QUERY returned %d rows, %d cols", pg_stmt->num_rows, pg_stmt->num_cols);
                    }

                    // Log ALL queries that have metadata_type in SELECT or have metadata_items
                    if (strstr(pg_stmt->pg_sql, "metadata_items") || strstr(pg_stmt->pg_sql, "metadata_type")) {
                        LOG_ERROR("METADATA_QUERY: returned %d rows, %d cols", pg_stmt->num_rows, pg_stmt->num_cols);
                        LOG_ERROR("  Query: %.200s", pg_stmt->pg_sql);
                        // Log all column names
                        LOG_ERROR("  Columns:");
                        for (int i = 0; i < pg_stmt->num_cols && i < 50; i++) {
                            LOG_ERROR("    [%d] %s", i, PQfname(pg_stmt->result, i));
                        }
                        if (pg_stmt->num_cols > 50) {
                            LOG_ERROR("    ... (%d total columns)", pg_stmt->num_cols);
                        }
                    }
                } else {
                    log_sql_fallback(pg_stmt->sql, pg_stmt->pg_sql,
                                     PQerrorMessage(exec_conn->conn), "PREPARED READ");
                    PQclear(pg_stmt->result);
                    pg_stmt->result = NULL;
                }
            } else {
                pg_stmt->current_row++;
            }

            if (pg_stmt->result) {
                return (pg_stmt->current_row < pg_stmt->num_rows) ? SQLITE_ROW : SQLITE_DONE;
            }
        } else if (pg_stmt->is_pg == 1) {  // WRITE
            // Log INSERT on play_queue_generators for debugging
            if (strstr(pg_stmt->pg_sql, "play_queue_generators")) {
                LOG_INFO("INSERT play_queue_generators on thread %p conn %p",
                        (void*)pthread_self(), (void*)exec_conn);
            }

            PGresult *res = PQexecParams(exec_conn->conn, pg_stmt->pg_sql,
                pg_stmt->param_count, NULL, paramValues, NULL, NULL, 0);

            ExecStatusType status = PQresultStatus(res);
            if (status == PGRES_COMMAND_OK || status == PGRES_TUPLES_OK) {
                exec_conn->last_changes = atoi(PQcmdTuples(res) ?: "1");

                // Extract metadata_id for play_queue_generators (for IN(NULL) fix)
                if (status == PGRES_TUPLES_OK && PQntuples(res) > 0) {
                    const char *id_str = PQgetvalue(res, 0, 0);
                    if (id_str && *id_str) {
                        if (strstr(pg_stmt->pg_sql, "play_queue_generators")) {
                            LOG_INFO("STEP play_queue_generators: RETURNING id = %s on thread %p conn %p",
                                    id_str, (void*)pthread_self(), (void*)exec_conn);
                        }
                        sqlite3_int64 meta_id = extract_metadata_id_from_generator_sql(pg_stmt->sql);
                        if (meta_id > 0) pg_set_global_metadata_id(meta_id);
                    }
                }
            } else {
                LOG_ERROR("STEP PG write error: %s", PQerrorMessage(exec_conn->conn));
            }
            PQclear(res);
        }
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
            if (pg_stmt->param_values[i]) {
                free(pg_stmt->param_values[i]);
                pg_stmt->param_values[i] = NULL;
            }
        }
        pg_stmt_clear_result(pg_stmt);
    }

    // Also clear cached statements - these use a separate registry
    pg_stmt_t *cached = pg_find_cached_stmt(pStmt);
    if (cached) {
        pg_stmt_clear_result(cached);
    }

    return sqlite3_reset(pStmt);
}

static int my_sqlite3_finalize(sqlite3_stmt *pStmt) {
    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt) {
        pg_unregister_stmt(pStmt);
        pg_stmt_free(pg_stmt);
    }
    pg_clear_cached_stmt(pStmt);
    return sqlite3_finalize(pStmt);
}

static int my_sqlite3_clear_bindings(sqlite3_stmt *pStmt) {
    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt) {
        for (int i = 0; i < MAX_PARAMS; i++) {
            if (pg_stmt->param_values[i]) {
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
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result) {
        return pg_stmt->num_cols;
    }
    return sqlite3_column_count(pStmt);
}

static int my_sqlite3_column_type(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result) {
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            LOG_ERROR("COL_TYPE_BOUNDS: idx=%d out of bounds (num_cols=%d) sql=%.100s",
                     idx, pg_stmt->num_cols, pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
            return SQLITE_NULL;
        }
        int row = pg_stmt->current_row;
        if (row < 0 || row >= pg_stmt->num_rows) {
            LOG_ERROR("COL_TYPE_ROW_BOUNDS: row=%d out of bounds (num_rows=%d)", row, pg_stmt->num_rows);
            return SQLITE_NULL;
        }
        if (PQgetisnull(pg_stmt->result, row, idx)) return SQLITE_NULL;
        return pg_oid_to_sqlite_type(PQftype(pg_stmt->result, idx));
    }
    return sqlite3_column_type(pStmt, idx);
}

static int my_sqlite3_column_int(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result) {
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            LOG_ERROR("COL_INT_BOUNDS: idx=%d out of bounds (num_cols=%d)", idx, pg_stmt->num_cols);
            return 0;
        }
        int row = pg_stmt->current_row;
        if (row < 0 || row >= pg_stmt->num_rows) {
            LOG_ERROR("COL_INT_ROW_BOUNDS: row=%d out of bounds (num_rows=%d)", row, pg_stmt->num_rows);
            return 0;
        }

        const char *col_name = PQfname(pg_stmt->result, idx);
        int result_val = 0;

        if (!PQgetisnull(pg_stmt->result, row, idx)) {
            const char *val = PQgetvalue(pg_stmt->result, row, idx);
            if (val[0] == 't' && val[1] == '\0') result_val = 1;
            else if (val[0] == 'f' && val[1] == '\0') result_val = 0;
            else result_val = atoi(val);
        }

        // Log ALL type-related column reads for Movies/TV Shows queries
        if (col_name && (strstr(col_name, "type") || strstr(col_name, "metadata"))) {
            LOG_INFO("COL_INT: row=%d idx=%d name='%s' val=%d sql=%.100s",
                     row, idx, col_name, result_val, pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");

            // Extra detail if metadata_type specifically
            if (strstr(col_name, "metadata_type")) {
                LOG_ERROR("METADATA_TYPE_READ: row=%d idx=%d name='%s' val=%d (expected 1 or 18 for movies)",
                         row, idx, col_name, result_val);
                // Dump first 10 columns to see what's around it
                LOG_ERROR("  Column context (first 10 cols):");
                for (int i = 0; i < 10 && i < pg_stmt->num_cols; i++) {
                    const char *cn = PQfname(pg_stmt->result, i);
                    const char *cv = PQgetisnull(pg_stmt->result, row, i) ? "NULL" : PQgetvalue(pg_stmt->result, row, i);
                    LOG_ERROR("    [%d] %s = %s", i, cn ? cn : "?", cv);
                }
            }
        }

        return result_val;
    }
    return sqlite3_column_int(pStmt, idx);
}

static sqlite3_int64 my_sqlite3_column_int64(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result) {
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            LOG_ERROR("COL_INT64_BOUNDS: idx=%d out of bounds (num_cols=%d)", idx, pg_stmt->num_cols);
            return 0;
        }
        int row = pg_stmt->current_row;
        if (row < 0 || row >= pg_stmt->num_rows) {
            LOG_ERROR("COL_INT64_ROW_BOUNDS: row=%d out of bounds (num_rows=%d)", row, pg_stmt->num_rows);
            return 0;
        }

        const char *col_name = PQfname(pg_stmt->result, idx);
        sqlite3_int64 result_val = 0;

        if (!PQgetisnull(pg_stmt->result, row, idx)) {
            const char *val = PQgetvalue(pg_stmt->result, row, idx);
            if (val[0] == 't' && val[1] == '\0') result_val = 1;
            else if (val[0] == 'f' && val[1] == '\0') result_val = 0;
            else result_val = atoll(val);
        }

        // Log ALL type-related column reads
        if (col_name && (strstr(col_name, "type") || strstr(col_name, "metadata"))) {
            LOG_INFO("COL_INT64: row=%d idx=%d name='%s' val=%lld sql=%.100s",
                     row, idx, col_name, result_val, pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");

            if (strstr(col_name, "metadata_type")) {
                LOG_ERROR("METADATA_TYPE_INT64_READ: row=%d idx=%d name='%s' val=%lld",
                         row, idx, col_name, result_val);
            }
        }

        return result_val;
    }
    return sqlite3_column_int64(pStmt, idx);
}

static double my_sqlite3_column_double(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result) {
        if (idx < 0 || idx >= pg_stmt->num_cols) return 0.0;
        int row = pg_stmt->current_row;
        if (row < 0 || row >= pg_stmt->num_rows) return 0.0;
        if (!PQgetisnull(pg_stmt->result, row, idx)) {
            const char *val = PQgetvalue(pg_stmt->result, row, idx);
            if (val[0] == 't' && val[1] == '\0') return 1.0;
            if (val[0] == 'f' && val[1] == '\0') return 0.0;
            return atof(val);
        }
        return 0.0;
    }
    return sqlite3_column_double(pStmt, idx);
}

static const unsigned char* my_sqlite3_column_text(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result) {
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            LOG_ERROR("COL_TEXT_BOUNDS: idx=%d out of bounds (num_cols=%d) sql=%.100s",
                     idx, pg_stmt->num_cols, pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
            return NULL;
        }
        int row = pg_stmt->current_row;
        if (row < 0 || row >= pg_stmt->num_rows) {
            LOG_ERROR("COL_TEXT_ROW_BOUNDS: row=%d out of bounds (num_rows=%d)", row, pg_stmt->num_rows);
            return NULL;
        }
        if (!PQgetisnull(pg_stmt->result, row, idx)) {
            const unsigned char* value = (const unsigned char*)PQgetvalue(pg_stmt->result, row, idx);

            // DEBUG: Log if we're returning "folder" to help debug metadata_type issue
            if (value && strcmp((const char*)value, "folder") == 0) {
                const char *col_name = PQfname(pg_stmt->result, idx);
                LOG_ERROR("FOLDER_DEBUG: Returning 'folder' for column '%s' (idx=%d, row=%d)",
                         col_name ? col_name : "unknown", idx, row);
                LOG_ERROR("FOLDER_DEBUG: SQL was: %s", pg_stmt->pg_sql ? pg_stmt->pg_sql : "unknown");

                // Log all columns in this row for context
                LOG_ERROR("FOLDER_DEBUG: Row dump:");
                for (int i = 0; i < pg_stmt->num_cols; i++) {
                    const char *cn = PQfname(pg_stmt->result, i);
                    const char *cv = PQgetisnull(pg_stmt->result, row, i) ? "NULL" : PQgetvalue(pg_stmt->result, row, i);
                    LOG_ERROR("  [%d] %s = %s", i, cn ? cn : "?", cv);
                }
            }

            return value;
        }
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

    // Decode hex to binary
    for (size_t i = 0; i < bin_len; i++) {
        unsigned int byte;
        if (sscanf(&hex_str[i * 2], "%2x", &byte) != 1) {
            free(binary);
            *out_length = 0;
            return NULL;
        }
        binary[i] = (unsigned char)byte;
    }

    // Cache the decoded data
    pg_stmt->decoded_blobs[col] = binary;
    pg_stmt->decoded_blob_lens[col] = (int)bin_len;
    *out_length = (int)bin_len;

    return binary;
}

static const void* my_sqlite3_column_blob(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result) {
        if (idx < 0 || idx >= pg_stmt->num_cols) return NULL;
        int row = pg_stmt->current_row;
        if (row < 0 || row >= pg_stmt->num_rows) return NULL;
        if (!PQgetisnull(pg_stmt->result, row, idx)) {
            // Check if this is a BYTEA column (OID 17)
            Oid col_type = PQftype(pg_stmt->result, idx);
            const char *col_name = PQfname(pg_stmt->result, idx);
            LOG_DEBUG("column_blob called: col=%d name=%s type=%d row=%d", idx, col_name ? col_name : "?", col_type, row);
            if (col_type == 17) {  // BYTEA
                int blob_len;
                return pg_decode_bytea(pg_stmt, row, idx, &blob_len);
            }
            return PQgetvalue(pg_stmt->result, row, idx);
        }
        return NULL;
    }
    return sqlite3_column_blob(pStmt, idx);
}

static int my_sqlite3_column_bytes(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result) {
        if (idx < 0 || idx >= pg_stmt->num_cols) return 0;
        int row = pg_stmt->current_row;
        if (row < 0 || row >= pg_stmt->num_rows) return 0;
        if (!PQgetisnull(pg_stmt->result, row, idx)) {
            // Check if this is a BYTEA column (OID 17)
            Oid col_type = PQftype(pg_stmt->result, idx);
            if (col_type == 17) {  // BYTEA
                // Decode the blob (caches it) and return the decoded length
                int blob_len;
                pg_decode_bytea(pg_stmt, row, idx, &blob_len);
                return blob_len;
            }
            return PQgetlength(pg_stmt->result, row, idx);
        }
        return 0;
    }
    return sqlite3_column_bytes(pStmt, idx);
}

static const char* my_sqlite3_column_name(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result) {
        if (idx >= 0 && idx < pg_stmt->num_cols) {
            return PQfname(pg_stmt->result, idx);
        }
    }
    return sqlite3_column_name(pStmt, idx);
}

// sqlite3_column_value returns a pointer to a sqlite3_value for a column.
// For PostgreSQL statements, we return a fake sqlite3_value that encodes the pg_stmt and column.
// The sqlite3_value_* functions will decode this to return proper PostgreSQL data.
static sqlite3_value* my_sqlite3_column_value(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result) {
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            LOG_ERROR("COLUMN_VALUE_BOUNDS: idx=%d out of bounds (num_cols=%d) sql=%.100s",
                     idx, pg_stmt->num_cols, pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
            return NULL;
        }

        // Return a fake value from our pool
        int slot = fake_value_next++ % MAX_FAKE_VALUES;
        pg_fake_value_t *fake = &fake_value_pool[slot];
        fake->magic = PG_FAKE_VALUE_MAGIC;
        fake->pg_stmt = pg_stmt;
        fake->col_idx = idx;
        fake->row_idx = pg_stmt->current_row;

        return (sqlite3_value*)fake;
    }
    return sqlite3_column_value(pStmt, idx);
}

// Intercept sqlite3_value_type to handle our fake values
static int my_sqlite3_value_type(sqlite3_value *pVal) {
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
    if (pg_stmt && pg_stmt->is_pg == 2 && pg_stmt->result) {
        return (pg_stmt->current_row < pg_stmt->num_rows) ? pg_stmt->num_cols : 0;
    }
    return sqlite3_data_count(pStmt);
}

// ============================================================================
// Interposed SQLite Functions - Changes/Last Insert Rowid
// ============================================================================

static int my_sqlite3_changes(sqlite3 *db) {
    pg_connection_t *pg_conn = pg_find_connection(db);
    if (pg_conn && pg_conn->is_pg_active) {
        return pg_conn->last_changes;
    }
    return sqlite3_changes(db);
}

static sqlite3_int64 my_sqlite3_last_insert_rowid(sqlite3 *db) {
    pg_connection_t *pg_conn = pg_find_connection(db);

    // Only use PostgreSQL lastval() if we found the EXACT connection for this db
    // Using a fallback connection would return wrong values from different tables
    if (pg_conn && pg_conn->is_pg_active && pg_conn->conn) {
        PGresult *res = PQexec(pg_conn->conn, "SELECT lastval()");
        if (PQresultStatus(res) == PGRES_TUPLES_OK && PQntuples(res) > 0) {
            sqlite3_int64 rowid = atoll(PQgetvalue(res, 0, 0) ?: "0");
            PQclear(res);
            if (rowid > 0) {
                LOG_INFO("last_insert_rowid: lastval() = %lld on conn %p for db %p",
                        rowid, (void*)pg_conn, (void*)db);
                return rowid;
            }
        } else {
            PQclear(res);
        }
    }

    // Fall back to SQLite - this handles non-redirected databases correctly
    return sqlite3_last_insert_rowid(db);
}

static int my_sqlite3_get_table(sqlite3 *db, const char *sql, char ***pazResult,
                                 int *pnRow, int *pnColumn, char **pzErrMsg) {
    pg_connection_t *pg_conn = pg_find_connection(db);

    if (pg_conn && pg_conn->is_pg_active && pg_conn->conn && is_read_operation(sql)) {
        sql_translation_t trans = sql_translate(sql);
        if (trans.success && trans.sql) {
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
                    sql_translation_free(&trans);
                    return SQLITE_OK;
                }
            }
            PQclear(res);
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
DYLD_INTERPOSE(my_sqlite3_last_insert_rowid, sqlite3_last_insert_rowid)
DYLD_INTERPOSE(my_sqlite3_get_table, sqlite3_get_table)

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

    pg_config_init();
    pg_client_init();
    pg_statement_init();
    sql_translator_init();
    shim_initialized = 1;

    fprintf(stderr, "[SHIM_INIT] All modules initialized\n");
    fflush(stderr);
}

__attribute__((destructor))
static void shim_cleanup(void) {
    LOG_INFO("=== Plex PostgreSQL Interpose Shim unloading ===");
    pg_statement_cleanup();
    pg_client_cleanup();
    sql_translator_cleanup();
    pg_logging_cleanup();
}
