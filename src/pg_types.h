#ifndef PG_TYPES_H
#define PG_TYPES_H

#include <sqlite3.h>
#include <libpq-fe.h>
#include <pthread.h>

#define MAX_CONNECTIONS 16
#define MAX_PARAMS 64
#define MAX_CACHED_STMTS_PER_THREAD 64
#define PG_VALUE_MAGIC 0x50475641  // "PGVA"

// Configuration structure
typedef struct {
    char host[256];
    int port;
    char database[128];
    char user[128];
    char password[256];
    char schema[64];
} pg_conn_config_t;

// Connection structure
typedef struct pg_connection {
    PGconn *conn;
    
    // SQLite handle used as a key to map app's handle to this connection
    sqlite3 *sqlite_handle; 
    
    char db_path[1024];
    int is_pg_active;    // Whether we're using PostgreSQL
    int in_transaction;
    pthread_mutex_t mutex;
    int last_changes;           // Track rows affected by last write
    sqlite3_int64 last_insert_rowid;  // Track last inserted row ID
    sqlite3_int64 last_generator_metadata_id;  // Track metadata_item_id from last generator URI
    char last_error[1024];      // Track last PostgreSQL error message
    int last_error_code;        // Track last SQLite-style error code
} pg_connection_t;

// Statement structure
typedef struct pg_stmt {
    pg_connection_t *conn;
    
    // SQLite statement handle used as key (if needed, though mainly stmt_map handles this)
    // We don't strictly need it inside here if we have stmt_map, checking db_interpose_pg.c usages...
    // It was used for fallback. 
    sqlite3_stmt *sqlite_stmt_handle; 

    char *sql;
    char *pg_sql;
    PGresult *result;
    int current_row;
    int num_rows;
    int num_cols;
    int is_pg;
    int is_cached;  // 1 if this is from TLS (cached stmt), uses expanded_sql
    int needs_requery;  // 1 if reset() was called and we need to execute a new query

    char *param_values[MAX_PARAMS];
    int param_lengths[MAX_PARAMS];
    int param_formats[MAX_PARAMS]; // 0 = text, 1 = binary
    int param_count;
} pg_stmt_t;

// Fake sqlite3_value for PostgreSQL columns
typedef struct pg_value {
    unsigned int magic;      // PG_VALUE_MAGIC to identify our values
    pg_stmt_t *stmt;         // Parent statement
    int col_idx;             // Column index
    int type;                // SQLite type (SQLITE_INTEGER, etc)
} pg_value_t;

// Thread-local storage structures
typedef struct {
    sqlite3_stmt *sqlite_stmt;
    pg_stmt_t *pg_stmt;
} cached_stmt_entry_t;

typedef struct {
    cached_stmt_entry_t entries[MAX_CACHED_STMTS_PER_THREAD];
    int count;
} thread_cached_stmts_t;

#endif // PG_TYPES_H
