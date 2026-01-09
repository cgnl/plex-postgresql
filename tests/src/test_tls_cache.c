/*
 * Unit tests for Thread-Local Storage (TLS) Caching
 *
 * Tests:
 * 1. TLS variable isolation between threads
 * 2. Cache invalidation on generation change
 * 3. Pool slot caching logic
 * 4. Connection cache hit/miss behavior
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <pthread.h>
#include <stdatomic.h>
#include <unistd.h>

// Test counters
static int tests_passed = 0;
static int tests_failed = 0;
static pthread_mutex_t test_mutex = PTHREAD_MUTEX_INITIALIZER;

#define TEST(name) printf("  Testing: %s... ", name)
#define PASS() do { printf("\033[32mPASS\033[0m\n"); pthread_mutex_lock(&test_mutex); tests_passed++; pthread_mutex_unlock(&test_mutex); } while(0)
#define FAIL(msg) do { printf("\033[31mFAIL: %s\033[0m\n", msg); pthread_mutex_lock(&test_mutex); tests_failed++; pthread_mutex_unlock(&test_mutex); } while(0)

// ============================================================================
// Simulate TLS pool slot cache from pg_client.c
// ============================================================================

static __thread int tls_pool_slot = -1;
static __thread uint32_t tls_pool_generation = 0;

// Simulate pool slot structure
typedef enum {
    SLOT_FREE = 0,
    SLOT_READY,
    SLOT_RESERVED,
    SLOT_ERROR
} slot_state_t;

typedef struct {
    atomic_int state;
    atomic_uint generation;
    pthread_t owner_thread;
    void *conn;  // Simulated connection
} mock_pool_slot_t;

#define POOL_SIZE 8
static mock_pool_slot_t mock_pool[POOL_SIZE];

// Simulate pool_get_connection fast path
static void* mock_pool_get_connection(void) {
    pthread_t current = pthread_self();

    // Fast path: check cached slot
    if (tls_pool_slot >= 0 && tls_pool_slot < POOL_SIZE) {
        mock_pool_slot_t *slot = &mock_pool[tls_pool_slot];

        if (atomic_load(&slot->state) == SLOT_READY &&
            pthread_equal(slot->owner_thread, current) &&
            atomic_load(&slot->generation) == tls_pool_generation) {
            return slot->conn;  // Cache hit!
        }
        // Cache invalid
        tls_pool_slot = -1;
    }

    // Slow path: scan pool (simplified)
    for (int i = 0; i < POOL_SIZE; i++) {
        if (atomic_load(&mock_pool[i].state) == SLOT_READY &&
            pthread_equal(mock_pool[i].owner_thread, current)) {
            // Found our slot - cache it
            tls_pool_slot = i;
            tls_pool_generation = atomic_load(&mock_pool[i].generation);
            return mock_pool[i].conn;
        }
    }

    return NULL;  // No connection found
}

// ============================================================================
// TLS Isolation Tests
// ============================================================================

static __thread int tls_test_value = 0;

typedef struct {
    int thread_id;
    int value_to_set;
    int observed_value;
} thread_data_t;

static void* tls_isolation_thread(void *arg) {
    thread_data_t *data = (thread_data_t *)arg;

    // Set our thread's value
    tls_test_value = data->value_to_set;

    // Small delay to allow other thread to set its value
    usleep(10000);

    // Read back - should still be our value
    data->observed_value = tls_test_value;

    return NULL;
}

static void test_tls_isolation(void) {
    TEST("TLS - thread isolation");

    thread_data_t data1 = {.thread_id = 1, .value_to_set = 100, .observed_value = 0};
    thread_data_t data2 = {.thread_id = 2, .value_to_set = 200, .observed_value = 0};

    pthread_t t1, t2;
    pthread_create(&t1, NULL, tls_isolation_thread, &data1);
    pthread_create(&t2, NULL, tls_isolation_thread, &data2);

    pthread_join(t1, NULL);
    pthread_join(t2, NULL);

    // Each thread should see its own value
    if (data1.observed_value == 100 && data2.observed_value == 200) {
        PASS();
    } else {
        FAIL("TLS values leaked between threads");
    }
}

// ============================================================================
// Pool Slot Cache Tests
// ============================================================================

static void test_cache_hit_same_thread(void) {
    TEST("Pool cache - hit on same thread");

    // Reset
    tls_pool_slot = -1;
    memset(mock_pool, 0, sizeof(mock_pool));

    pthread_t self = pthread_self();

    // Setup: slot 3 is READY and owned by us
    mock_pool[3].owner_thread = self;
    mock_pool[3].conn = (void*)0xDEADBEEF;
    atomic_store(&mock_pool[3].state, SLOT_READY);
    atomic_store(&mock_pool[3].generation, 42);

    // First call: slow path, should cache slot 3
    void *conn1 = mock_pool_get_connection();

    // Second call: should hit cache
    void *conn2 = mock_pool_get_connection();

    if (conn1 == (void*)0xDEADBEEF &&
        conn2 == (void*)0xDEADBEEF &&
        tls_pool_slot == 3 &&
        tls_pool_generation == 42) {
        PASS();
    } else {
        FAIL("Cache should hit on second call");
    }
}

static void test_cache_invalidate_on_generation(void) {
    TEST("Pool cache - invalidate on generation change");

    // Reset
    tls_pool_slot = -1;
    memset(mock_pool, 0, sizeof(mock_pool));

    pthread_t self = pthread_self();

    // Setup slot
    mock_pool[2].owner_thread = self;
    mock_pool[2].conn = (void*)0xCAFEBABE;
    atomic_store(&mock_pool[2].state, SLOT_READY);
    atomic_store(&mock_pool[2].generation, 10);

    // First call: caches slot 2 with generation 10
    void *conn1 = mock_pool_get_connection();

    // Simulate generation increment (another thread reused the slot)
    atomic_store(&mock_pool[2].generation, 11);

    // Also update owner (simulating another thread took over)
    mock_pool[2].owner_thread = (pthread_t)0;

    // Move our connection to slot 5
    mock_pool[5].owner_thread = self;
    mock_pool[5].conn = (void*)0xFEEDFACE;
    atomic_store(&mock_pool[5].state, SLOT_READY);
    atomic_store(&mock_pool[5].generation, 20);

    // Second call: cached generation doesn't match, should find new slot
    void *conn2 = mock_pool_get_connection();

    if (conn1 == (void*)0xCAFEBABE &&
        conn2 == (void*)0xFEEDFACE &&
        tls_pool_slot == 5) {
        PASS();
    } else {
        FAIL("Should find new slot after generation change");
    }
}

static void test_cache_invalidate_on_state_change(void) {
    TEST("Pool cache - invalidate on state change");

    // Reset
    tls_pool_slot = -1;
    memset(mock_pool, 0, sizeof(mock_pool));

    pthread_t self = pthread_self();

    // Setup slot
    mock_pool[1].owner_thread = self;
    mock_pool[1].conn = (void*)0x12345678;
    atomic_store(&mock_pool[1].state, SLOT_READY);
    atomic_store(&mock_pool[1].generation, 5);

    // First call: caches slot 1
    mock_pool_get_connection();

    if (tls_pool_slot != 1) {
        FAIL("Should cache slot 1");
        return;
    }

    // Slot goes to ERROR state
    atomic_store(&mock_pool[1].state, SLOT_ERROR);

    // Setup new slot
    mock_pool[4].owner_thread = self;
    mock_pool[4].conn = (void*)0x87654321;
    atomic_store(&mock_pool[4].state, SLOT_READY);
    atomic_store(&mock_pool[4].generation, 6);

    // Second call: state doesn't match, should find new slot
    void *conn = mock_pool_get_connection();

    if (conn == (void*)0x87654321 && tls_pool_slot == 4) {
        PASS();
    } else {
        FAIL("Should find new slot after state change");
    }
}

// ============================================================================
// Multi-threaded Cache Tests
// ============================================================================

typedef struct {
    int slot_to_use;
    void *expected_conn;
    int success;
} mt_thread_data_t;

static void* mt_cache_thread(void *arg) {
    mt_thread_data_t *data = (mt_thread_data_t *)arg;
    pthread_t self = pthread_self();

    // Each thread gets its own slot
    mock_pool[data->slot_to_use].owner_thread = self;
    mock_pool[data->slot_to_use].conn = data->expected_conn;
    atomic_store(&mock_pool[data->slot_to_use].state, SLOT_READY);
    atomic_store(&mock_pool[data->slot_to_use].generation, 1);

    // Get connection multiple times
    for (int i = 0; i < 100; i++) {
        void *conn = mock_pool_get_connection();
        if (conn != data->expected_conn) {
            data->success = 0;
            return NULL;
        }
    }

    data->success = 1;
    return NULL;
}

static void test_multithread_cache_isolation(void) {
    TEST("Pool cache - multi-thread isolation");

    // Reset
    memset(mock_pool, 0, sizeof(mock_pool));

    mt_thread_data_t data[4] = {
        {.slot_to_use = 0, .expected_conn = (void*)0x1111, .success = 0},
        {.slot_to_use = 1, .expected_conn = (void*)0x2222, .success = 0},
        {.slot_to_use = 2, .expected_conn = (void*)0x3333, .success = 0},
        {.slot_to_use = 3, .expected_conn = (void*)0x4444, .success = 0},
    };

    pthread_t threads[4];
    for (int i = 0; i < 4; i++) {
        pthread_create(&threads[i], NULL, mt_cache_thread, &data[i]);
    }

    for (int i = 0; i < 4; i++) {
        pthread_join(threads[i], NULL);
    }

    int all_success = 1;
    for (int i = 0; i < 4; i++) {
        if (!data[i].success) all_success = 0;
    }

    if (all_success) {
        PASS();
    } else {
        FAIL("Some threads got wrong connection");
    }
}

// ============================================================================
// Connection Cache Tests (simulating tls_cached_db/tls_cached_conn)
// ============================================================================

static __thread void *tls_cached_db = NULL;
static __thread void *tls_cached_conn = NULL;

static void* mock_find_connection(void *db) {
    // Fast path: check cache
    if (tls_cached_db == db && tls_cached_conn != NULL) {
        return tls_cached_conn;  // Cache hit
    }

    // Slow path: "lookup" (simulated)
    void *conn = (void*)((uintptr_t)db + 0x1000);

    // Cache for next time
    tls_cached_db = db;
    tls_cached_conn = conn;

    return conn;
}

static void test_conn_cache_hit(void) {
    TEST("Conn cache - hit on same db");

    tls_cached_db = NULL;
    tls_cached_conn = NULL;

    void *db = (void*)0xDB000001;

    void *conn1 = mock_find_connection(db);
    void *conn2 = mock_find_connection(db);

    // Should return same connection and use cache
    if (conn1 == conn2 && tls_cached_db == db) {
        PASS();
    } else {
        FAIL("Should hit cache on second call");
    }
}

static void test_conn_cache_miss_different_db(void) {
    TEST("Conn cache - miss on different db");

    tls_cached_db = NULL;
    tls_cached_conn = NULL;

    void *db1 = (void*)0xDB000001;
    void *db2 = (void*)0xDB000002;

    void *conn1 = mock_find_connection(db1);
    void *conn2 = mock_find_connection(db2);

    // Different DBs should get different connections
    if (conn1 != conn2 && tls_cached_db == db2) {
        PASS();
    } else {
        FAIL("Different DBs should get different connections");
    }
}

// ============================================================================
// Main
// ============================================================================

int main(void) {
    printf("\n\033[1m=== TLS Cache Tests ===\033[0m\n\n");

    printf("\033[1mTLS Isolation:\033[0m\n");
    test_tls_isolation();

    printf("\n\033[1mPool Slot Cache:\033[0m\n");
    test_cache_hit_same_thread();
    test_cache_invalidate_on_generation();
    test_cache_invalidate_on_state_change();

    printf("\n\033[1mMulti-threaded:\033[0m\n");
    test_multithread_cache_isolation();

    printf("\n\033[1mConnection Cache:\033[0m\n");
    test_conn_cache_hit();
    test_conn_cache_miss_different_db();

    printf("\n\033[1m=== Results ===\033[0m\n");
    printf("Passed: \033[32m%d\033[0m\n", tests_passed);
    printf("Failed: \033[31m%d\033[0m\n", tests_failed);
    printf("\n");

    return tests_failed > 0 ? 1 : 0;
}
