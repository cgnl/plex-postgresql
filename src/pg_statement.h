/*
 * PostgreSQL Shim - Statement Module
 * Statement tracking, TLS caching, and helper functions
 */

#ifndef PG_STATEMENT_H
#define PG_STATEMENT_H

#include "pg_types.h"

// Initialize/cleanup statement module
void pg_statement_init(void);
void pg_statement_cleanup(void);

// Statement registry (maps sqlite3_stmt* -> pg_stmt_t*)
void pg_register_stmt(sqlite3_stmt *sqlite_stmt, pg_stmt_t *pg_stmt);
void pg_unregister_stmt(sqlite3_stmt *sqlite_stmt);
pg_stmt_t* pg_find_stmt(sqlite3_stmt *stmt);
pg_stmt_t* pg_find_any_stmt(sqlite3_stmt *stmt);
int pg_is_our_stmt(void *ptr);

// TLS cached statement management
void pg_register_cached_stmt(sqlite3_stmt *sqlite_stmt, pg_stmt_t *pg_stmt);
pg_stmt_t* pg_find_cached_stmt(sqlite3_stmt *sqlite_stmt);
void pg_clear_cached_stmt(sqlite3_stmt *sqlite_stmt);      // For TLS destructor (unrefs)
void pg_clear_cached_stmt_weak(sqlite3_stmt *sqlite_stmt); // For finalize (no unref)

// Statement creation/destruction
pg_stmt_t* pg_stmt_create(pg_connection_t *conn, const char *sql, sqlite3_stmt *shadow_stmt);
void pg_stmt_free(pg_stmt_t *stmt);  // Internal - prefer pg_stmt_unref
void pg_stmt_ref(pg_stmt_t *stmt);   // CRITICAL FIX: Increment reference count
void pg_stmt_unref(pg_stmt_t *stmt); // CRITICAL FIX: Decrement ref count, free if 0
void pg_stmt_clear_result(pg_stmt_t *stmt);

// Helpers for SQL transformation
char* convert_metadata_settings_insert_to_upsert(const char *sql);
sqlite3_int64 extract_metadata_id_from_generator_sql(const char *sql);

// Fake sqlite3_value helpers
sqlite3_value* pg_create_column_value(pg_stmt_t *stmt, int col_idx);
int pg_is_our_value(sqlite3_value *val);
int pg_oid_to_sqlite_type(Oid oid);
const char* pg_oid_to_sqlite_decltype(Oid oid);

#endif // PG_STATEMENT_H
