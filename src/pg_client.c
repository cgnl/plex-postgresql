/*
 * PostgreSQL Shim - Client/Connection Module
 * Connection management with pooling
 */

#include "pg_client.h"
#include "pg_config.h"
#include "pg_logging.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

// ============================================================================
// Connection Pool Configuration
// ============================================================================

#define POOL_SIZE 30  // Max connections in pool

typedef struct {
    pg_connection_t *conn;
    pthread_t owner_thread;      // Thread that owns this connection (0 = free)
    time_t last_used;            // Last time this connection was used
    int in_use;                  // Currently being used in a query
} pool_slot_t;

// ============================================================================
// Static State
// ============================================================================

static pg_connection_t *connections[MAX_CONNECTIONS];
static pthread_mutex_t connections_mutex = PTHREAD_MUTEX_INITIALIZER;
static int client_initialized = 0;

// Connection pool for library.db
static pool_slot_t library_pool[POOL_SIZE];
static pthread_mutex_t pool_mutex = PTHREAD_MUTEX_INITIALIZER;
static int pool_initialized = 0;
static char library_db_path[512] = {0};

// Connection idle timeout (seconds) - close connections idle longer than this
#define POOL_IDLE_TIMEOUT 60

// Track mapping from sqlite3* handles to pool slots for cleanup on close
static struct {
    sqlite3 *db;
    int pool_slot;
} db_to_pool[MAX_CONNECTIONS];
static int db_to_pool_count = 0;

// Global metadata ID for play_queue_generators workaround
static sqlite3_int64 global_last_metadata_id = 0;
static pthread_mutex_t metadata_id_mutex = PTHREAD_MUTEX_INITIALIZER;

// Global last_insert_rowid shared across all connections (fixes multi-connection issues)
static sqlite3_int64 global_last_insert_rowid = 0;
static pthread_mutex_t global_rowid_mutex = PTHREAD_MUTEX_INITIALIZER;

// Forward declarations
static int is_library_db(const char *path);
static pg_connection_t* pool_get_connection(const char *db_path);

// ============================================================================
// Initialization
// ============================================================================

void pg_client_init(void) {
    if (client_initialized) return;
    memset(connections, 0, sizeof(connections));
    memset(library_pool, 0, sizeof(library_pool));
    pool_initialized = 0;
    client_initialized = 1;
    LOG_DEBUG("pg_client initialized with pool size %d", POOL_SIZE);
}

void pg_client_cleanup(void) {
    pthread_mutex_lock(&connections_mutex);
    for (int i = 0; i < MAX_CONNECTIONS; i++) {
        if (connections[i]) {
            if (connections[i]->conn) {
                PQfinish(connections[i]->conn);
            }
            pthread_mutex_destroy(&connections[i]->mutex);
            free(connections[i]);
            connections[i] = NULL;
        }
    }
    pthread_mutex_unlock(&connections_mutex);

    // Clean up pool
    pthread_mutex_lock(&pool_mutex);
    for (int i = 0; i < POOL_SIZE; i++) {
        if (library_pool[i].conn) {
            if (library_pool[i].conn->conn) {
                LOG_INFO("Cleanup: closing pool connection %d (thread %p)",
                        i, (void*)library_pool[i].owner_thread);
                PQfinish(library_pool[i].conn->conn);
            }
            pthread_mutex_destroy(&library_pool[i].conn->mutex);
            free(library_pool[i].conn);
            library_pool[i].conn = NULL;
        }
    }
    pool_initialized = 0;
    db_to_pool_count = 0;
    pthread_mutex_unlock(&pool_mutex);

    client_initialized = 0;
}

// ============================================================================
// Connection Registry
// ============================================================================

void pg_register_connection(pg_connection_t *conn) {
    if (!conn) return;

    pthread_mutex_lock(&connections_mutex);
    for (int i = 0; i < MAX_CONNECTIONS; i++) {
        if (connections[i] == NULL) {
            connections[i] = conn;
            LOG_DEBUG("Registered connection %p at slot %d", (void*)conn, i);
            pthread_mutex_unlock(&connections_mutex);
            return;
        }
    }
    pthread_mutex_unlock(&connections_mutex);
    LOG_ERROR("Connection registry full! MAX_CONNECTIONS=%d", MAX_CONNECTIONS);
}

