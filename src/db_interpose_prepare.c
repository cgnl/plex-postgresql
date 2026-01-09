/*
 * Plex PostgreSQL Interposing Shim - Prepare Operations
 *
 * Handles sqlite3_prepare*, including recursion prevention and stack protection.
 */

// Must be defined before any includes for pthread_getattr_np on Linux/musl
#define _GNU_SOURCE

#include "db_interpose.h"
#include <time.h>

// ============================================================================
// Query Loop Detection
// ============================================================================
// Plex can get into infinite query loops (e.g., OnDeck with many views).
// We detect this by tracking recent query hashes and breaking the loop.

#define LOOP_DETECT_WINDOW_MS 1000   // 1 second window
#define LOOP_DETECT_THRESHOLD 100    // Max same query in window before breaking

typedef struct {
    uint32_t hash;
    uint64_t first_seen_ms;
    int count;
} query_loop_entry_t;

#define LOOP_DETECT_SLOTS 16
static __thread query_loop_entry_t loop_detect[LOOP_DETECT_SLOTS];
static __thread int loop_detect_initialized = 0;

static uint32_t simple_hash(const char *str, int max_len) {
    uint32_t hash = 5381;
    int len = 0;
    while (*str && len < max_len) {
        hash = ((hash << 5) + hash) + (unsigned char)*str++;
        len++;
    }
    return hash;
}

static uint64_t get_time_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

// Returns 1 if query loop detected and should be broken
static int detect_query_loop(const char *sql) {
    if (!sql) return 0;
    
    if (!loop_detect_initialized) {
        memset(loop_detect, 0, sizeof(loop_detect));
        loop_detect_initialized = 1;
    }
    
    uint32_t hash = simple_hash(sql, 200);  // Hash first 200 chars
    uint64_t now = get_time_ms();
    int slot = hash % LOOP_DETECT_SLOTS;
    
    query_loop_entry_t *entry = &loop_detect[slot];
    
    // Check if same query hash
    if (entry->hash == hash) {
        // Check if within time window
        if (now - entry->first_seen_ms < LOOP_DETECT_WINDOW_MS) {
            entry->count++;
            if (entry->count >= LOOP_DETECT_THRESHOLD) {
                // Only log every 10th detection to reduce spam
                static __thread int log_counter = 0;
                if (log_counter++ % 10 == 0) {
                    LOG_ERROR("LOOP DETECTED: query called %d times in %llu ms (logged 1/10)",
                             entry->count, (unsigned long long)(now - entry->first_seen_ms));
                }
                // Reset for next detection
                entry->count = 0;
                entry->first_seen_ms = now;
                // Don't break - Plex crashes on empty results
                // The prepared statement caching makes the queries fast anyway
                return 0;
            }
        } else {
            // Window expired, reset
            entry->first_seen_ms = now;
            entry->count = 1;
        }
    } else {
        // Different query, reset slot
        entry->hash = hash;
        entry->first_seen_ms = now;
        entry->count = 1;
    }
    
    return 0;
}

// ============================================================================
// Helper Functions
// ============================================================================

