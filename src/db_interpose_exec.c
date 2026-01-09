/*
 * Plex PostgreSQL Interposing Shim - Exec Operations
 *
 * Handles sqlite3_exec function interposition.
 */

#include "db_interpose.h"

// ============================================================================
// Exec Function
// ============================================================================

int my_sqlite3_exec(sqlite3 *db, const char *sql,
                    int (*callback)(void*, int, char**, char**),
                    void *arg, char **errmsg) {
    // CRITICAL FIX: NULL check to prevent crash in strcasestr
    if (!sql) {
        LOG_ERROR("exec called with NULL SQL");
        return orig_sqlite3_exec ? orig_sqlite3_exec(db, sql, callback, arg, errmsg) : SQLITE_ERROR;
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
                
                // Use prepared statement for better performance (skip parse/plan)
                uint64_t sql_hash = pg_hash_sql(exec_sql);
                char stmt_name[32];
                snprintf(stmt_name, sizeof(stmt_name), "ex_%llx", (unsigned long long)sql_hash);
                
                const char *cached_stmt_name = NULL;
                PGresult *res;
                if (pg_stmt_cache_lookup(pg_conn, sql_hash, &cached_stmt_name)) {
                    // Cached - execute prepared
                    res = PQexecPrepared(pg_conn->conn, cached_stmt_name, 0, NULL, NULL, NULL, 0);
                } else {
                    // Not cached - prepare and execute
                    PGresult *prep_res = PQprepare(pg_conn->conn, stmt_name, exec_sql, 0, NULL);
                    if (PQresultStatus(prep_res) == PGRES_COMMAND_OK) {
                        pg_stmt_cache_add(pg_conn, sql_hash, stmt_name, 0);
                        PQclear(prep_res);
                        res = PQexecPrepared(pg_conn->conn, stmt_name, 0, NULL, NULL, NULL, 0);
                    } else {
                        // Prepare failed - fall back to PQexec
                        LOG_DEBUG("EXEC prepare failed, using PQexec: %s", PQerrorMessage(pg_conn->conn));
                        PQclear(prep_res);
                        res = PQexec(pg_conn->conn, exec_sql);
                    }
                }
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

    int rc = orig_sqlite3_exec ? orig_sqlite3_exec(db, exec_sql, callback, arg, errmsg) : SQLITE_ERROR;
    if (cleaned_sql) free(cleaned_sql);
    return rc;
}