void pg_unregister_connection(pg_connection_t *conn) {
    if (!conn) return;

    pthread_mutex_lock(&connections_mutex);
    for (int i = 0; i < MAX_CONNECTIONS; i++) {
        if (connections[i] == conn) {
            connections[i] = NULL;
            LOG_DEBUG("Unregistered connection %p from slot %d", (void*)conn, i);
            break;
        }
    }
    pthread_mutex_unlock(&connections_mutex);
}

pg_connection_t* pg_find_connection(sqlite3 *db) {
    if (!db) return NULL;

    // First, check the per-handle registry
    pthread_mutex_lock(&connections_mutex);
    for (int i = 0; i < MAX_CONNECTIONS; i++) {
        if (connections[i] && connections[i]->shadow_db == db) {
            pg_connection_t *handle_conn = connections[i];
            pthread_mutex_unlock(&connections_mutex);

            // For library.db, use pooled connection instead
            if (is_library_db(handle_conn->db_path)) {
                pg_connection_t *pool_conn = pool_get_connection(handle_conn->db_path);
                if (pool_conn && pool_conn->is_pg_active) {
                    // Track this db->pool mapping for cleanup on close
                    pthread_mutex_lock(&pool_mutex);
                    int found = 0;
                    for (int j = 0; j < db_to_pool_count; j++) {
                        if (db_to_pool[j].db == db) {
                            found = 1;
                            break;
                        }
                    }
                    if (!found && db_to_pool_count < MAX_CONNECTIONS) {
                        // Find which pool slot we're using
                        for (int j = 0; j < POOL_SIZE; j++) {
                            if (library_pool[j].conn == pool_conn) {
                                db_to_pool[db_to_pool_count].db = db;
                                db_to_pool[db_to_pool_count].pool_slot = j;
                                db_to_pool_count++;
                                LOG_DEBUG("Tracked db %p -> pool slot %d", (void*)db, j);
                                break;
                            }
                        }
                    }
                    pthread_mutex_unlock(&pool_mutex);
                    return pool_conn;
                }
            }
            return handle_conn;
        }
    }
    pthread_mutex_unlock(&connections_mutex);
    return NULL;
}

pg_connection_t* pg_find_any_library_connection(void) {
    // Try to get a pooled connection
    if (library_db_path[0]) {
        pg_connection_t *pool_conn = pool_get_connection(library_db_path);
        if (pool_conn && pool_conn->is_pg_active) {
            return pool_conn;
        }
    }

    // Fall back to any registered library connection
    pthread_mutex_lock(&connections_mutex);
    for (int i = 0; i < MAX_CONNECTIONS; i++) {
        if (connections[i] && connections[i]->is_pg_active &&
            strstr(connections[i]->db_path, "com.plexapp.plugins.library.db")) {
            pg_connection_t *handle_conn = connections[i];
            pthread_mutex_unlock(&connections_mutex);

            // Try to get pooled connection
            pg_connection_t *pool_conn = pool_get_connection(handle_conn->db_path);
            if (pool_conn && pool_conn->is_pg_active) {
                return pool_conn;
            }
            return handle_conn;
        }
    }
    pthread_mutex_unlock(&connections_mutex);
    return NULL;
}

// ============================================================================
// Connection Pool for library.db
// ============================================================================

static int is_library_db(const char *path) {
    return path && strstr(path, "com.plexapp.plugins.library.db") != NULL;
}

// Reap idle connections from pool (close connections idle > POOL_IDLE_TIMEOUT)
static void pool_reap_idle_connections(void) {
    time_t now = time(NULL);
    int reaped = 0;

    // Must be called with pool_mutex held
    for (int i = 0; i < POOL_SIZE; i++) {
        if (library_pool[i].conn &&
            library_pool[i].owner_thread != 0 &&
            (now - library_pool[i].last_used) > POOL_IDLE_TIMEOUT) {

            LOG_INFO("Pool reaper: closing idle connection %d (idle %ld seconds, thread %p)",
                    i, now - library_pool[i].last_used,
                    (void*)library_pool[i].owner_thread);

            if (library_pool[i].conn->conn) {
                PQfinish(library_pool[i].conn->conn);
            }
            pthread_mutex_destroy(&library_pool[i].conn->mutex);
            free(library_pool[i].conn);
            library_pool[i].conn = NULL;
            library_pool[i].owner_thread = 0;
            library_pool[i].last_used = 0;
            reaped++;
        }
    }

    if (reaped > 0) {
        LOG_INFO("Pool reaper: closed %d idle connections", reaped);
    }
}

