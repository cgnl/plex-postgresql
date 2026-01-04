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
#include <stdint.h>
#include <stdatomic.h>
#include <sys/select.h>
#include <sys/socket.h>

// Socket timeout for PostgreSQL connections (prevents infinite poll() waits)
#define PG_SOCKET_TIMEOUT_SEC 60

// ============================================================================
// Connection Pool Configuration
// ============================================================================

// POOL_SIZE is defined in pg_types.h (30)

typedef struct {
    pg_connection_t *conn;
    pthread_t owner_thread;           // Thread that owns this connection (0 = free)
    time_t last_used;                 // Last time this connection was used
    _Atomic pool_slot_state_t state;  // Atomic state machine (replaces in_use)
    _Atomic uint32_t generation;      // Increment on each reuse to detect stale refs
} pool_slot_t;

// ============================================================================
// Static State
// ============================================================================

static pg_connection_t *connections[MAX_CONNECTIONS];
static pthread_mutex_t connections_mutex = PTHREAD_MUTEX_INITIALIZER;
static volatile int client_initialized = 0;
static pthread_once_t client_init_once = PTHREAD_ONCE_INIT;

// Connection pool for library.db
static pool_slot_t library_pool[POOL_SIZE];
static pthread_mutex_t pool_mutex = PTHREAD_MUTEX_INITIALIZER;
static char library_db_path[512] = {0};

// Connection idle timeout (seconds) - release slots idle longer than this
// Must be long enough for slow queries and Plex processing between step() calls
#define POOL_IDLE_TIMEOUT 300

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

// Helper: Set socket timeouts on PostgreSQL connection to prevent infinite waits
static void pg_set_socket_timeout(PGconn *pg_conn) {
    if (!pg_conn) return;

    int sock = PQsocket(pg_conn);
    if (sock < 0) {
        LOG_ERROR("pg_set_socket_timeout: invalid socket");
        return;
    }

    struct timeval tv;
    tv.tv_sec = PG_SOCKET_TIMEOUT_SEC;
    tv.tv_usec = 0;

    if (setsockopt(sock, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv)) < 0) {
        LOG_ERROR("pg_set_socket_timeout: failed to set SO_RCVTIMEO");
    }
    if (setsockopt(sock, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof(tv)) < 0) {
        LOG_ERROR("pg_set_socket_timeout: failed to set SO_SNDTIMEO");
    }

    LOG_DEBUG("Socket timeout set to %d seconds for socket %d", PG_SOCKET_TIMEOUT_SEC, sock);
}

// ============================================================================
// Initialization
// ============================================================================

static void do_client_init(void) {
    memset(connections, 0, sizeof(connections));
    memset(library_pool, 0, sizeof(library_pool));
    client_initialized = 1;
    LOG_DEBUG("pg_client initialized with pool size %d", POOL_SIZE);
}

