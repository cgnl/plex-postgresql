/*
 * Plex PostgreSQL Interposing Shim - Step/Reset/Finalize Operations
 *
 * Handles sqlite3_step, sqlite3_reset, sqlite3_finalize, sqlite3_clear_bindings.
 * This is the main query execution module.
 */

#include "db_interpose.h"
#include "pg_query_cache.h"

// ============================================================================
// Step Function - Main Query Execution
// ============================================================================

int my_sqlite3_step(sqlite3_stmt *pStmt) {
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

                    // Use prepared statement for better performance (skip parse/plan)
                    uint64_t sql_hash = pg_hash_sql(exec_sql);
                    char stmt_name[32];
                    snprintf(stmt_name, sizeof(stmt_name), "ce_%llx", (unsigned long long)sql_hash);
                    
                    const char *cached_stmt_name = NULL;
                    PGresult *res;
                    if (pg_stmt_cache_lookup(cached_exec_conn, sql_hash, &cached_stmt_name)) {
                        // Cached - execute prepared
                        res = PQexecPrepared(cached_exec_conn->conn, cached_stmt_name, 0, NULL, NULL, NULL, 0);
                    } else {
                        // Not cached - prepare and execute
                        PGresult *prep_res = PQprepare(cached_exec_conn->conn, stmt_name, exec_sql, 0, NULL);
                        if (PQresultStatus(prep_res) == PGRES_COMMAND_OK) {
                            pg_stmt_cache_add(cached_exec_conn, sql_hash, stmt_name, 0);
                            PQclear(prep_res);
                            res = PQexecPrepared(cached_exec_conn->conn, stmt_name, 0, NULL, NULL, NULL, 0);
                        } else {
                            // Prepare failed - fall back to PQexec
                            LOG_DEBUG("CACHED EXEC prepare failed, using PQexec: %s", PQerrorMessage(cached_exec_conn->conn));
                            PQclear(prep_res);
                            res = PQexec(cached_exec_conn->conn, exec_sql);
                        }
                    }
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
                int sqlite_result = orig_sqlite3_step ? orig_sqlite3_step(pStmt) : SQLITE_ERROR;

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

                            // Use prepared statement for better performance (skip parse/plan)
                            uint64_t read_sql_hash = pg_hash_sql(trans.sql);
                            char read_stmt_name[32];
                            snprintf(read_stmt_name, sizeof(read_stmt_name), "cr_%llx", (unsigned long long)read_sql_hash);
                            
                            const char *cached_read_stmt_name = NULL;
                            if (pg_stmt_cache_lookup(cached_read_conn, read_sql_hash, &cached_read_stmt_name)) {
                                // Cached - execute prepared
                                LOG_DEBUG("CACHED READ (prepared): stmt=%s sql=%.60s", cached_read_stmt_name, trans.sql);
                                new_stmt->result = PQexecPrepared(cached_read_conn->conn, cached_read_stmt_name, 0, NULL, NULL, NULL, 0);
                            } else {
                                // Not cached - prepare and execute
                                PGresult *prep_res = PQprepare(cached_read_conn->conn, read_stmt_name, trans.sql, 0, NULL);
                                if (PQresultStatus(prep_res) == PGRES_COMMAND_OK) {
                                    pg_stmt_cache_add(cached_read_conn, read_sql_hash, read_stmt_name, 0);
                                    PQclear(prep_res);
                                    LOG_DEBUG("CACHED READ (new prepared): stmt=%s sql=%.60s", read_stmt_name, trans.sql);
                                    new_stmt->result = PQexecPrepared(cached_read_conn->conn, read_stmt_name, 0, NULL, NULL, NULL, 0);
                                } else {
                                    // Prepare failed - fall back to PQexec
                                    LOG_DEBUG("CACHED READ prepare failed, using PQexec: %s", PQerrorMessage(cached_read_conn->conn));
                                    PQclear(prep_res);
                                    new_stmt->result = PQexec(cached_read_conn->conn, trans.sql);
                                }
                            }
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

            // CRITICAL FIX: Handle cached results FIRST, before checking pg_stmt->result
            // When using cache, result is NULL but cached_result is set.
            // The cache hit (below) sets current_row=0 and returns immediately.
            // On subsequent step() calls, we advance current_row here.
            if (pg_stmt->cached_result) {
                // Advance to next row
                // Cache hit set current_row=0, so second step increments to 1, etc.
                pg_stmt->current_row++;
                if (pg_stmt->current_row >= pg_stmt->num_rows) {
                    // Done with cached result - release ref
                    pg_query_cache_release(pg_stmt->cached_result);
                    pg_stmt->cached_result = NULL;
                    pg_stmt->read_done = 1;
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return SQLITE_DONE;
                }
                pthread_mutex_unlock(&pg_stmt->mutex);
                return SQLITE_ROW;
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
                // ============================================================
                // QUERY RESULT CACHE: Check if we have cached results
                // This is critical for Plex's OnDeck which runs 2000+ identical queries
                // ============================================================
                #if 1  // Re-enabled with proper caching in column_text
                LOG_DEBUG("CACHE_LOOKUP: checking cache for sql=%.60s", pg_stmt->pg_sql ? pg_stmt->pg_sql : "NULL");
                cached_result_t *cached = pg_query_cache_lookup(pg_stmt);
                LOG_DEBUG("CACHE_LOOKUP: result=%p", (void*)cached);
                if (cached) {
                    // Cache hit! Use cached result instead of hitting PostgreSQL
                    // Store cache metadata in pg_stmt for column access
                    pg_stmt->num_rows = cached->num_rows;
                    pg_stmt->num_cols = cached->num_cols;
                    pg_stmt->current_row = 0;
                    pg_stmt->result_conn = NULL;  // No real PGresult

                    // Store pointer to cached result for column_* functions
                    // We'll use a special marker to indicate cached result
                    pg_stmt->cached_result = cached;

                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return (cached->num_rows > 0) ? SQLITE_ROW : SQLITE_DONE;
                }
                #endif

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
                // TEMP DEBUG: Force PQexecParams path to test if prepared statements cause crash
                if (0 && pg_stmt->use_prepared && pg_stmt->stmt_name[0]) {
                    LOG_DEBUG("PREPARED PATH: use_prepared=%d stmt_name=%s sql=%.60s",
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
                        LOG_DEBUG("EXEC_PREPARED: stmt=%s params=%d p0=%s p1=%s",
                                 cached_name, pg_stmt->param_count,
                                 (pg_stmt->param_count > 0 && paramValues[0]) ? paramValues[0] : "NULL",
                                 (pg_stmt->param_count > 1 && paramValues[1]) ? paramValues[1] : "NULL");
                        pg_stmt->result = PQexecPrepared(exec_conn->conn, cached_name,
                            pg_stmt->param_count, paramValues, NULL, NULL, 0);
                        LOG_DEBUG("EXEC_PREPARED DONE: result=%p status=%d",
                                 (void*)pg_stmt->result,
                                 pg_stmt->result ? (int)PQresultStatus(pg_stmt->result) : -1);
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
                LOG_DEBUG("MUTEX_UNLOCKED: checking result status");

                // Check for query errors
                ExecStatusType status = PQresultStatus(pg_stmt->result);
                LOG_DEBUG("RESULT_STATUS: status=%d tuples=%d", (int)status, pg_stmt->result ? PQntuples(pg_stmt->result) : -1);
                if (status != PGRES_TUPLES_OK && status != PGRES_COMMAND_OK) {
                    const char *err = (exec_conn && exec_conn->conn) ? PQerrorMessage(exec_conn->conn) : "NULL connection";
                    LOG_ERROR("PostgreSQL query failed: %s", err);
                    LOG_ERROR("Failed query: %.500s", pg_stmt->pg_sql);
                }
                LOG_DEBUG("RESULT_CHECK DONE");

                if (PQresultStatus(pg_stmt->result) == PGRES_TUPLES_OK) {
                    pg_stmt->num_rows = PQntuples(pg_stmt->result);
                    pg_stmt->num_cols = PQnfields(pg_stmt->result);
                    LOG_DEBUG("TUPLES_OK: rows=%d cols=%d", pg_stmt->num_rows, pg_stmt->num_cols);
                    pg_stmt->current_row = 0;
                    pg_stmt->result_conn = exec_conn;  // Track which connection owns this result

                    // QUERY RESULT CACHE: Store result for potential reuse
                    // pg_query_cache_store(pg_stmt, pg_stmt->result);  // DISABLED
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
                // Subsequent step() call - advance to next row
                pg_stmt->current_row++;
            }

            // NOTE: cached_result handling moved to top of is_pg==2 block
            // to prevent re-entering cache lookup on subsequent step() calls

            // Handle real PGresult
            if (pg_stmt->result) {
                LOG_DEBUG("PG_RESULT: current_row=%d num_rows=%d", pg_stmt->current_row, pg_stmt->num_rows);
                if (pg_stmt->current_row >= pg_stmt->num_rows) {
                    // CRITICAL FIX: Free PGresult immediately when done
                    // Prevents memory accumulation when Plex doesn't call reset()
                    LOG_DEBUG("DONE_PENDING: calling PQclear result=%p", (void*)pg_stmt->result);
                    PQclear(pg_stmt->result);
                    LOG_DEBUG("DONE_PENDING: PQclear complete");
                    pg_stmt->result = NULL;
                    pg_stmt->result_conn = NULL;
                    pg_stmt->read_done = 1;  // Prevent re-execution on next step() call
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    LOG_DEBUG("RETURNING SQLITE_DONE");
                    return SQLITE_DONE;
                }
                pthread_mutex_unlock(&pg_stmt->mutex);
                LOG_DEBUG("RETURNING SQLITE_ROW for stmt=%p pg_stmt=%p thread=%p sql=%.50s",
                          (void*)pStmt, (void*)pg_stmt, (void*)pthread_self(), pg_stmt->sql ? pg_stmt->sql : "NULL");
                // Flush log to ensure it's written before potential crash
                fflush(NULL);
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

    return orig_sqlite3_step ? orig_sqlite3_step(pStmt) : SQLITE_ERROR;
}

// ============================================================================
// Reset/Finalize/Clear Bindings
// ============================================================================

int my_sqlite3_reset(sqlite3_stmt *pStmt) {
    // Clear prepared statements
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt) {
        pthread_mutex_lock(&pg_stmt->mutex);
        for (int i = 0; i < MAX_PARAMS; i++) {
            if (pg_stmt->param_values[i] && !is_preallocated_buffer(pg_stmt, i)) {
                free(pg_stmt->param_values[i]);
                pg_stmt->param_values[i] = NULL;
            }
        }
        pg_stmt_clear_result(pg_stmt);  // This also resets write_executed
        int is_pg_only = (pg_stmt->is_pg == 2);
        pthread_mutex_unlock(&pg_stmt->mutex);

        // If this is a PostgreSQL-only statement (is_pg == 2), don't call real SQLite
        // as the statement handle is not a valid SQLite statement
        if (is_pg_only) {
            return SQLITE_OK;
        }
    }

    // Also clear cached statements - these use a separate registry
    pg_stmt_t *cached = pg_find_cached_stmt(pStmt);
    if (cached) {
        pthread_mutex_lock(&cached->mutex);
        pg_stmt_clear_result(cached);  // This also resets write_executed
        int is_pg_only = (cached->is_pg == 2);
        pthread_mutex_unlock(&cached->mutex);

        // If this is a PostgreSQL-only statement, don't call real SQLite
        if (is_pg_only) {
            return SQLITE_OK;
        }
    }

    return orig_sqlite3_reset ? orig_sqlite3_reset(pStmt) : SQLITE_ERROR;
}

int my_sqlite3_finalize(sqlite3_stmt *pStmt) {
    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    int is_pg_only = 0;

    if (pg_stmt) {
        // Check if this is a PostgreSQL-only statement before cleaning up
        is_pg_only = (pg_stmt->is_pg == 2);

        // Statement is in global registry
        // Check if it's also in TLS cache - if so, need to decrement the TLS reference too
        pg_stmt_t *cached = pg_find_cached_stmt(pStmt);
        if (cached == pg_stmt) {
            // Same statement in both caches - TLS added an extra reference
            // Use normal clear to properly decrement the ref_count
            LOG_DEBUG("finalize: stmt in both global and TLS, clearing TLS ref");
            pg_clear_cached_stmt(pStmt);
        } else if (cached) {
            // Different statement in TLS cache (shouldn't happen, but be defensive)
            LOG_ERROR("finalize: BUG - different pg_stmt in global vs TLS for same sqlite_stmt!");
            pg_clear_cached_stmt(pStmt);
        }

        pg_unregister_stmt(pStmt);
        pg_stmt_unref(pg_stmt);
    } else {
        // Statement might only be in TLS cache - check it too
        pg_stmt_t *cached = pg_find_cached_stmt(pStmt);
        if (cached) {
            is_pg_only = (cached->is_pg == 2);
            // TLS-only statement - but pg_register_cached_stmt incremented ref_count
            // so it's now at 2 instead of 1. Need to unref twice.
            LOG_DEBUG("finalize: stmt only in TLS (ref_count=%d), clearing",
                     atomic_load(&cached->ref_count));
            pg_clear_cached_stmt(pStmt);  // This unrefs once
            pg_stmt_unref(cached);         // Unref again to actually free
        }
    }

    // If this was a PostgreSQL-only statement, don't call real SQLite
    if (is_pg_only) {
        return SQLITE_OK;
    }

    return orig_sqlite3_finalize ? orig_sqlite3_finalize(pStmt) : SQLITE_ERROR;
}

int my_sqlite3_clear_bindings(sqlite3_stmt *pStmt) {
    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt) {
        for (int i = 0; i < MAX_PARAMS; i++) {
            if (pg_stmt->param_values[i] && !is_preallocated_buffer(pg_stmt, i)) {
                free(pg_stmt->param_values[i]);
                pg_stmt->param_values[i] = NULL;
            }
        }
    }
    return orig_sqlite3_clear_bindings ? orig_sqlite3_clear_bindings(pStmt) : SQLITE_ERROR;
}