static pg_connection_t* create_pool_connection(const char *db_path) {
    pg_conn_config_t *cfg = pg_config_get();

    pg_connection_t *conn = calloc(1, sizeof(pg_connection_t));
    if (!conn) {
        LOG_ERROR("Failed to allocate pg_connection_t for pool");
        return NULL;
    }

    pthread_mutex_init(&conn->mutex, NULL);
    strncpy(conn->db_path, db_path ? db_path : "", sizeof(conn->db_path) - 1);

    char conninfo[1024];
    snprintf(conninfo, sizeof(conninfo),
             "host=%s port=%d dbname=%s user=%s password=%s connect_timeout=5",
             cfg->host, cfg->port, cfg->database, cfg->user, cfg->password);

    conn->conn = PQconnectdb(conninfo);

    if (PQstatus(conn->conn) != CONNECTION_OK) {
        LOG_ERROR("Pool connection failed: %s", PQerrorMessage(conn->conn));
        PQfinish(conn->conn);
        conn->conn = NULL;
    } else {
        char schema_cmd[256];
        snprintf(schema_cmd, sizeof(schema_cmd), "SET search_path TO %s, public", cfg->schema);
        PGresult *res = PQexec(conn->conn, schema_cmd);
        if (PQresultStatus(res) != PGRES_COMMAND_OK) {
            LOG_ERROR("Failed to set search_path: %s", PQresultErrorMessage(res));
        }
        PQclear(res);
        conn->is_pg_active = 1;
    }

    return conn;
}

static pg_connection_t* pool_get_connection(const char *db_path) {
    if (!is_library_db(db_path)) {
        return NULL;
    }

    pthread_t current_thread = pthread_self();
    time_t now = time(NULL);

    pthread_mutex_lock(&pool_mutex);

    // Save the db_path for later use
    if (!library_db_path[0] && db_path) {
        strncpy(library_db_path, db_path, sizeof(library_db_path) - 1);
    }

    // Periodically reap idle connections (every ~10th call to reduce overhead)
    static int reap_counter = 0;
    if (++reap_counter >= 10) {
        pool_reap_idle_connections();
        reap_counter = 0;
    }

    // 1. First, find a connection already owned by this thread
    for (int i = 0; i < POOL_SIZE; i++) {
        if (library_pool[i].conn &&
            pthread_equal(library_pool[i].owner_thread, current_thread)) {
            library_pool[i].last_used = now;
            pg_connection_t *conn = library_pool[i].conn;

            // Check if connection is still healthy
            if (conn->conn && PQstatus(conn->conn) == CONNECTION_OK) {
                pthread_mutex_unlock(&pool_mutex);
                return conn;
            }
            // Connection is dead, will be replaced below
            break;
        }
    }

    // 2. Find a free slot or create new connection
    int oldest_idx = -1;
    time_t oldest_time = now;
    int free_idx = -1;

    for (int i = 0; i < POOL_SIZE; i++) {
        if (!library_pool[i].conn) {
            free_idx = i;
            break;
        }
        // Track oldest for potential reuse
        if (library_pool[i].last_used < oldest_time) {
            oldest_time = library_pool[i].last_used;
            oldest_idx = i;
        }
    }

    int use_idx = -1;

    if (free_idx >= 0) {
        // Create new connection in free slot
        use_idx = free_idx;
        library_pool[use_idx].conn = create_pool_connection(db_path);
        if (library_pool[use_idx].conn && library_pool[use_idx].conn->is_pg_active) {
            LOG_INFO("Pool: created new connection in slot %d (total: %d)",
                    use_idx, use_idx + 1);
        }
    } else if (oldest_idx >= 0) {
        // Reuse oldest connection
        use_idx = oldest_idx;
        LOG_DEBUG("Pool: reusing slot %d from thread %p (idle %lds)",
                 use_idx, (void*)library_pool[use_idx].owner_thread,
                 now - library_pool[use_idx].last_used);
    }

    if (use_idx >= 0 && library_pool[use_idx].conn) {
        library_pool[use_idx].owner_thread = current_thread;
        library_pool[use_idx].last_used = now;

        pg_connection_t *conn = library_pool[use_idx].conn;

        // Verify connection is healthy
        if (!conn->conn || PQstatus(conn->conn) != CONNECTION_OK) {
            // Reconnect
            if (conn->conn) {
                PQfinish(conn->conn);
            }
            pg_conn_config_t *cfg = pg_config_get();
            char conninfo[1024];
            snprintf(conninfo, sizeof(conninfo),
                     "host=%s port=%d dbname=%s user=%s password=%s connect_timeout=5",
                     cfg->host, cfg->port, cfg->database, cfg->user, cfg->password);
            conn->conn = PQconnectdb(conninfo);

            if (PQstatus(conn->conn) == CONNECTION_OK) {
                char schema_cmd[256];
                snprintf(schema_cmd, sizeof(schema_cmd), "SET search_path TO %s, public", cfg->schema);
                PGresult *res = PQexec(conn->conn, schema_cmd);
                PQclear(res);
                conn->is_pg_active = 1;
                LOG_INFO("Pool: reconnected slot %d", use_idx);
            } else {
                LOG_ERROR("Pool: reconnect failed for slot %d", use_idx);
                conn->is_pg_active = 0;
            }
        }

        pthread_mutex_unlock(&pool_mutex);
        return conn;
    }

    pthread_mutex_unlock(&pool_mutex);
    return NULL;
}

