/*
 * PostgreSQL Shim - Type Definitions
 * Central type definitions shared across all modules
 */

#ifndef PG_TYPES_H
#define PG_TYPES_H

#include <sqlite3.h>
#include <libpq-fe.h>
#include <pthread.h>

// ============================================================================
// Constants
// ============================================================================

#define MAX_CONNECTIONS 512
#define MAX_PARAMS 64
#define MAX_STATEMENTS 1024
#define MAX_CACHED_STMTS_PER_THREAD 64
#define PG_VALUE_MAGIC 0x50475641  // "PGVA" - identifies our fake sqlite3_value

// Log file path
#define LOG_FILE "/tmp/plex_redirect_pg.log"
#define FALLBACK_LOG_FILE "/tmp/plex_pg_fallbacks.log"

// Environment variables
#define ENV_PG_HOST     "PLEX_PG_HOST"
#define ENV_PG_PORT     "PLEX_PG_PORT"
#define ENV_PG_DATABASE "PLEX_PG_DATABASE"
#define ENV_PG_USER     "PLEX_PG_USER"
#define ENV_PG_PASSWORD "PLEX_PG_PASSWORD"
#define ENV_PG_SCHEMA   "PLEX_PG_SCHEMA"
#define ENV_PG_LOG_LEVEL "PLEX_PG_LOG_LEVEL"
#define ENV_PG_LOG_FILE  "PLEX_PG_LOG_FILE"

// PostgreSQL-only mode flag
#define PG_READ_ENABLED 1

// ============================================================================
// Configuration Structure
// ============================================================================

typedef struct {
    char host[256];
    int port;
    char database[128];
    char user[128];
    char password[256];
    char schema[64];
} pg_conn_config_t;

// ============================================================================
// Connection Structure
// ============================================================================

typedef struct pg_connection {
    PGconn *conn;
    sqlite3 *shadow_db;              // Real SQLite handle for fallback/mapping
    char db_path[1024];
    int is_pg_active;                // Whether we're using PostgreSQL
    int in_transaction;
    pthread_mutex_t mutex;
    int last_changes;                // Track rows affected by last write
    sqlite3_int64 last_insert_rowid; // Track last inserted row ID
    sqlite3_int64 last_generator_metadata_id;  // Track metadata_item_id from generator URI
    char last_error[1024];           // Track last PostgreSQL error message
    int last_error_code;             // Track last SQLite-style error code
} pg_connection_t;

// ============================================================================
// Statement Structure
// ============================================================================

typedef struct pg_stmt {
    pg_connection_t *conn;
    sqlite3_stmt *shadow_stmt;       // Real SQLite statement handle (for mapping)
    char *sql;                       // Original SQL
    char *pg_sql;                    // Translated PostgreSQL SQL
    PGresult *result;
    int current_row;
    int num_rows;
    int num_cols;
    int is_pg;                       // 0=skip, 1=write, 2=read, 3=no-op
    int is_cached;                   // 1 if from TLS (cached stmt)
    int needs_requery;               // 1 if reset() was called
    int write_executed;              // 1 if write has been executed (prevents duplicate execution)

    char *param_values[MAX_PARAMS];
    int param_lengths[MAX_PARAMS];
    int param_formats[MAX_PARAMS];   // 0 = text, 1 = binary
    int param_count;

    // Decoded BYTEA blob cache (per-row, freed on step/reset)
    void *decoded_blobs[MAX_PARAMS]; // Decoded binary data per column
    int decoded_blob_lens[MAX_PARAMS]; // Length of decoded data per column
    int decoded_blob_row;            // Row for which blobs are cached (-1 = none)
} pg_stmt_t;

// ============================================================================
// Thread-Local Storage Structures
// ============================================================================

typedef struct {
    sqlite3_stmt *sqlite_stmt;
    pg_stmt_t *pg_stmt;
} cached_stmt_entry_t;

typedef struct {
    cached_stmt_entry_t entries[MAX_CACHED_STMTS_PER_THREAD];
    int count;
} thread_cached_stmts_t;

// ============================================================================
// Fake sqlite3_value for PostgreSQL columns
// ============================================================================

typedef struct pg_value {
    unsigned int magic;              // PG_VALUE_MAGIC to identify our values
    pg_stmt_t *stmt;                 // Parent statement
    int col_idx;                     // Column index
    int type;                        // SQLite type (SQLITE_INTEGER, etc)
} pg_value_t;

// ============================================================================
// DYLD Interpose Macro (macOS only)
// ============================================================================

#ifdef __APPLE__
#define DYLD_INTERPOSE(_replacement, _original) \
    __attribute__((used)) static struct { \
        const void* replacement; \
        const void* original; \
    } _interpose_##_original __attribute__((section("__DATA,__interpose"))) = { \
        (const void*)(unsigned long)&_replacement, \
        (const void*)(unsigned long)&_original \
    };
#endif

#endif // PG_TYPES_H