void pg_client_init(void) {
    pthread_once(&client_init_once, do_client_init);
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

    // Clean up pool - use atomic state transitions
    pthread_mutex_lock(&pool_mutex);
    for (int i = 0; i < POOL_SIZE; i++) {
        // Force transition to FREE regardless of current state
        pool_slot_state_t old_state = atomic_exchange(&library_pool[i].state, SLOT_FREE);

        if (library_pool[i].conn) {
            if (library_pool[i].conn->conn) {
                LOG_INFO("Cleanup: closing pool connection %d (state was %d, thread %p)",
                        i, old_state, (void*)library_pool[i].owner_thread);
                PQfinish(library_pool[i].conn->conn);
            }
            pthread_mutex_destroy(&library_pool[i].conn->mutex);
            free(library_pool[i].conn);
            library_pool[i].conn = NULL;
        }
        library_pool[i].owner_thread = 0;
        library_pool[i].generation = 0;
    }
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
                // Pool is full - return NULL to fall back to SQLite
                // DO NOT return handle_conn as it has no real PG connection
                LOG_DEBUG("Pool full for library.db, falling back to SQLite");
                return NULL;
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

// Fast suffix check - avoids full strstr scan
static int is_library_db(const char *path) {
    if (!path) return 0;
    static const char suffix[] = "com.plexapp.plugins.library.db";
    static const size_t suffix_len = sizeof(suffix) - 1;  // 30 chars
    size_t path_len = strlen(path);
    if (path_len < suffix_len) return 0;
    return memcmp(path + path_len - suffix_len, suffix, suffix_len) == 0;
}

// Reap idle connections from pool (close connections idle > POOL_IDLE_TIMEOUT)
// NOTE: Currently disabled (causes race conditions with new state machine)
// Connections are cleaned up on close or reused via PHASE 2/4
static void pool_reap_idle_connections(void) {
    time_t now = time(NULL);
    int reaped = 0;

    // Only reap FREE slots with old connections - use atomic CAS to claim
    for (int i = 0; i < POOL_SIZE; i++) {
        if (library_pool[i].conn &&
            (now - library_pool[i].last_used) > POOL_IDLE_TIMEOUT) {

            // Try to claim the slot atomically
            pool_slot_state_t expected = SLOT_FREE;
            if (atomic_compare_exchange_strong(&library_pool[i].state,
                                               &expected, SLOT_RESERVED)) {
                LOG_INFO("Pool reaper: closing idle connection %d (idle %ld seconds)",
                        i, now - library_pool[i].last_used);

                if (library_pool[i].conn->conn) {
                    PQfinish(library_pool[i].conn->conn);
                }
                pthread_mutex_destroy(&library_pool[i].conn->mutex);
                free(library_pool[i].conn);
                library_pool[i].conn = NULL;
                library_pool[i].owner_thread = 0;
                library_pool[i].last_used = 0;
                atomic_store(&library_pool[i].state, SLOT_FREE);
                reaped++;
            }
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
    // Include TCP keepalive to detect dead connections faster
    snprintf(conninfo, sizeof(conninfo),
             "host=%s port=%d dbname=%s user=%s password=%s "
             "connect_timeout=5 keepalives=1 keepalives_idle=30 "
             "keepalives_interval=10 keepalives_count=3",
             cfg->host, cfg->port, cfg->database, cfg->user, cfg->password);

    conn->conn = PQconnectdb(conninfo);

    if (PQstatus(conn->conn) != CONNECTION_OK) {
        const char *err = conn->conn ? PQerrorMessage(conn->conn) : "NULL connection";
        LOG_ERROR("Pool connection failed: %s", err);
        if (conn->conn) {
            PQfinish(conn->conn);
        }
        conn->conn = NULL;
    } else {
        // Set socket timeout to prevent infinite poll() waits on dead connections
        pg_set_socket_timeout(conn->conn);

        char schema_cmd[256];
        snprintf(schema_cmd, sizeof(schema_cmd), "SET search_path TO %s, public", cfg->schema);
        PGresult *res = PQexec(conn->conn, schema_cmd);
        if (PQresultStatus(res) != PGRES_COMMAND_OK) {
            const char *err = res ? PQresultErrorMessage(res) : "NULL result";
            LOG_ERROR("Failed to set search_path: %s", err);
        }
        if (res) PQclear(res);

        // Set statement_timeout to prevent infinite hangs on PostgreSQL lock contention
        res = PQexec(conn->conn, "SET statement_timeout = '60s'");
        if (PQresultStatus(res) != PGRES_COMMAND_OK) {
            LOG_ERROR("Failed to set statement_timeout: %s", PQresultErrorMessage(res));
        }
        if (res) PQclear(res);

        conn->is_pg_active = 1;
    }

    return conn;
}

// Helper: Perform reconnection for a slot (caller must own the slot via SLOT_RECONNECTING)
static pg_connection_t* do_slot_reconnect(int slot_idx) {
    pg_connection_t *conn = library_pool[slot_idx].conn;
    if (!conn) {
        atomic_store(&library_pool[slot_idx].state, SLOT_ERROR);
        return NULL;
    }

    // Clear prepared statement cache - statements are invalidated on reconnect
    pg_stmt_cache_clear(conn);

    // Close old connection if exists
    if (conn->conn) {
        PQfinish(conn->conn);
        conn->conn = NULL;
    }

    // Build connection string
    pg_conn_config_t *cfg = pg_config_get();
    char conninfo[1024];
    snprintf(conninfo, sizeof(conninfo),
             "host=%s port=%d dbname=%s user=%s password=%s "
             "connect_timeout=5 keepalives=1 keepalives_idle=30 "
             "keepalives_interval=10 keepalives_count=3",
             cfg->host, cfg->port, cfg->database, cfg->user, cfg->password);

    // Do network I/O (no mutex held - we own this slot via atomic state)
    PGconn *new_pg_conn = PQconnectdb(conninfo);

    if (PQstatus(new_pg_conn) == CONNECTION_OK) {
        // Set socket timeout to prevent infinite poll() waits
        pg_set_socket_timeout(new_pg_conn);

        char schema_cmd[256];
        snprintf(schema_cmd, sizeof(schema_cmd), "SET search_path TO %s, public", cfg->schema);
        PGresult *res = PQexec(new_pg_conn, schema_cmd);
        PQclear(res);

        // Set statement_timeout to prevent infinite hangs
        res = PQexec(new_pg_conn, "SET statement_timeout = '60s'");
        PQclear(res);

        conn->conn = new_pg_conn;
        conn->is_pg_active = 1;
        library_pool[slot_idx].last_used = time(NULL);

        LOG_INFO("Pool: reconnected slot %d", slot_idx);
        atomic_store(&library_pool[slot_idx].state, SLOT_READY);
        return conn;
    } else {
        LOG_ERROR("Pool: reconnect failed for slot %d: %s",
                  slot_idx, PQerrorMessage(new_pg_conn));
        PQfinish(new_pg_conn);
        conn->conn = NULL;
        conn->is_pg_active = 0;
        atomic_store(&library_pool[slot_idx].state, SLOT_ERROR);
        return NULL;
    }
}

static pg_connection_t* pool_get_connection(const char *db_path) {
    if (!is_library_db(db_path)) {
        return NULL;
    }

    pthread_t current_thread = pthread_self();
    time_t now = time(NULL);

    // Save db_path atomically (only first time)
    pthread_mutex_lock(&pool_mutex);
    if (!library_db_path[0] && db_path) {
        strncpy(library_db_path, db_path, sizeof(library_db_path) - 1);
    }
    pthread_mutex_unlock(&pool_mutex);

    // =========================================================================
    // PHASE 0: Cleanup stale READY connections (idle > POOL_IDLE_TIMEOUT)
    // Release slots that haven't been used - the owning thread is likely gone
    // Phase 2 will reset these connections when claimed
    // CRITICAL: Timeout must be long enough for slow queries and Plex processing
    // =========================================================================
    for (int i = 0; i < POOL_SIZE; i++) {
        pool_slot_state_t state = atomic_load(&library_pool[i].state);
        if (state == SLOT_READY && (now - library_pool[i].last_used) > POOL_IDLE_TIMEOUT) {
            // Mark as FREE so Phase 2 can claim and reset it
            pool_slot_state_t expected = SLOT_READY;
            if (atomic_compare_exchange_strong(&library_pool[i].state, &expected, SLOT_FREE)) {
                LOG_INFO("Pool: released stale slot %d (idle %lds)",
                        i, (long)(now - library_pool[i].last_used));
            }
        }
    }

    // =========================================================================
    // PHASE 1: Find thread's existing READY connection (lock-free)
    // =========================================================================
    for (int i = 0; i < POOL_SIZE; i++) {
        pool_slot_state_t state = atomic_load(&library_pool[i].state);

        if (state == SLOT_READY &&
            pthread_equal(library_pool[i].owner_thread, current_thread)) {

            pg_connection_t *conn = library_pool[i].conn;
            if (conn && conn->conn && PQstatus(conn->conn) == CONNECTION_OK) {
                // CRITICAL FIX: Do NOT check for pending data here!
                // When processing multiple statements, pending data is VALID response
                // data from queries we're actively executing. Resetting would cause
                // deadlock: PQexecPrepared waits for response, but we just reset it.
                //
                // Orphaned data (from timeouts) will be handled by:
                // 1. Query errors triggering pg_pool_check_connection_health()
                // 2. Connection reset when slot is released to another thread (PHASE 2)
                library_pool[i].last_used = now;
                return conn;
            }

            // Connection is dead - try to transition to RECONNECTING
            pool_slot_state_t expected = SLOT_READY;
            if (atomic_compare_exchange_strong(&library_pool[i].state,
                                               &expected, SLOT_RECONNECTING)) {
                // We own the reconnect
                return do_slot_reconnect(i);
            }
            // Another thread beat us - continue searching
        }
    }

    // =========================================================================
    // PHASE 2: Claim FREE slot with existing connection (reuse released slots)
    // =========================================================================
    for (int i = 0; i < POOL_SIZE; i++) {
        // Only try slots that have an existing connection we can reuse
        if (library_pool[i].conn == NULL) continue;

        pool_slot_state_t expected = SLOT_FREE;
        if (atomic_compare_exchange_strong(&library_pool[i].state,
                                           &expected, SLOT_RESERVED)) {
            // Successfully claimed slot atomically
            library_pool[i].owner_thread = current_thread;
            library_pool[i].last_used = now;
            atomic_fetch_add(&library_pool[i].generation, 1);

            pg_connection_t *conn = library_pool[i].conn;

            // When reusing another thread's connection, use PQreset() to ensure clean state
            // This is safer than trying to drain stale results which can crash
            if (conn && conn->conn) {
                LOG_DEBUG("Pool: resetting connection in slot %d for reuse", i);
                pg_stmt_cache_clear(conn);  // Clear cache before reset
                PQreset(conn->conn);

                if (PQstatus(conn->conn) == CONNECTION_OK) {
                    // Re-apply settings after reset
                    pg_conn_config_t *cfg = pg_config_get();
                    char schema_cmd[256];
                    snprintf(schema_cmd, sizeof(schema_cmd), "SET search_path TO %s, public", cfg->schema);
                    PGresult *res = PQexec(conn->conn, schema_cmd);
                    PQclear(res);
                    res = PQexec(conn->conn, "SET statement_timeout = '10s'");
                    PQclear(res);

                    LOG_DEBUG("Pool: reusing reset connection in slot %d", i);
                    atomic_store(&library_pool[i].state, SLOT_READY);
                    return conn;
                }
            }

            // Connection reset failed - do full reconnect
            atomic_store(&library_pool[i].state, SLOT_RECONNECTING);
            return do_slot_reconnect(i);
        }
    }

    // =========================================================================
    // PHASE 3: Find empty FREE slot and create new connection
    // =========================================================================
    for (int i = 0; i < POOL_SIZE; i++) {
        // Only try slots without existing connection
        if (library_pool[i].conn != NULL) continue;

        pool_slot_state_t expected = SLOT_FREE;
        if (atomic_compare_exchange_strong(&library_pool[i].state,
                                           &expected, SLOT_RESERVED)) {
            // Successfully claimed slot atomically
            library_pool[i].owner_thread = current_thread;
            library_pool[i].last_used = now;
            atomic_fetch_add(&library_pool[i].generation, 1);

            LOG_DEBUG("Pool: claimed empty slot %d for thread %p",
                     i, (void*)current_thread);

            // Create connection (no mutex held - we own this slot)
            pg_connection_t *new_conn = create_pool_connection(db_path);

            if (new_conn && new_conn->is_pg_active) {
                library_pool[i].conn = new_conn;
                LOG_INFO("Pool: created new connection in slot %d", i);
                atomic_store(&library_pool[i].state, SLOT_READY);
                return new_conn;
            } else {
                // Creation failed - release slot
                LOG_ERROR("Pool: failed to create connection for slot %d", i);
                library_pool[i].conn = NULL;
                library_pool[i].owner_thread = 0;
                if (new_conn) {
                    if (new_conn->conn) PQfinish(new_conn->conn);
                    pthread_mutex_destroy(&new_conn->mutex);
                    free(new_conn);
                }
                atomic_store(&library_pool[i].state, SLOT_FREE);
                // Continue trying other slots
            }
        }
    }

    // =========================================================================
    // PHASE 4: Try to claim ERROR slots (failed connections that need retry)
    // =========================================================================
    for (int i = 0; i < POOL_SIZE; i++) {
        pool_slot_state_t expected = SLOT_ERROR;
        if (atomic_compare_exchange_strong(&library_pool[i].state,
                                           &expected, SLOT_RESERVED)) {
            // Claimed error slot - clean up and recreate
            library_pool[i].owner_thread = current_thread;
            library_pool[i].last_used = now;
            atomic_fetch_add(&library_pool[i].generation, 1);

            // Free old connection if any
            if (library_pool[i].conn) {
                if (library_pool[i].conn->conn) {
                    PQfinish(library_pool[i].conn->conn);
                }
                pthread_mutex_destroy(&library_pool[i].conn->mutex);
                free(library_pool[i].conn);
                library_pool[i].conn = NULL;
            }

            LOG_DEBUG("Pool: reclaiming error slot %d", i);

            pg_connection_t *new_conn = create_pool_connection(db_path);
            if (new_conn && new_conn->is_pg_active) {
                library_pool[i].conn = new_conn;
                LOG_INFO("Pool: recovered slot %d with new connection", i);
                atomic_store(&library_pool[i].state, SLOT_READY);
                return new_conn;
            } else {
                library_pool[i].conn = NULL;
                library_pool[i].owner_thread = 0;
                if (new_conn) {
                    if (new_conn->conn) PQfinish(new_conn->conn);
                    pthread_mutex_destroy(&new_conn->mutex);
                    free(new_conn);
                }
                atomic_store(&library_pool[i].state, SLOT_FREE);
            }
        }
    }

    // =========================================================================
    // PHASE 5: Pool is full - log and return NULL
    // =========================================================================
    // Dump slot states for debugging
    for (int i = 0; i < POOL_SIZE; i++) {
        pool_slot_state_t state = atomic_load(&library_pool[i].state);
        LOG_ERROR("Pool slot %d: state=%d conn=%p owner=%p",
                 i, (int)state, (void*)library_pool[i].conn, (void*)library_pool[i].owner_thread);
    }
    LOG_ERROR("Pool: no available slots for thread %p (all %d slots busy)", (void*)current_thread, POOL_SIZE);
    return NULL;
}

// Public function for getting thread connection (now uses pool)
pg_connection_t* pg_get_thread_connection(const char *db_path) {
    return pool_get_connection(db_path);
}

// Update last_used timestamp for a connection to prevent premature pool release
// CRITICAL: Call this during long-running operations to keep connection alive
void pg_pool_touch_connection(pg_connection_t *conn) {
    if (!conn) return;

    pthread_t current_thread = pthread_self();
    time_t now = time(NULL);

    // Find the slot for this connection (lock-free)
    for (int i = 0; i < POOL_SIZE; i++) {
        if (library_pool[i].conn == conn &&
            pthread_equal(library_pool[i].owner_thread, current_thread)) {
            library_pool[i].last_used = now;
            return;
        }
    }
}

// Check connection health after query error, reset if corrupted
// Call this after any query that returns an unexpected error
// Returns 1 if connection was reset, 0 if still healthy
int pg_pool_check_connection_health(pg_connection_t *conn) {
    if (!conn || !conn->conn) return 0;

    // Check if connection is still valid
    ConnStatusType status = PQstatus(conn->conn);
    if (status == CONNECTION_OK) {
        return 0;  // Connection is healthy
    }

    // Connection is bad - need to reset
    LOG_ERROR("Pool: connection health check failed (status=%d), resetting", status);

    pthread_t current_thread = pthread_self();

    // Find our slot and transition to RECONNECTING
    for (int i = 0; i < POOL_SIZE; i++) {
        if (library_pool[i].conn == conn &&
            pthread_equal(library_pool[i].owner_thread, current_thread)) {

            pool_slot_state_t expected = SLOT_READY;
            if (atomic_compare_exchange_strong(&library_pool[i].state,
                                               &expected, SLOT_RECONNECTING)) {
                // Clear prepared statement cache
                pg_stmt_cache_clear(conn);

                // Reset connection
                PQreset(conn->conn);

                if (PQstatus(conn->conn) == CONNECTION_OK) {
                    // Re-apply settings
                    pg_conn_config_t *cfg = pg_config_get();
                    char schema_cmd[256];
                    snprintf(schema_cmd, sizeof(schema_cmd), "SET search_path TO %s, public", cfg->schema);
                    PGresult *res = PQexec(conn->conn, schema_cmd);
                    PQclear(res);
                    res = PQexec(conn->conn, "SET statement_timeout = '10s'");
                    PQclear(res);

                    LOG_INFO("Pool: connection reset successful for slot %d", i);
                    library_pool[i].last_used = time(NULL);
                    atomic_store(&library_pool[i].state, SLOT_READY);
                    return 1;
                } else {
                    // Reset failed - mark as error
                    LOG_ERROR("Pool: connection reset failed for slot %d", i);
                    atomic_store(&library_pool[i].state, SLOT_ERROR);
                    conn->is_pg_active = 0;
                    return 1;
                }
            }
            break;
        }
    }

    return 0;
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

    // If we found a pool slot owned by current thread, transition to FREE
    // The connection stays open for potential reuse by another thread
    if (pool_slot >= 0 && pool_slot < POOL_SIZE) {
        pthread_t current = pthread_self();
        if (library_pool[pool_slot].conn &&
            pthread_equal(library_pool[pool_slot].owner_thread, current)) {

            pool_slot_state_t current_state = atomic_load(&library_pool[pool_slot].state);
            LOG_INFO("Pool: releasing slot %d for db %p (state=%d, thread %p)",
                    pool_slot, (void*)db, current_state, (void*)current);

            // Only release if in READY state (not while RECONNECTING)
            if (current_state == SLOT_READY) {
                library_pool[pool_slot].owner_thread = 0;
                library_pool[pool_slot].last_used = time(NULL);
                // Keep state READY - connection is still valid, just unowned
                // Another thread will claim it in PHASE 1 or it will be reused
                atomic_store(&library_pool[pool_slot].state, SLOT_FREE);
            }
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

    // For library.db, DON'T create a real PostgreSQL connection here.
    // All queries will go through the connection pool via pg_find_connection().
    // This prevents connection leaks where each sqlite3_open creates a new PG connection.
    if (is_library_db(db_path)) {
        conn->conn = NULL;  // No direct connection - pool handles it
        conn->is_pg_active = 1;  // Mark as active so queries use the pool
        LOG_INFO("PostgreSQL pool-only connection for: %s", db_path);
        return conn;
    }

    char conninfo[1024];
    snprintf(conninfo, sizeof(conninfo),
             "host=%s port=%d dbname=%s user=%s password=%s connect_timeout=5",
             cfg->host, cfg->port, cfg->database, cfg->user, cfg->password);

    conn->conn = PQconnectdb(conninfo);

    if (PQstatus(conn->conn) != CONNECTION_OK) {
        const char *err = conn->conn ? PQerrorMessage(conn->conn) : "NULL connection";
        LOG_ERROR("PostgreSQL connection failed: %s", err);
        if (conn->conn) {
            PQfinish(conn->conn);
        }
        conn->conn = NULL;
    } else {
        LOG_INFO("PostgreSQL connected for: %s", db_path);

        // Set socket timeout to prevent infinite poll() waits
        pg_set_socket_timeout(conn->conn);

        char schema_cmd[256];
        snprintf(schema_cmd, sizeof(schema_cmd), "SET search_path TO %s, public", cfg->schema);
        PGresult *res = PQexec(conn->conn, schema_cmd);
        if (PQresultStatus(res) != PGRES_COMMAND_OK) {
            const char *err = res ? PQresultErrorMessage(res) : "NULL result";
            LOG_ERROR("Failed to set search_path: %s", err);
        }
        if (res) PQclear(res);

        // Set statement_timeout to prevent infinite hangs
        res = PQexec(conn->conn, "SET statement_timeout = '60s'");
        if (PQresultStatus(res) != PGRES_COMMAND_OK) {
            LOG_ERROR("Failed to set statement_timeout: %s", PQresultErrorMessage(res));
        }
        if (res) PQclear(res);

        conn->is_pg_active = 1;
    }

    return conn;
}

int pg_ensure_connection(pg_connection_t *conn) {
    if (!conn) return 0;

    pthread_mutex_lock(&conn->mutex);

    // Check if connection exists and is healthy
    if (conn->conn && PQstatus(conn->conn) == CONNECTION_OK) {
        // Additional health check: send a simple ping query
        PGresult *res = PQexec(conn->conn, "SELECT 1");
        if (res && PQresultStatus(res) == PGRES_TUPLES_OK) {
            PQclear(res);
            pthread_mutex_unlock(&conn->mutex);
            return 1;
        }
        if (res) PQclear(res);
        // Connection is broken, will reconnect below
        LOG_INFO("Connection health check failed, will reconnect");
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
        const char *err = conn->conn ? PQerrorMessage(conn->conn) : "NULL connection";
        LOG_ERROR("PostgreSQL reconnection failed: %s", err);
        if (conn->conn) {
            PQfinish(conn->conn);
        }
        conn->conn = NULL;
        conn->is_pg_active = 0;
        pthread_mutex_unlock(&conn->mutex);
        return 0;
    }

    LOG_INFO("PostgreSQL reconnected successfully");

    // Set socket timeout to prevent infinite poll() waits
    pg_set_socket_timeout(conn->conn);

    char schema_cmd[256];
    snprintf(schema_cmd, sizeof(schema_cmd), "SET search_path TO %s, public", cfg->schema);
    PGresult *res = PQexec(conn->conn, schema_cmd);
    if (PQresultStatus(res) != PGRES_COMMAND_OK) {
        const char *err = res ? PQresultErrorMessage(res) : "NULL result";
        LOG_ERROR("Failed to set search_path on reconnect: %s", err);
    }
    if (res) PQclear(res);

    // Set statement_timeout to prevent infinite hangs
    res = PQexec(conn->conn, "SET statement_timeout = '60s'");
    if (PQresultStatus(res) != PGRES_COMMAND_OK) {
        LOG_ERROR("Failed to set statement_timeout on reconnect: %s", PQresultErrorMessage(res));
    }
    if (res) PQclear(res);

    conn->is_pg_active = 1;
    pthread_mutex_unlock(&conn->mutex);
    return 1;
}

void pg_close(pg_connection_t *conn) {
    if (!conn) return;

    // Clear prepared statement cache before closing
    pg_stmt_cache_clear(conn);

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

// ============================================================================
// Prepared Statement Cache Management
// ============================================================================

// FNV-1a hash - fast with good distribution
uint64_t pg_hash_sql(const char *sql) {
    if (!sql) return 0;

    uint64_t hash = 14695981039346656037ULL;  // FNV offset basis
    while (*sql) {
        hash ^= (uint64_t)(unsigned char)*sql++;
        hash *= 1099511628211ULL;  // FNV prime
    }
    return hash;
}

// Lookup statement in cache by hash
// Returns 1 if found (stmt_name set), 0 if not found
int pg_stmt_cache_lookup(pg_connection_t *conn, uint64_t sql_hash, const char **stmt_name) {
    if (!conn || !stmt_name) return 0;

    stmt_cache_t *cache = &conn->stmt_cache;

    for (int i = 0; i < cache->count; i++) {
        if (cache->entries[i].sql_hash == sql_hash && cache->entries[i].prepared) {
            // Update last_used for LRU
            cache->entries[i].last_used = time(NULL);
            *stmt_name = cache->entries[i].stmt_name;
            return 1;
        }
    }

    return 0;
}

// Add statement to cache
// Returns index of entry, or -1 on failure
int pg_stmt_cache_add(pg_connection_t *conn, uint64_t sql_hash, const char *stmt_name, int param_count) {
    if (!conn || !stmt_name) return -1;

    stmt_cache_t *cache = &conn->stmt_cache;

    // Check if already exists (update it)
    for (int i = 0; i < cache->count; i++) {
        if (cache->entries[i].sql_hash == sql_hash) {
            cache->entries[i].prepared = 1;
            cache->entries[i].param_count = param_count;
            cache->entries[i].last_used = time(NULL);
            strncpy(cache->entries[i].stmt_name, stmt_name, sizeof(cache->entries[i].stmt_name) - 1);
            cache->entries[i].stmt_name[sizeof(cache->entries[i].stmt_name) - 1] = '\0';
            LOG_DEBUG("Updated prepared statement in cache: %s (hash=%llx)", stmt_name, (unsigned long long)sql_hash);
            return i;
        }
    }

    // Need to add new entry
    int idx;
    if (cache->count < STMT_CACHE_SIZE) {
        // Room available
        idx = cache->count++;
    } else {
        // Cache full - evict LRU entry
        idx = 0;
        time_t oldest = cache->entries[0].last_used;
        for (int i = 1; i < STMT_CACHE_SIZE; i++) {
            if (cache->entries[i].last_used < oldest) {
                oldest = cache->entries[i].last_used;
                idx = i;
            }
        }

        // Deallocate the old prepared statement on PostgreSQL
        if (cache->entries[idx].prepared && conn->conn) {
            char dealloc[64];
            snprintf(dealloc, sizeof(dealloc), "DEALLOCATE %s", cache->entries[idx].stmt_name);
            PGresult *res = PQexec(conn->conn, dealloc);
            if (res) PQclear(res);
            LOG_DEBUG("Evicted prepared statement from cache: %s", cache->entries[idx].stmt_name);
        }
    }

    // Fill in the entry
    cache->entries[idx].sql_hash = sql_hash;
    cache->entries[idx].param_count = param_count;
    cache->entries[idx].prepared = 1;
    cache->entries[idx].last_used = time(NULL);
    strncpy(cache->entries[idx].stmt_name, stmt_name, sizeof(cache->entries[idx].stmt_name) - 1);
    cache->entries[idx].stmt_name[sizeof(cache->entries[idx].stmt_name) - 1] = '\0';

    LOG_DEBUG("Added prepared statement to cache: %s (hash=%llx, idx=%d)", stmt_name, (unsigned long long)sql_hash, idx);
    return idx;
}

// Clear all cached statements for a connection (called on disconnect/reset)
void pg_stmt_cache_clear(pg_connection_t *conn) {
    if (!conn) return;

    stmt_cache_t *cache = &conn->stmt_cache;

    // Deallocate all prepared statements on PostgreSQL
    if (conn->conn) {
        for (int i = 0; i < cache->count; i++) {
            if (cache->entries[i].prepared) {
                char dealloc[64];
                snprintf(dealloc, sizeof(dealloc), "DEALLOCATE %s", cache->entries[i].stmt_name);
                PGresult *res = PQexec(conn->conn, dealloc);
                if (res) PQclear(res);
            }
        }
    }

    // Clear the cache
    memset(cache, 0, sizeof(stmt_cache_t));
    LOG_DEBUG("Cleared prepared statement cache for connection %p", (void*)conn);
}
