/*
 * Unit tests for Query Cache
 *
 * Tests:
 * 1. FNV-1a hash function properties
 * 2. Cache key consistency
 * 3. TTL expiration logic
 * 4. Reference counting safety
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <time.h>
#include <unistd.h>

// Test counters
static int tests_passed = 0;
static int tests_failed = 0;

#define TEST(name) printf("  Testing: %s... ", name)
#define PASS() do { printf("\033[32mPASS\033[0m\n"); tests_passed++; } while(0)
#define FAIL(msg) do { printf("\033[31mFAIL: %s\033[0m\n", msg); tests_failed++; } while(0)

// ============================================================================
// Replicate FNV-1a hash from pg_query_cache.c
// ============================================================================

static uint64_t fnv1a_hash(const void *data, size_t len) {
    const uint8_t *bytes = (const uint8_t *)data;
    uint64_t hash = 0xcbf29ce484222325ULL;  // FNV offset basis
    for (size_t i = 0; i < len; i++) {
        hash ^= bytes[i];
        hash *= 0x100000001b3ULL;  // FNV prime
    }
    return hash;
}

// ============================================================================
// Hash Function Tests
// ============================================================================

static void test_hash_consistency(void) {
    TEST("Hash - same input gives same hash");

    const char *sql = "SELECT * FROM metadata_items WHERE id = $1";
    uint64_t hash1 = fnv1a_hash(sql, strlen(sql));
    uint64_t hash2 = fnv1a_hash(sql, strlen(sql));

    if (hash1 == hash2) {
        PASS();
    } else {
        FAIL("Hash not consistent");
    }
}

static void test_hash_different_inputs(void) {
    TEST("Hash - different inputs give different hashes");

    const char *sql1 = "SELECT * FROM metadata_items WHERE id = $1";
    const char *sql2 = "SELECT * FROM metadata_items WHERE id = $2";

    uint64_t hash1 = fnv1a_hash(sql1, strlen(sql1));
    uint64_t hash2 = fnv1a_hash(sql2, strlen(sql2));

    if (hash1 != hash2) {
        PASS();
    } else {
        FAIL("Different inputs produced same hash");
    }
}

static void test_hash_distribution(void) {
    TEST("Hash - good distribution across buckets");

    // Generate 1000 different SQL queries and check bucket distribution
    #define NUM_QUERIES 1000
    #define NUM_BUCKETS 64

    int buckets[NUM_BUCKETS] = {0};
    char sql[256];

    for (int i = 0; i < NUM_QUERIES; i++) {
        snprintf(sql, sizeof(sql), "SELECT * FROM table_%d WHERE id = %d", i, i * 17);
        uint64_t hash = fnv1a_hash(sql, strlen(sql));
        int bucket = hash % NUM_BUCKETS;
        buckets[bucket]++;
    }

    // Check that no bucket has more than 3x the average
    int avg = NUM_QUERIES / NUM_BUCKETS;  // ~15
    int max_allowed = avg * 3;  // ~45
    int max_found = 0;

    for (int i = 0; i < NUM_BUCKETS; i++) {
        if (buckets[i] > max_found) max_found = buckets[i];
    }

    if (max_found <= max_allowed) {
        PASS();
    } else {
        FAIL("Poor hash distribution");
    }
}

static void test_hash_empty_string(void) {
    TEST("Hash - empty string handled");

    uint64_t hash = fnv1a_hash("", 0);

    // Should return the offset basis for empty input
    if (hash == 0xcbf29ce484222325ULL) {
        PASS();
    } else {
        FAIL("Empty string hash incorrect");
    }
}

static void test_hash_single_char_difference(void) {
    TEST("Hash - single char difference produces different hash");

    const char *sql1 = "SELECT * FROM a";
    const char *sql2 = "SELECT * FROM b";

    uint64_t hash1 = fnv1a_hash(sql1, strlen(sql1));
    uint64_t hash2 = fnv1a_hash(sql2, strlen(sql2));

    if (hash1 != hash2) {
        PASS();
    } else {
        FAIL("Single char difference not detected");
    }
}

// ============================================================================
// Cache Key Tests (simulate cache key computation)
// ============================================================================

static uint64_t compute_cache_key(const char *sql, const char **params, int param_count) {
    // Replicate logic from pg_query_cache.c
    uint64_t hash = fnv1a_hash(sql, strlen(sql));

    for (int i = 0; i < param_count; i++) {
        if (params[i]) {
            uint64_t param_hash = fnv1a_hash(params[i], strlen(params[i]));
            hash ^= param_hash;
            hash *= 0x100000001b3ULL;
        }
    }

    return hash;
}

static void test_cache_key_with_params(void) {
    TEST("Cache key - includes bound parameters");

    const char *sql = "SELECT * FROM t WHERE id = $1";
    const char *params1[] = {"100"};
    const char *params2[] = {"200"};

    uint64_t key1 = compute_cache_key(sql, params1, 1);
    uint64_t key2 = compute_cache_key(sql, params2, 1);

    if (key1 != key2) {
        PASS();
    } else {
        FAIL("Different params should give different keys");
    }
}

static void test_cache_key_same_params(void) {
    TEST("Cache key - same SQL+params gives same key");

    const char *sql = "SELECT * FROM t WHERE id = $1 AND name = $2";
    const char *params[] = {"100", "test"};

    uint64_t key1 = compute_cache_key(sql, params, 2);
    uint64_t key2 = compute_cache_key(sql, params, 2);

    if (key1 == key2) {
        PASS();
    } else {
        FAIL("Same SQL+params should give same key");
    }
}

static void test_cache_key_null_param(void) {
    TEST("Cache key - handles NULL parameters");

    const char *sql = "SELECT * FROM t WHERE id = $1";
    const char *params1[] = {NULL};
    const char *params2[] = {"100"};

    uint64_t key1 = compute_cache_key(sql, params1, 1);
    uint64_t key2 = compute_cache_key(sql, params2, 1);

    // Should produce different keys
    if (key1 != key2) {
        PASS();
    } else {
        FAIL("NULL param should differ from non-NULL");
    }
}

// ============================================================================
// TTL Logic Tests
// ============================================================================

#define QUERY_CACHE_TTL_MS 1000

static int is_cache_expired(uint64_t created_ms, uint64_t now_ms) {
    return (now_ms - created_ms) > QUERY_CACHE_TTL_MS;
}

static void test_ttl_not_expired(void) {
    TEST("TTL - entry not expired within window");

    uint64_t created = 1000000;
    uint64_t now = created + 500;  // 500ms later

    if (!is_cache_expired(created, now)) {
        PASS();
    } else {
        FAIL("Entry should not be expired at 500ms");
    }
}

static void test_ttl_expired(void) {
    TEST("TTL - entry expired after window");

    uint64_t created = 1000000;
    uint64_t now = created + 1500;  // 1500ms later

    if (is_cache_expired(created, now)) {
        PASS();
    } else {
        FAIL("Entry should be expired at 1500ms");
    }
}

static void test_ttl_boundary(void) {
    TEST("TTL - boundary at exactly TTL");

    uint64_t created = 1000000;
    uint64_t now = created + QUERY_CACHE_TTL_MS + 1;  // Just past TTL

    if (is_cache_expired(created, now)) {
        PASS();
    } else {
        FAIL("Entry should be expired just past TTL");
    }
}

// ============================================================================
// Reference Counting Tests
// ============================================================================

typedef struct {
    int ref_count;
    int freed;
} mock_cached_result_t;

static void mock_release(mock_cached_result_t *entry) {
    if (!entry) return;
    entry->ref_count--;
    if (entry->ref_count <= 0) {
        entry->freed = 1;
    }
}

static void test_refcount_basic(void) {
    TEST("RefCount - decrement on release");

    mock_cached_result_t entry = {.ref_count = 2, .freed = 0};

    mock_release(&entry);

    if (entry.ref_count == 1 && !entry.freed) {
        PASS();
    } else {
        FAIL("RefCount should be 1, not freed");
    }
}

static void test_refcount_free_at_zero(void) {
    TEST("RefCount - free when reaching zero");

    mock_cached_result_t entry = {.ref_count = 1, .freed = 0};

    mock_release(&entry);

    if (entry.ref_count == 0 && entry.freed) {
        PASS();
    } else {
        FAIL("Should be freed at ref_count 0");
    }
}

static void test_refcount_multiple_refs(void) {
    TEST("RefCount - multiple references prevent free");

    mock_cached_result_t entry = {.ref_count = 3, .freed = 0};

    mock_release(&entry);  // 3 -> 2
    mock_release(&entry);  // 2 -> 1

    if (entry.ref_count == 1 && !entry.freed) {
        PASS();
    } else {
        FAIL("Should not be freed with ref_count > 0");
    }

    mock_release(&entry);  // 1 -> 0, should free

    if (entry.freed) {
        // Already passed above
    }
}

// ============================================================================
// LRU Eviction Tests
// ============================================================================

#define CACHE_SIZE 4

typedef struct {
    uint64_t cache_key;
    uint64_t created_ms;
    int hit_count;
} mock_cache_entry_t;

static int find_lru_slot(mock_cache_entry_t *entries, int count) {
    // Find oldest entry (lowest created_ms with lowest hit_count as tiebreaker)
    int lru_idx = 0;
    uint64_t oldest = entries[0].created_ms;
    int lowest_hits = entries[0].hit_count;

    for (int i = 1; i < count; i++) {
        if (entries[i].created_ms < oldest ||
            (entries[i].created_ms == oldest && entries[i].hit_count < lowest_hits)) {
            lru_idx = i;
            oldest = entries[i].created_ms;
            lowest_hits = entries[i].hit_count;
        }
    }

    return lru_idx;
}

static void test_lru_finds_oldest(void) {
    TEST("LRU - finds oldest entry");

    mock_cache_entry_t entries[CACHE_SIZE] = {
        {.cache_key = 1, .created_ms = 1000, .hit_count = 5},
        {.cache_key = 2, .created_ms = 500,  .hit_count = 2},  // Oldest
        {.cache_key = 3, .created_ms = 1500, .hit_count = 1},
        {.cache_key = 4, .created_ms = 2000, .hit_count = 3},
    };

    int lru = find_lru_slot(entries, CACHE_SIZE);

    if (lru == 1) {
        PASS();
    } else {
        FAIL("Should find slot 1 as oldest");
    }
}

static void test_lru_tiebreaker_hits(void) {
    TEST("LRU - uses hit_count as tiebreaker");

    mock_cache_entry_t entries[CACHE_SIZE] = {
        {.cache_key = 1, .created_ms = 1000, .hit_count = 5},
        {.cache_key = 2, .created_ms = 1000, .hit_count = 2},  // Same age, fewer hits
        {.cache_key = 3, .created_ms = 1000, .hit_count = 10},
        {.cache_key = 4, .created_ms = 1000, .hit_count = 3},
    };

    int lru = find_lru_slot(entries, CACHE_SIZE);

    if (lru == 1) {
        PASS();
    } else {
        FAIL("Should find slot 1 with fewest hits");
    }
}

// ============================================================================
// Main
// ============================================================================

int main(void) {
    printf("\n\033[1m=== Query Cache Tests ===\033[0m\n\n");

    printf("\033[1mHash Function:\033[0m\n");
    test_hash_consistency();
    test_hash_different_inputs();
    test_hash_distribution();
    test_hash_empty_string();
    test_hash_single_char_difference();

    printf("\n\033[1mCache Key Computation:\033[0m\n");
    test_cache_key_with_params();
    test_cache_key_same_params();
    test_cache_key_null_param();

    printf("\n\033[1mTTL Expiration:\033[0m\n");
    test_ttl_not_expired();
    test_ttl_expired();
    test_ttl_boundary();

    printf("\n\033[1mReference Counting:\033[0m\n");
    test_refcount_basic();
    test_refcount_free_at_zero();
    test_refcount_multiple_refs();

    printf("\n\033[1mLRU Eviction:\033[0m\n");
    test_lru_finds_oldest();
    test_lru_tiebreaker_hits();

    printf("\n\033[1m=== Results ===\033[0m\n");
    printf("Passed: \033[32m%d\033[0m\n", tests_passed);
    printf("Failed: \033[31m%d\033[0m\n", tests_failed);
    printf("\n");

    return tests_failed > 0 ? 1 : 0;
}