// Public function for getting thread connection (now uses pool)
pg_connection_t* pg_get_thread_connection(const char *db_path) {
    return pool_get_connection(db_path);
}

// Close pool connection for a specific database handle
// Called when sqlite3_close() is invoked
void pg_close_pool_for_db(sqlite3 *db) {
    if (!db) return;

    pthread_mutex_lock(&pool_mutex);

    // Find and remove db-to-pool mapping
    int pool_slot = -1;
    for (int i = 0; i < db_to_pool_count; i++) {
        if (db_to_pool[i].db == db) {
            pool_slot = db_to_pool[i].pool_slot;

            // Remove this mapping by shifting remaining entries
            for (int j = i; j < db_to_pool_count - 1; j++) {
                db_to_pool[j] = db_to_pool[j + 1];
            }
            db_to_pool_count--;
            break;
        }
    }

    // If we found a pool slot, mark it as free (reset owner_thread)
    // Don't close the connection immediately - let the reaper handle it
    // This allows other threads to reuse the connection if needed
    if (pool_slot >= 0 && pool_slot < POOL_SIZE) {
        if (library_pool[pool_slot].conn) {
            LOG_INFO("Pool: releasing slot %d for db %p (thread %p)",
                    pool_slot, (void*)db, (void*)library_pool[pool_slot].owner_thread);
            library_pool[pool_slot].owner_thread = 0;  // Mark as free for reuse
            library_pool[pool_slot].last_used = time(NULL);  // Update timestamp for reaper
        }
    }

    pthread_mutex_unlock(&pool_mutex);
}

// ============================================================================
// Connection Lifecycle (for non-pooled connections)
// ============================================================================

