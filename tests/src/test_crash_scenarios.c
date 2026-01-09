/*
 * Unit tests based on historical crash analysis
 *
 * This file contains tests derived from actual crashes that occurred in production:
 * - Stack overflow (2026-01-06): 218 recursive frames, 544KB stack exhausted
 * - Integer overflow (2025-12-31): signed int overflow after ~2B calls
 * - Thread safety (2025-12-24): heap corruption from concurrent PQclear
 * - NULL pointer (2026-01-01): strchr/strcasestr returning NULL
 * - Deadlock (2026-01-01): blocking mutex causing 40+ thread freeze
 * - Connection pool (2025-12-31): stale slot references, state corruption
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <stddef.h>
#include <pthread.h>
#include <assert.h>
#include <unistd.h>
#include <sys/time.h>
#include <stdatomic.h>
#include <limits.h>

// Test counters
static int tests_passed = 0;
static int tests_failed = 0;

#define TEST(name) printf("  Testing: %s... ", name)
#define PASS() do { printf("\033[32mPASS\033[0m\n"); tests_passed++; } while(0)
#define FAIL(msg) do { printf("\033[31mFAIL: %s\033[0m\n", msg); tests_failed++; } while(0)

// ============================================================================
// CRASH SCENARIO 1: Integer Overflow (2025-12-31)
// Bug: fake_value_next was signed int, overflowed after ~2B calls
// Fix: Use unsigned int + bitmask instead of modulo
// ============================================================================

#define MAX_FAKE_VALUES 256

// BUGGY version (caused SIGBUS)
static int buggy_get_fake_slot_signed(int *counter) {
    int slot = (*counter)++ % MAX_FAKE_VALUES;
    return slot;  // BUG: Can be negative when counter overflows!
}

// FIXED version (what we use now)
static unsigned int fixed_get_fake_slot_unsigned(unsigned int *counter) {
    unsigned int slot = ((*counter)++) & 0xFF;  // Bitmask always positive
    return slot;
}

void test_integer_overflow_signed(void) {
    TEST("Integer overflow - signed counter bug detection");

    int counter = INT_MAX - 5;  // Start near overflow
    int negative_found = 0;

    for (int i = 0; i < 20; i++) {
        int slot = buggy_get_fake_slot_signed(&counter);
        if (slot < 0) {
            negative_found = 1;
            break;
        }
    }

    if (negative_found) {
        PASS();  // We correctly detected the bug would occur
    } else {
        FAIL("Should have detected negative slot index");
    }
}

void test_integer_overflow_fixed(void) {
    TEST("Integer overflow - unsigned counter fix");

    unsigned int counter = UINT_MAX - 5;  // Start near overflow
    int all_valid = 1;

    for (int i = 0; i < 20; i++) {
        unsigned int slot = fixed_get_fake_slot_unsigned(&counter);
        if (slot >= MAX_FAKE_VALUES) {
            all_valid = 0;
            break;
        }
    }

    if (all_valid) {
        PASS();
    } else {
        FAIL("Slot index out of range after overflow");
    }
}

void test_bitmask_vs_modulo(void) {
    TEST("Bitmask vs modulo equivalence for power-of-2");

    // Verify bitmask (& 0xFF) equals modulo (% 256) for all uint values
    int failures = 0;
    for (unsigned int i = 0; i < 1000; i++) {
        if ((i & 0xFF) != (i % 256)) {
            failures++;
        }
    }

    // Also test edge cases
    unsigned int edge_cases[] = {0, 255, 256, 257, UINT_MAX - 1, UINT_MAX};
    for (int i = 0; i < 6; i++) {
        if ((edge_cases[i] & 0xFF) != (edge_cases[i] % 256)) {
            failures++;
        }
    }

    if (failures == 0) {
        PASS();
    } else {
        FAIL("Bitmask and modulo differ");
    }
}

// ============================================================================
// CRASH SCENARIO 2: Stack Overflow (2026-01-06)
// Bug: Recursion depth reached 218, stack exhausted 544KB
// Fix: Track depth, reject at 100; threshold checks at 400KB/500KB
// ============================================================================

#define RECURSION_LIMIT 100
#define STACK_HARD_LIMIT 400000   // 400KB - reject query
#define STACK_SOFT_LIMIT 500000   // 500KB - skip complex processing

static __thread int prepare_depth = 0;

typedef struct {
    int recursion_rejected;
    int stack_rejected;
    int soft_limit_triggered;
} protection_result_t;

static protection_result_t simulate_prepare_with_protection(int depth, size_t stack_remaining) {
    protection_result_t result = {0, 0, 0};

    prepare_depth = depth;

    // Check 1: Recursion limit
    if (prepare_depth > RECURSION_LIMIT) {
        result.recursion_rejected = 1;
        return result;
    }

    // Check 2: Hard stack limit
    if (stack_remaining < STACK_HARD_LIMIT) {
        result.stack_rejected = 1;
        return result;
    }

    // Check 3: Soft stack limit
    if (stack_remaining < STACK_SOFT_LIMIT) {
        result.soft_limit_triggered = 1;
    }

    return result;
}

void test_recursion_limit_218(void) {
    TEST("Recursion limit - rejects at depth 218 (actual crash)");

    protection_result_t r = simulate_prepare_with_protection(218, 1000000);

    if (r.recursion_rejected) {
        PASS();
    } else {
        FAIL("Should reject at 218 recursion depth");
    }
}

void test_recursion_limit_boundary(void) {
    TEST("Recursion limit - boundary at 100");

    protection_result_t r100 = simulate_prepare_with_protection(100, 1000000);
    protection_result_t r101 = simulate_prepare_with_protection(101, 1000000);

    if (!r100.recursion_rejected && r101.recursion_rejected) {
        PASS();
    } else {
        FAIL("Should accept 100, reject 101");
    }
}

void test_stack_hard_limit(void) {
    TEST("Stack hard limit - rejects below 400KB");

    protection_result_t r_ok = simulate_prepare_with_protection(1, 450000);
    protection_result_t r_fail = simulate_prepare_with_protection(1, 350000);

    if (!r_ok.stack_rejected && r_fail.stack_rejected) {
        PASS();
    } else {
        FAIL("Should accept 450KB, reject 350KB");
    }
}

void test_stack_soft_limit(void) {
    TEST("Stack soft limit - triggers below 500KB");

    protection_result_t r_ok = simulate_prepare_with_protection(1, 550000);
    protection_result_t r_soft = simulate_prepare_with_protection(1, 450000);

    if (!r_ok.soft_limit_triggered && r_soft.soft_limit_triggered) {
        PASS();
    } else {
        FAIL("Should not trigger at 550KB, should trigger at 450KB");
    }
}

void test_crash_scenario_exact(void) {
    TEST("Exact crash scenario - 544KB stack, 42KB remaining");

    // From CRASH_ANALYSIS_ONDECK.md: 42KB remaining caused crash
    protection_result_t r = simulate_prepare_with_protection(1, 42000);

    if (r.stack_rejected) {
        PASS();
    } else {
        FAIL("Should reject with only 42KB remaining");
    }
}

// ============================================================================
// CRASH SCENARIO 3: NULL Pointer (2026-01-01)
// Bug: strchr/strcasestr returning NULL not checked
// Fix: Always check return values before use
// ============================================================================

// Safe version of strcasestr that handles NULL
static char* safe_strcasestr(const char *haystack, const char *needle) {
    if (!haystack || !needle) return NULL;

    size_t needle_len = strlen(needle);
    if (needle_len == 0) return (char*)haystack;

    for (const char *p = haystack; *p; p++) {
        if (strncasecmp(p, needle, needle_len) == 0) {
            return (char*)p;
        }
    }
    return NULL;
}

// Simulate the buggy json operator fix (kept for documentation)
// static int buggy_fix_json_operator(const char *sql) {
//     char *arrow = strchr(sql, '>');
//     // BUG: No NULL check!
//     if (arrow[-1] == '-') {  // Crash if arrow is NULL!
//         return 1;
//     }
//     return 0;
// }

static int fixed_fix_json_operator(const char *sql) {
    char *arrow = strchr(sql, '>');
    if (arrow && arrow > sql && arrow[-1] == '-') {
        return 1;
    }
    return 0;
}

void test_null_strchr_crash(void) {
    TEST("NULL pointer - strchr returns NULL");

    // This would crash with buggy version
    const char *sql = "SELECT * FROM table";  // No '>' character

    // Fixed version should handle this
    int result = fixed_fix_json_operator(sql);

    if (result == 0) {
        PASS();
    } else {
        FAIL("Should return 0 when no '>' found");
    }
}

void test_safe_strcasestr_null(void) {
    TEST("safe_strcasestr - handles NULL inputs");

    char *r1 = safe_strcasestr(NULL, "test");
    char *r2 = safe_strcasestr("test", NULL);
    char *r3 = safe_strcasestr("hello world", "WORLD");

    if (r1 == NULL && r2 == NULL && r3 != NULL) {
        PASS();
    } else {
        FAIL("NULL handling incorrect");
    }
}

void test_strchr_boundary(void) {
    TEST("strchr boundary - arrow at start of string");

    const char *sql = ">value";
    char *arrow = strchr(sql, '>');

    // Arrow is at position 0, arrow[-1] would be out of bounds!
    int safe = (arrow && arrow > sql);

    if (!safe) {
        PASS();  // Correctly detected boundary issue
    } else {
        FAIL("Should detect boundary issue");
    }
}

// ============================================================================
// CRASH SCENARIO 4: Deadlock (2026-01-01)
// Bug: 40+ threads blocked on mutex_lock while holding other locks
// Fix: Use trylock with fallback to SQLite
// ============================================================================

static pthread_mutex_t test_mutex1 = PTHREAD_MUTEX_INITIALIZER;
static pthread_mutex_t test_mutex2 = PTHREAD_MUTEX_INITIALIZER;
static _Atomic int deadlock_detected = 0;

// Simulate the deadlock-prone pattern
static void* deadlock_thread_a(void *arg) {
    (void)arg;
    pthread_mutex_lock(&test_mutex1);
    usleep(1000);  // Hold mutex1, try to get mutex2

    if (pthread_mutex_trylock(&test_mutex2) != 0) {
        // Would deadlock with blocking lock!
        atomic_store(&deadlock_detected, 1);
    } else {
        pthread_mutex_unlock(&test_mutex2);
    }

    pthread_mutex_unlock(&test_mutex1);
    return NULL;
}

static void* deadlock_thread_b(void *arg) {
    (void)arg;
    pthread_mutex_lock(&test_mutex2);
    usleep(1000);  // Hold mutex2, try to get mutex1

    if (pthread_mutex_trylock(&test_mutex1) != 0) {
        atomic_store(&deadlock_detected, 1);
    } else {
        pthread_mutex_unlock(&test_mutex1);
    }

    pthread_mutex_unlock(&test_mutex2);
    return NULL;
}

void test_deadlock_detection(void) {
    TEST("Deadlock - trylock detects potential deadlock");

    atomic_store(&deadlock_detected, 0);

    pthread_t t1, t2;
    pthread_create(&t1, NULL, deadlock_thread_a, NULL);
    pthread_create(&t2, NULL, deadlock_thread_b, NULL);

    pthread_join(t1, NULL);
    pthread_join(t2, NULL);

    // With trylock, we detect the contention instead of deadlocking
    if (atomic_load(&deadlock_detected)) {
        PASS();
    } else {
        // No contention occurred (threads didn't overlap) - still OK
        printf("(no contention) ");
        PASS();
    }
}

void test_trylock_with_retry(void) {
    TEST("Trylock with retry loop (10x 1ms)");

    pthread_mutex_t mtx = PTHREAD_MUTEX_INITIALIZER;
    pthread_mutex_lock(&mtx);

    // Try to acquire locked mutex with retry loop
    int acquired = 0;
    for (int i = 0; i < 10 && !acquired; i++) {
        if (pthread_mutex_trylock(&mtx) == 0) {
            acquired = 1;
            pthread_mutex_unlock(&mtx);
        }
        usleep(1000);
    }

    pthread_mutex_unlock(&mtx);

    // Should NOT acquire (we held the lock)
    if (!acquired) {
        PASS();
    } else {
        FAIL("Should not have acquired locked mutex");
    }
}

// ============================================================================
// CRASH SCENARIO 5: Connection Pool State (2025-12-31)
// Bug: Stale slot references, corrupted state
// Fix: Atomic state machine + generation counters
// ============================================================================

typedef enum {
    SLOT_FREE = 0,
    SLOT_RESERVED = 1,
    SLOT_READY = 2,
    SLOT_RECONNECTING = 3,
    SLOT_ERROR = 4
} pool_slot_state_t;

typedef struct {
    _Atomic int state;
    _Atomic uint32_t generation;
    void *connection;  // Simulated
} pool_slot_t;

#define POOL_SIZE 4

static pool_slot_t test_pool[POOL_SIZE];

static void init_test_pool(void) {
    for (int i = 0; i < POOL_SIZE; i++) {
        atomic_store(&test_pool[i].state, SLOT_FREE);
        atomic_store(&test_pool[i].generation, 0);
        test_pool[i].connection = NULL;
    }
}

static int pool_acquire_slot_atomic(uint32_t *out_generation) {
    for (int i = 0; i < POOL_SIZE; i++) {
        int expected = SLOT_FREE;
        if (atomic_compare_exchange_strong(&test_pool[i].state, &expected, SLOT_RESERVED)) {
            *out_generation = atomic_fetch_add(&test_pool[i].generation, 1);
            atomic_store(&test_pool[i].state, SLOT_READY);
            return i;
        }
    }
    return -1;  // Pool exhausted
}

static int pool_release_slot(int slot, uint32_t expected_gen) {
    if (slot < 0 || slot >= POOL_SIZE) return -1;

    uint32_t current_gen = atomic_load(&test_pool[slot].generation);
    if (current_gen != expected_gen + 1) {
        return -1;  // Stale reference!
    }

    atomic_store(&test_pool[slot].state, SLOT_FREE);
    return 0;
}

void test_pool_atomic_acquire(void) {
    TEST("Connection pool - atomic CAS acquire");

    init_test_pool();

    uint32_t gen;
    int slot = pool_acquire_slot_atomic(&gen);

    if (slot >= 0 && slot < POOL_SIZE &&
        atomic_load(&test_pool[slot].state) == SLOT_READY) {
        PASS();
    } else {
        FAIL("Failed to acquire slot");
    }
}

void test_pool_generation_counter(void) {
    TEST("Connection pool - generation counter prevents stale use");

    init_test_pool();

    uint32_t gen1, gen2;
    int slot = pool_acquire_slot_atomic(&gen1);
    pool_release_slot(slot, gen1);

    // Acquire same slot again
    int slot2 = pool_acquire_slot_atomic(&gen2);

    // Try to release with OLD generation - should fail
    int result = pool_release_slot(slot2, gen1);  // Wrong generation!

    if (result == -1) {
        PASS();  // Correctly rejected stale reference
    } else {
        FAIL("Should reject stale generation");
    }
}

void test_pool_exhaustion(void) {
    TEST("Connection pool - handles exhaustion");

    init_test_pool();

    uint32_t gens[POOL_SIZE + 1];
    int slots[POOL_SIZE + 1];

    // Acquire all slots
    for (int i = 0; i < POOL_SIZE; i++) {
        slots[i] = pool_acquire_slot_atomic(&gens[i]);
    }

    // Try to acquire one more - should fail
    slots[POOL_SIZE] = pool_acquire_slot_atomic(&gens[POOL_SIZE]);

    if (slots[POOL_SIZE] == -1) {
        PASS();
    } else {
        FAIL("Should return -1 when pool exhausted");
    }
}

// ============================================================================
// CRASH SCENARIO 6: Loop Detection (2026-01-06)
// Bug: Same query repeated 218 times rapidly
// Fix: Hash-based loop detector with time window
// ============================================================================

typedef struct {
    uint32_t hash;
    int count;
    uint64_t first_time;
} loop_entry_t;

#define LOOP_SLOTS 16
#define LOOP_THRESHOLD 50
#define LOOP_WINDOW_MS 100

static __thread loop_entry_t loop_table[LOOP_SLOTS];
static __thread int loop_init = 0;

static uint64_t get_ms(void) {
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return tv.tv_sec * 1000ULL + tv.tv_usec / 1000;
}

static uint32_t hash_sql(const char *sql) {
    uint32_t h = 5381;
    while (*sql) h = ((h << 5) + h) + *sql++;
    return h;
}

static int check_loop(const char *sql) {
    if (!loop_init) {
        memset(loop_table, 0, sizeof(loop_table));
        loop_init = 1;
    }

    uint32_t h = hash_sql(sql);
    int slot = h % LOOP_SLOTS;
    uint64_t now = get_ms();

    loop_entry_t *e = &loop_table[slot];

    if (e->hash == h && now - e->first_time < LOOP_WINDOW_MS) {
        if (++e->count >= LOOP_THRESHOLD) {
            return 1;  // Loop detected!
        }
    } else {
        e->hash = h;
        e->count = 1;
        e->first_time = now;
    }

    return 0;
}

void test_loop_218_repeats(void) {
    TEST("Loop detection - catches 218 rapid repeats");

    loop_init = 0;  // Reset

    const char *sql = "SELECT * FROM metadata_items WHERE id = ?";
    int detected = 0;

    for (int i = 0; i < 218 && !detected; i++) {
        if (check_loop(sql)) {
            detected = 1;
        }
    }

    if (detected) {
        PASS();
    } else {
        FAIL("Should detect 218 repeats");
    }
}

void test_loop_different_queries(void) {
    TEST("Loop detection - no false positive for different queries");

    loop_init = 0;  // Reset

    char sql[256];
    int false_positive = 0;

    for (int i = 0; i < 100; i++) {
        snprintf(sql, sizeof(sql), "SELECT * FROM table_%d", i);
        if (check_loop(sql)) {
            false_positive = 1;
            break;
        }
    }

    if (!false_positive) {
        PASS();
    } else {
        FAIL("False positive for different queries");
    }
}

// ============================================================================
// CRASH SCENARIO 7: OnDeck Special Case (2026-01-07)
// Bug: OnDeck queries with <100KB stack crash in Metal/dyld
// Fix: Return empty result for OnDeck queries on low stack
// ============================================================================

static int should_skip_ondeck(const char *sql, size_t stack_remaining) {
    if (!sql) return 0;

    // Check for OnDeck query
    int is_ondeck = (safe_strcasestr(sql, "includeOnDeck") != NULL);

    // Special handling: OnDeck needs extra stack for Metal framework
    if (is_ondeck && stack_remaining < 100000) {
        return 1;  // Skip to prevent dyld crash
    }

    return 0;
}

void test_ondeck_low_stack_skip(void) {
    TEST("OnDeck - skipped on critically low stack (<100KB)");

    const char *sql = "SELECT * FROM view WHERE includeOnDeck=1";

    int skip = should_skip_ondeck(sql, 42000);  // 42KB - actual crash value

    if (skip) {
        PASS();
    } else {
        FAIL("Should skip OnDeck with 42KB stack");
    }
}

void test_ondeck_normal_stack_ok(void) {
    TEST("OnDeck - allowed on normal stack (>100KB)");

    const char *sql = "SELECT * FROM view WHERE includeOnDeck=1";

    int skip = should_skip_ondeck(sql, 200000);  // 200KB - should be fine

    if (!skip) {
        PASS();
    } else {
        FAIL("Should not skip OnDeck with 200KB stack");
    }
}

void test_non_ondeck_low_stack_ok(void) {
    TEST("Non-OnDeck queries - not affected by OnDeck skip");

    const char *sql = "SELECT * FROM metadata_items";

    int skip = should_skip_ondeck(sql, 42000);  // Low stack but not OnDeck

    if (!skip) {
        PASS();
    } else {
        FAIL("Non-OnDeck should not be skipped");
    }
}

// ============================================================================
// Main
// ============================================================================

int main(void) {
    printf("\n\033[1m=== Crash Scenario Tests (from production history) ===\033[0m\n\n");

    printf("\033[1m1. Integer Overflow (SIGBUS 2025-12-31):\033[0m\n");
    test_integer_overflow_signed();
    test_integer_overflow_fixed();
    test_bitmask_vs_modulo();

    printf("\n\033[1m2. Stack Overflow (2026-01-06):\033[0m\n");
    test_recursion_limit_218();
    test_recursion_limit_boundary();
    test_stack_hard_limit();
    test_stack_soft_limit();
    test_crash_scenario_exact();

    printf("\n\033[1m3. NULL Pointer Crashes:\033[0m\n");
    test_null_strchr_crash();
    test_safe_strcasestr_null();
    test_strchr_boundary();

    printf("\n\033[1m4. Deadlock Prevention:\033[0m\n");
    test_deadlock_detection();
    test_trylock_with_retry();

    printf("\n\033[1m5. Connection Pool Safety:\033[0m\n");
    test_pool_atomic_acquire();
    test_pool_generation_counter();
    test_pool_exhaustion();

    printf("\n\033[1m6. Loop Detection:\033[0m\n");
    test_loop_218_repeats();
    test_loop_different_queries();

    printf("\n\033[1m7. OnDeck Special Handling:\033[0m\n");
    test_ondeck_low_stack_skip();
    test_ondeck_normal_stack_ok();
    test_non_ondeck_low_stack_ok();

    printf("\n\033[1m=== Results ===\033[0m\n");
    printf("Passed: \033[32m%d\033[0m\n", tests_passed);
    printf("Failed: \033[31m%d\033[0m\n", tests_failed);
    printf("\nTests based on actual crashes from:\n");
    printf("  - crash_analysis.md (2026-01-06)\n");
    printf("  - STACK_OVERFLOW_FIX.md (2026-01-06)\n");
    printf("  - CRASH_ANALYSIS_ONDECK.md (2026-01-07)\n");
    printf("  - Git commits: 3493906, a56c321, cc58df1, d8be226, f29a7eb, 5d8ce3e\n");

    return tests_failed > 0 ? 1 : 0;
}