// Helper to create a simplified SQL for SQLite when query uses FTS
// Removes FTS joins and MATCH clauses since SQLite shadow DB doesn't have FTS tables
char* simplify_fts_for_sqlite(const char *sql) {
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

// ============================================================================
// Internal Prepare Implementation
// ============================================================================

// Internal prepare_v2 implementation - called either directly or from worker thread
// from_worker: 0 = called from Plex's thread, 1 = called from worker with large stack
int my_sqlite3_prepare_v2_internal(sqlite3 *db, const char *zSql, int nByte,
                                   sqlite3_stmt **ppStmt, const char **pzTail,
                                   int from_worker) {
    // LOOP DETECTION: Break infinite query loops (e.g., OnDeck with many views)
    if (zSql && detect_query_loop(zSql)) {
        // Return empty result to break the loop
        if (real_sqlite3_prepare_v2) {
            int rc = real_sqlite3_prepare_v2(db, "SELECT 1 WHERE 0", -1, ppStmt, pzTail);
            return rc;
        }
        if (ppStmt) *ppStmt = NULL;
        if (pzTail) *pzTail = NULL;
        return SQLITE_OK;
    }

    // CRITICAL: Track recursion depth to prevent infinite loops
    // SQLite can internally call prepare_v2 again, creating deep recursion
    prepare_v2_depth++;

    // If recursion is too deep, bail out immediately to prevent stack overflow
    // Normal operations should never recurse more than 5-10 times
    // The crash on 2026-01-06 showed 218 recursive frames!
    // Reduced from 100 to 50: 50 levels × 2KB = 100KB reserved for recursion
    if (prepare_v2_depth > 50) {
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
    void *stack_addr = NULL;
    size_t stack_size = 0;

#ifdef __APPLE__
    // macOS: use non-portable pthread functions
    stack_addr = pthread_get_stackaddr_np(self);
    stack_size = pthread_get_stacksize_np(self);
#else
    // Linux: use pthread_attr_getstack via pthread_getattr_np
    pthread_attr_t attr;
    void *stack_bottom = NULL;
    if (pthread_getattr_np(self, &attr) == 0) {
        pthread_attr_getstack(&attr, &stack_bottom, &stack_size);
        // On Linux, stack_addr is the BOTTOM of the stack
        // Adjust to get the TOP (where stack starts)
        stack_addr = (char*)stack_bottom + stack_size;
        pthread_attr_destroy(&attr);
    }
#endif

    // Calculate stack base and current position
    char *stack_base = (char*)stack_addr;
    volatile char local_var;  // volatile prevents compiler optimization
    char *current_stack = (char*)&local_var;

    // Calculate how much stack we've used
    // Stack grows downward on both macOS/ARM64 and Linux
    ptrdiff_t stack_used = stack_base - current_stack;
    if (stack_used < 0) stack_used = -stack_used;

#ifndef __APPLE__
    // Linux sanity check: verify current_stack is within stack bounds
    if (stack_bottom && stack_addr) {
        if (current_stack < (char*)stack_bottom || current_stack > (char*)stack_addr) {
            LOG_ERROR("STACK CALCULATION ERROR: current=%p not in [%p, %p]",
                     (void*)current_stack, stack_bottom, stack_addr);
            // Fall back to safe defaults - don't trigger protection on bad calculation
            stack_size = 8 * 1024 * 1024;  // Assume 8MB
            stack_used = 0;
        }
    }
#endif

    // Calculate how much stack is left
    ptrdiff_t stack_remaining = (ptrdiff_t)stack_size - stack_used;

    // DEBUG: Log stack info periodically to verify protection is active
    static __thread int stack_log_counter = 0;
    if (++stack_log_counter == 1 || stack_log_counter % 1000 == 0) {
        LOG_INFO("STACK_CHECK: size=%ldKB used=%ldKB remaining=%ldKB (threshold=64KB)",
                 (long)stack_size/1024, (long)stack_used/1024, (long)stack_remaining/1024);
    }

    // WORKER THREAD DELEGATION:
    // Delegate to 8MB stack worker thread when main thread stack is low
    if (!from_worker && stack_remaining < WORKER_DELEGATION_THRESHOLD && worker_running) {
        LOG_DEBUG("WORKER DELEGATION: stack_remaining=%ld bytes < %d, delegating to 8MB worker",
                 (long)stack_remaining, WORKER_DELEGATION_THRESHOLD);
        prepare_v2_depth--;  // Worker will increment again
        return delegate_prepare_to_worker(db, zSql, nByte, ppStmt, pzTail);
    }

    // CRITICAL FIX: OnDeck queries with low stack cause Plex to crash AFTER query completes
    // When stack < 100KB, Plex's Metal initialization (for thumbnails) crashes in dyld
    // OnDeck queries are identified by their SQL pattern, not URL parameters
    // Must check BEFORE the 8KB threshold since crash happens with ~50KB remaining
    int is_ondeck_query = zSql && (
        (strcasestr(zSql, "metadata_item_settings") && strcasestr(zSql, "metadata_items")) ||
        (strcasestr(zSql, "metadata_item_views") && strcasestr(zSql, "grandparents")) ||
        strcasestr(zSql, "grandparentsSettings")
    );

    // For OnDeck queries with low stack, use PostgreSQL path with minimal stack
    // This avoids the "return empty" workaround that breaks functionality
    if (is_ondeck_query && stack_remaining < 100000) {
        LOG_INFO("STACK LOW OnDeck: %ld bytes remaining - using PG fast path",
                 (long)stack_remaining);
        
        pg_connection_t *pg_conn = pg_find_connection(db);
        if (pg_conn && pg_conn->is_pg_active && pg_conn->conn) {
            // Prepare minimal SQLite statement, route to PostgreSQL
            int rc;
            if (real_sqlite3_prepare_v2) {
                rc = real_sqlite3_prepare_v2(db, "SELECT 1", -1, ppStmt, pzTail);
            } else {
                rc = SQLITE_ERROR;
                if (ppStmt) *ppStmt = NULL;
            }
            
            if (rc == SQLITE_OK && *ppStmt) {
                pg_stmt_t *pg_stmt = pg_stmt_create(pg_conn, zSql, *ppStmt);
                if (pg_stmt) {
                    pg_stmt->is_pg = 2;  // read operation
                    
                    // Translate query for PostgreSQL
                    sql_translation_t trans = sql_translate(zSql);
                    if (trans.success && trans.sql) {
                        pg_stmt->pg_sql = strdup(trans.sql);
                        pg_stmt->param_count = trans.param_count;
                        LOG_INFO("STACK LOW OnDeck: routed to PG: %.100s", trans.sql);
                    }
                    sql_translation_free(&trans);
                }
            }
            prepare_v2_depth--;
            return rc;
        }
        
        // No PG connection - fall back to empty result
        LOG_ERROR("STACK CRITICAL OnDeck: no PG connection, returning empty");
        int rc;
        if (real_sqlite3_prepare_v2) {
            rc = real_sqlite3_prepare_v2(db, "SELECT 1 WHERE 0", -1, ppStmt, pzTail);
        } else {
            rc = SQLITE_ERROR;
            if (ppStmt) *ppStmt = NULL;
        }
        prepare_v2_depth--;
        return rc;
    }

    // Hard stack threshold - increased from 8KB to 64KB for safety
    // 64KB gives SQLite enough room for simple queries without crashing
    int stack_threshold = from_worker ? 32000 : 64000;

    if (stack_remaining < stack_threshold) {
        // For PostgreSQL-destined read queries, use a minimal SQLite query
        // to get a valid statement handle, then route execution to PostgreSQL
        pg_connection_t *pg_conn_check = pg_find_connection(db);
        int is_pg_read = pg_conn_check && pg_conn_check->is_pg_active &&
                         pg_conn_check->conn && zSql && is_read_operation(zSql);

        if (is_pg_read) {
            LOG_INFO("STACK LOW (%ld bytes) but using PG path for: %.100s",
                     (long)stack_remaining, zSql);

            // Prepare a minimal "SELECT 1" to get a valid statement handle
            // The actual query will be executed by PostgreSQL in step()
            int rc;
            if (real_sqlite3_prepare_v2) {
                rc = real_sqlite3_prepare_v2(db, "SELECT 1", -1, ppStmt, pzTail);
            } else {
                rc = SQLITE_ERROR;
            }

            if (rc == SQLITE_OK && *ppStmt) {
                // Create PG statement with the REAL query
                pg_stmt_t *pg_stmt = pg_stmt_create(pg_conn_check, zSql, *ppStmt);
                if (pg_stmt) {
                    pg_stmt->is_pg = 2;  // read operation

                    // Translate the query
                    sql_translation_t trans = sql_translate(zSql);
                    if (trans.success && trans.sql) {
                        pg_stmt->pg_sql = strdup(trans.sql);
                        pg_stmt->param_count = trans.param_count;

                        // Store parameter names
                        if (trans.param_names && trans.param_count > 0) {
                            pg_stmt->param_names = malloc(trans.param_count * sizeof(char*));
                            if (pg_stmt->param_names) {
                                for (int i = 0; i < trans.param_count; i++) {
                                    pg_stmt->param_names[i] = trans.param_names[i] ?
                                                              strdup(trans.param_names[i]) : NULL;
                                }
                            }
                        }

                        // Set up prepared statement caching
                        if (pg_stmt->pg_sql) {
                            pg_stmt->sql_hash = pg_hash_sql(pg_stmt->pg_sql);
                            snprintf(pg_stmt->stmt_name, sizeof(pg_stmt->stmt_name),
                                     "ps_%llx", (unsigned long long)pg_stmt->sql_hash);
                            pg_stmt->use_prepared = 1;
                        }
                    }
                    sql_translation_free(&trans);
                    pg_register_stmt(*ppStmt, pg_stmt);
                }
            }

            prepare_v2_depth--;
            return rc;
        }

        // For non-PG queries or writes, reject with error
        LOG_ERROR("STACK PROTECTION TRIGGERED: stack_used=%ld/%ld bytes, remaining=%ld bytes",
                 (long)stack_used, (long)stack_size, (long)stack_remaining);
        LOG_ERROR("  Query rejected (not a PG read): %.200s", zSql ? zSql : "NULL");

        pg_connection_t *pg_conn = pg_find_connection(db);
        if (pg_conn) {
            pg_conn->last_error_code = SQLITE_NOMEM;
            snprintf(pg_conn->last_error, sizeof(pg_conn->last_error),
                     "Stack protection: insufficient stack space (remaining=%ld).",
                     (long)stack_remaining);
        }

        prepare_v2_depth--;
        if (ppStmt) *ppStmt = NULL;
        if (pzTail) *pzTail = NULL;
        return SQLITE_NOMEM;
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
        LOG_DEBUG("BACKTICK_QUERY: skip_complex=%d len=%d sql=%.200s",
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
            LOG_INFO("FTS query ORIGINAL: %.500s", zSql);
            LOG_INFO("FTS query SIMPLIFIED: %.500s", cleaned_sql);
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

    // CRITICAL FIX: If query still contains FTS after simplification, use empty result
    // SQLite's FTS virtual tables use ICU tokenizer which isn't available
    // This causes "unknown tokenizer: collating" error
    if (strcasestr(sql_for_sqlite, "fts4_") || strcasestr(sql_for_sqlite, " match ")) {
        LOG_INFO("FTS query blocked from SQLite (tokenizer not available): %.100s", sql_for_sqlite);
        if (real_sqlite3_prepare_v2) {
            int rc = real_sqlite3_prepare_v2(db, "SELECT 1 WHERE 0", -1, ppStmt, pzTail);
            if (cleaned_sql) free(cleaned_sql);
            prepare_v2_depth--;
            return rc;
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

// ============================================================================
// Public Prepare Functions
// ============================================================================

// Public wrapper - delegates to worker thread if stack is low
int my_sqlite3_prepare_v2(sqlite3 *db, const char *zSql, int nByte,
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

int my_sqlite3_prepare(sqlite3 *db, const char *zSql, int nByte,
                       sqlite3_stmt **ppStmt, const char **pzTail) {
    // Route through my_sqlite3_prepare_v2 to get icu_root cleanup and PG handling
    return my_sqlite3_prepare_v2(db, zSql, nByte, ppStmt, pzTail);
}

int my_sqlite3_prepare16_v2(sqlite3 *db, const void *zSql, int nByte,
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

int my_sqlite3_prepare_v3(sqlite3 *db, const char *zSql, int nByte,
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
