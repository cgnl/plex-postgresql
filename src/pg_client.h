/*
 * PostgreSQL Shim - Client/Connection Module
 * Connection management and registry
 */

#ifndef PG_CLIENT_H
#define PG_CLIENT_H

#include "pg_types.h"

// Initialize/cleanup client module
void pg_client_init(void);
void pg_client_cleanup(void);

// Connection lifecycle
pg_connection_t* pg_connect(const char *db_path, sqlite3 *shadow_db);
void pg_close(pg_connection_t *conn);
int pg_ensure_connection(pg_connection_t *conn);

// Connection registry (maps sqlite3* -> pg_connection_t*)
void pg_register_connection(pg_connection_t *conn);
void pg_unregister_connection(pg_connection_t *conn);
pg_connection_t* pg_find_connection(sqlite3 *db);
pg_connection_t* pg_find_any_library_connection(void);

// Thread-local connection (one PG connection per thread for library.db)
pg_connection_t* pg_get_thread_connection(const char *db_path);

// Update last_used timestamp to prevent pool slot from being released
// CRITICAL: Call during long-running operations to keep connection alive
void pg_pool_touch_connection(pg_connection_t *conn);

// Check connection health after query error, reset if corrupted
// Call this after any query that returns an unexpected error
// Returns 1 if connection was reset, 0 if still healthy
int pg_pool_check_connection_health(pg_connection_t *conn);

// Close pool connection for a database handle (called on sqlite3_close)
void pg_close_pool_for_db(sqlite3 *db);

// Global state
sqlite3_int64 pg_get_global_metadata_id(void);
void pg_set_global_metadata_id(sqlite3_int64 id);
sqlite3_int64 pg_get_global_last_insert_rowid(void);
void pg_set_global_last_insert_rowid(sqlite3_int64 id);

// Prepared statement cache management
uint64_t pg_hash_sql(const char *sql);
int pg_stmt_cache_lookup(pg_connection_t *conn, uint64_t sql_hash, const char **stmt_name);
int pg_stmt_cache_add(pg_connection_t *conn, uint64_t sql_hash, const char *stmt_name, int param_count);
void pg_stmt_cache_clear(pg_connection_t *conn);

#endif // PG_CLIENT_H
