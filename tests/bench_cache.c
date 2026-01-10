/*
 * Benchmark: mutex vs rwlock vs thread-local vs lock-free
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <stdint.h>
#include <time.h>
#include <stdatomic.h>

#define CACHE_SIZE 512
#define CACHE_MASK (CACHE_SIZE - 1)
#define NUM_THREADS 8
#define OPS_PER_THREAD 1000000
#define NUM_UNIQUE_QUERIES 100

// Simple hash
uint64_t hash_sql(const char *sql) {
    uint64_t h = 14695981039346656037ULL;
    while (*sql) {
        h ^= (uint64_t)*sql++;
        h *= 1099511628211ULL;
    }
    return h;
}

// =============================================================================
// 1. MUTEX CACHE
// =============================================================================
typedef struct {
    uint64_t hash;
    char *translated;
} mutex_cache_entry_t;

mutex_cache_entry_t mutex_cache[CACHE_SIZE];
pthread_mutex_t cache_mutex = PTHREAD_MUTEX_INITIALIZER;

const char* mutex_lookup(const char *sql) {
    uint64_t h = hash_sql(sql);
    int idx = h & CACHE_MASK;
    
    pthread_mutex_lock(&cache_mutex);
    const char *result = NULL;
    for (int i = 0; i < 8; i++) {
        int slot = (idx + i) & CACHE_MASK;
        if (mutex_cache[slot].hash == h) {
            result = mutex_cache[slot].translated;
            break;
        }
        if (mutex_cache[slot].hash == 0) break;
    }
    pthread_mutex_unlock(&cache_mutex);
    return result;
}

// =============================================================================
// 2. RWLOCK CACHE
// =============================================================================
mutex_cache_entry_t rwlock_cache[CACHE_SIZE];
pthread_rwlock_t cache_rwlock = PTHREAD_RWLOCK_INITIALIZER;

const char* rwlock_lookup(const char *sql) {
    uint64_t h = hash_sql(sql);
    int idx = h & CACHE_MASK;
    
    pthread_rwlock_rdlock(&cache_rwlock);
    const char *result = NULL;
    for (int i = 0; i < 8; i++) {
        int slot = (idx + i) & CACHE_MASK;
        if (rwlock_cache[slot].hash == h) {
            result = rwlock_cache[slot].translated;
            break;
        }
        if (rwlock_cache[slot].hash == 0) break;
    }
    pthread_rwlock_unlock(&cache_rwlock);
    return result;
}

// =============================================================================
// 3. THREAD-LOCAL CACHE
// =============================================================================
typedef struct {
    uint64_t hash;
    char *translated;
} tls_cache_entry_t;

__thread tls_cache_entry_t tls_cache[CACHE_SIZE];
__thread int tls_initialized = 0;

const char* tls_lookup(const char *sql) {
    uint64_t h = hash_sql(sql);
    int idx = h & CACHE_MASK;
    
    for (int i = 0; i < 8; i++) {
        int slot = (idx + i) & CACHE_MASK;
        if (tls_cache[slot].hash == h) {
            return tls_cache[slot].translated;
        }
        if (tls_cache[slot].hash == 0) break;
    }
    return NULL;
}

// =============================================================================
// 4. LOCK-FREE CACHE (simplified)
// =============================================================================
typedef struct {
    _Atomic uint64_t hash;
    _Atomic uintptr_t translated;
} lockfree_cache_entry_t;

lockfree_cache_entry_t lockfree_cache[CACHE_SIZE];

const char* lockfree_lookup(const char *sql) {
    uint64_t h = hash_sql(sql);
    int idx = h & CACHE_MASK;
    
    for (int i = 0; i < 8; i++) {
        int slot = (idx + i) & CACHE_MASK;
        uint64_t slot_hash = atomic_load_explicit(&lockfree_cache[slot].hash, memory_order_acquire);
        if (slot_hash == h) {
            return (const char*)atomic_load_explicit(&lockfree_cache[slot].translated, memory_order_acquire);
        }
        if (slot_hash == 0) break;
    }
    return NULL;
}

// =============================================================================
// TEST SETUP
// =============================================================================
char *test_queries[NUM_UNIQUE_QUERIES];
char *test_translations[NUM_UNIQUE_QUERIES];

void setup_test_data(void) {
    for (int i = 0; i < NUM_UNIQUE_QUERIES; i++) {
        char buf[128];
        snprintf(buf, sizeof(buf), "SELECT * FROM table%d WHERE id = ?", i);
        test_queries[i] = strdup(buf);
        snprintf(buf, sizeof(buf), "SELECT * FROM plex.table%d WHERE id = $1", i);
        test_translations[i] = strdup(buf);
    }
    
    // Pre-populate caches
    for (int i = 0; i < NUM_UNIQUE_QUERIES; i++) {
        uint64_t h = hash_sql(test_queries[i]);
        int idx = h & CACHE_MASK;
        
        // Find free slot
        for (int j = 0; j < 8; j++) {
            int slot = (idx + j) & CACHE_MASK;
            if (mutex_cache[slot].hash == 0) {
                mutex_cache[slot].hash = h;
                mutex_cache[slot].translated = test_translations[i];
                rwlock_cache[slot].hash = h;
                rwlock_cache[slot].translated = test_translations[i];
                atomic_store(&lockfree_cache[slot].hash, h);
                atomic_store(&lockfree_cache[slot].translated, (uintptr_t)test_translations[i]);
                break;
            }
        }
    }
}

// =============================================================================
// BENCHMARK THREADS
// =============================================================================
typedef const char* (*lookup_fn)(const char*);

typedef struct {
    lookup_fn fn;
    int thread_id;
    long long elapsed_ns;
    int hits;
} thread_arg_t;

void* bench_thread(void *arg) {
    thread_arg_t *ta = (thread_arg_t*)arg;
    
    // For TLS, need to populate per-thread cache
    if (ta->fn == tls_lookup) {
        for (int i = 0; i < NUM_UNIQUE_QUERIES; i++) {
            uint64_t h = hash_sql(test_queries[i]);
            int idx = h & CACHE_MASK;
            for (int j = 0; j < 8; j++) {
                int slot = (idx + j) & CACHE_MASK;
                if (tls_cache[slot].hash == 0) {
                    tls_cache[slot].hash = h;
                    tls_cache[slot].translated = test_translations[i];
                    break;
                }
            }
        }
    }
    
    struct timespec start, end;
    clock_gettime(CLOCK_MONOTONIC, &start);
    
    int hits = 0;
    for (int i = 0; i < OPS_PER_THREAD; i++) {
        const char *sql = test_queries[i % NUM_UNIQUE_QUERIES];
        const char *result = ta->fn(sql);
        if (result) hits++;
    }
    
    clock_gettime(CLOCK_MONOTONIC, &end);
    
    ta->elapsed_ns = (end.tv_sec - start.tv_sec) * 1000000000LL + (end.tv_nsec - start.tv_nsec);
    ta->hits = hits;
    return NULL;
}

void run_benchmark(const char *name, lookup_fn fn) {
    pthread_t threads[NUM_THREADS];
    thread_arg_t args[NUM_THREADS];
    
    for (int i = 0; i < NUM_THREADS; i++) {
        args[i].fn = fn;
        args[i].thread_id = i;
        pthread_create(&threads[i], NULL, bench_thread, &args[i]);
    }
    
    long long total_ns = 0;
    int total_hits = 0;
    for (int i = 0; i < NUM_THREADS; i++) {
        pthread_join(threads[i], NULL);
        total_ns += args[i].elapsed_ns;
        total_hits += args[i].hits;
    }
    
    long long total_ops = (long long)NUM_THREADS * OPS_PER_THREAD;
    double avg_ns = (double)total_ns / total_ops;
    double mops = (double)total_ops / (total_ns / (double)NUM_THREADS) * 1000.0;
    
    printf("%-15s: %6.1f ns/op, %6.1f M ops/sec, hits=%d\n", name, avg_ns, mops, total_hits);
}

int main(void) {
    setup_test_data();
    
    printf("Cache benchmark: %d threads, %d ops/thread, %d unique queries\n\n",
           NUM_THREADS, OPS_PER_THREAD, NUM_UNIQUE_QUERIES);
    
    // Warmup
    run_benchmark("warmup", mutex_lookup);
    printf("\n");
    
    // Real benchmarks
    run_benchmark("mutex", mutex_lookup);
    run_benchmark("rwlock", rwlock_lookup);
    run_benchmark("thread-local", tls_lookup);
    run_benchmark("lock-free", lockfree_lookup);
    
    return 0;
}
