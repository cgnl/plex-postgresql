/*
 * PostgreSQL Shim - Configuration Module
 * Configuration loading and SQL classification
 */

#include "pg_config.h"
#include "pg_logging.h"
#include "sql_translator_internal.h"  // for safe_strcasestr
#include <stdlib.h>
#include <string.h>
#include <ctype.h>

// ============================================================================
// Static State
// ============================================================================

static pg_conn_config_t pg_config;
static int config_loaded = 0;

// Database files to redirect to PostgreSQL
static const char *REDIRECT_PATTERNS[] = {
    "com.plexapp.plugins.library.db",
    "com.plexapp.plugins.library.blobs.db",
    NULL
};

// SQLite-specific commands to skip (no-op, return success)
// Pre-computed lengths to avoid strlen() in hot path
static const struct { const char *pattern; size_t len; } SQLITE_SKIP_PATTERNS[] = {
    {"icu_load_collation", 18},
    {"fts3_tokenizer", 14},
    {"SELECT load_extension", 21},
    {"VACUUM", 6},
    {"PRAGMA", 6},
    {"REINDEX", 7},
    {"ANALYZE sqlite_", 15},
    {"ATTACH DATABASE", 15},
    {"DETACH DATABASE", 15},
    // Transactions skipped - pool doesn't track connection-per-transaction
    {"BEGIN", 5},
    {"COMMIT", 6},
    {"ROLLBACK", 8},
    {"SAVEPOINT", 9},
    {"RELEASE SAVEPOINT", 17},
    {NULL, 0}
};

// Patterns that can appear anywhere in SQL (should skip)
static const char *ANYWHERE_SKIP_PATTERNS[] = {
    "sqlite_schema",
    "sqlite_master",
    "fts3_tokenizer",
    // "fts4",  -- Enable FTS translation
    // "fts5",  -- Enable FTS translation
    "spellfix",
    "icu_load_collation",
    // typeof() and last_insert_rowid() are translated by sql_translator, not skipped
    // Dynamic column update (no-op: SET $col=$col)
    "SET $2=$2",
    "SET $1=$1",
    NULL
};

// ============================================================================
// Configuration Loading
// ============================================================================

void pg_config_init(void) {
    if (config_loaded) return;

    const char *val;

    val = getenv(ENV_PG_HOST);
    strncpy(pg_config.host, val ? val : "localhost", sizeof(pg_config.host) - 1);

    val = getenv(ENV_PG_PORT);
    pg_config.port = val ? atoi(val) : 5432;

    val = getenv(ENV_PG_DATABASE);
    strncpy(pg_config.database, val ? val : "plex", sizeof(pg_config.database) - 1);

    val = getenv(ENV_PG_USER);
    strncpy(pg_config.user, val ? val : "plex", sizeof(pg_config.user) - 1);

    val = getenv(ENV_PG_PASSWORD);
    strncpy(pg_config.password, val ? val : "", sizeof(pg_config.password) - 1);

    val = getenv(ENV_PG_SCHEMA);
    strncpy(pg_config.schema, val ? val : "plex", sizeof(pg_config.schema) - 1);

    config_loaded = 1;

    LOG_INFO("PostgreSQL config: %s@%s:%d/%s (schema: %s)",
             pg_config.user, pg_config.host, pg_config.port,
             pg_config.database, pg_config.schema);
}

pg_conn_config_t* pg_config_get(void) {
    if (!config_loaded) pg_config_init();
    return &pg_config;
}

// ============================================================================
// SQL Classification
// ============================================================================

int should_redirect(const char *filename) {
    if (!filename) return 0;

    for (int i = 0; REDIRECT_PATTERNS[i]; i++) {
        if (strstr(filename, REDIRECT_PATTERNS[i])) {
            return 1;
        }
    }
    return 0;
}

int should_skip_sql(const char *sql) {
    if (!sql) return 0;

    // Skip whitespace at start
    while (*sql && (*sql == ' ' || *sql == '\t' || *sql == '\n')) sql++;

    // Check patterns that should match at start of SQL
    for (int i = 0; SQLITE_SKIP_PATTERNS[i].pattern; i++) {
        if (strncasecmp(sql, SQLITE_SKIP_PATTERNS[i].pattern, SQLITE_SKIP_PATTERNS[i].len) == 0) {
            return 1;
        }
    }

    // Check patterns that can appear anywhere
    for (int i = 0; ANYWHERE_SKIP_PATTERNS[i]; i++) {
        if (strcasestr(sql, ANYWHERE_SKIP_PATTERNS[i])) {
            return 1;
        }
    }

    return 0;
}

int is_write_operation(const char *sql) {
    if (!sql) return 0;

    // Skip whitespace
    while (*sql && isspace(*sql)) sql++;

    if (strncasecmp(sql, "INSERT", 6) == 0) return 1;
    if (strncasecmp(sql, "UPDATE", 6) == 0) return 1;
    if (strncasecmp(sql, "DELETE", 6) == 0) return 1;
    if (strncasecmp(sql, "REPLACE", 7) == 0) return 1;

    return 0;
}

int is_read_operation(const char *sql) {
    if (!sql) return 0;

    // Skip whitespace
    while (*sql && isspace(*sql)) sql++;

    if (strncasecmp(sql, "SELECT", 6) == 0) return 1;

    return 0;
}