pg_connection_t* pg_connect(const char *db_path, sqlite3 *shadow_db) {
    pg_conn_config_t *cfg = pg_config_get();

    pg_connection_t *conn = calloc(1, sizeof(pg_connection_t));
    if (!conn) {
        LOG_ERROR("Failed to allocate pg_connection_t");
        return NULL;
    }

    pthread_mutex_init(&conn->mutex, NULL);
    conn->shadow_db = shadow_db;
    strncpy(conn->db_path, db_path ? db_path : "", sizeof(conn->db_path) - 1);

    char conninfo[1024];
    snprintf(conninfo, sizeof(conninfo),
             "host=%s port=%d dbname=%s user=%s password=%s connect_timeout=5",
             cfg->host, cfg->port, cfg->database, cfg->user, cfg->password);

    conn->conn = PQconnectdb(conninfo);

    if (PQstatus(conn->conn) != CONNECTION_OK) {
        LOG_ERROR("PostgreSQL connection failed: %s", PQerrorMessage(conn->conn));
        PQfinish(conn->conn);
        conn->conn = NULL;
    } else {
        LOG_INFO("PostgreSQL connected for: %s", db_path);

        char schema_cmd[256];
        snprintf(schema_cmd, sizeof(schema_cmd), "SET search_path TO %s, public", cfg->schema);
        PGresult *res = PQexec(conn->conn, schema_cmd);
        if (PQresultStatus(res) != PGRES_COMMAND_OK) {
            LOG_ERROR("Failed to set search_path: %s", PQresultErrorMessage(res));
        }
        PQclear(res);

        conn->is_pg_active = 1;
    }

    return conn;
}

int pg_ensure_connection(pg_connection_t *conn) {
    if (!conn) return 0;

    pthread_mutex_lock(&conn->mutex);

    if (conn->conn && PQstatus(conn->conn) == CONNECTION_OK) {
        pthread_mutex_unlock(&conn->mutex);
        return 1;
    }

    if (conn->conn) {
        PQfinish(conn->conn);
        conn->conn = NULL;
    }

    pg_conn_config_t *cfg = pg_config_get();
    char conninfo[1024];
    snprintf(conninfo, sizeof(conninfo),
             "host=%s port=%d dbname=%s user=%s password=%s connect_timeout=5",
             cfg->host, cfg->port, cfg->database, cfg->user, cfg->password);

    conn->conn = PQconnectdb(conninfo);

    if (PQstatus(conn->conn) != CONNECTION_OK) {
        LOG_ERROR("PostgreSQL reconnection failed: %s", PQerrorMessage(conn->conn));
        PQfinish(conn->conn);
        conn->conn = NULL;
        conn->is_pg_active = 0;
        pthread_mutex_unlock(&conn->mutex);
        return 0;
    }

    LOG_INFO("PostgreSQL reconnected successfully");

    char schema_cmd[256];
    snprintf(schema_cmd, sizeof(schema_cmd), "SET search_path TO %s, public", cfg->schema);
    PGresult *res = PQexec(conn->conn, schema_cmd);
    if (PQresultStatus(res) != PGRES_COMMAND_OK) {
        LOG_ERROR("Failed to set search_path on reconnect: %s", PQresultErrorMessage(res));
    }
    PQclear(res);

    conn->is_pg_active = 1;
    pthread_mutex_unlock(&conn->mutex);
    return 1;
}

void pg_close(pg_connection_t *conn) {
    if (!conn) return;

    pthread_mutex_lock(&conn->mutex);
    if (conn->conn) {
        PQfinish(conn->conn);
        conn->conn = NULL;
    }
    pthread_mutex_unlock(&conn->mutex);

    pthread_mutex_destroy(&conn->mutex);
    free(conn);
}

// ============================================================================
// Global Metadata ID
// ============================================================================

sqlite3_int64 pg_get_global_metadata_id(void) {
    pthread_mutex_lock(&metadata_id_mutex);
    sqlite3_int64 result = global_last_metadata_id;
    pthread_mutex_unlock(&metadata_id_mutex);
    return result;
}

void pg_set_global_metadata_id(sqlite3_int64 id) {
    pthread_mutex_lock(&metadata_id_mutex);
    global_last_metadata_id = id;
    pthread_mutex_unlock(&metadata_id_mutex);
}

// ============================================================================
// Global Last Insert Rowid (shared across connections)
// ============================================================================

sqlite3_int64 pg_get_global_last_insert_rowid(void) {
    pthread_mutex_lock(&global_rowid_mutex);
    sqlite3_int64 result = global_last_insert_rowid;
    pthread_mutex_unlock(&global_rowid_mutex);
    return result;
}

void pg_set_global_last_insert_rowid(sqlite3_int64 id) {
    pthread_mutex_lock(&global_rowid_mutex);
    global_last_insert_rowid = id;
    pthread_mutex_unlock(&global_rowid_mutex);
}
