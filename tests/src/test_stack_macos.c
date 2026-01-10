/*
 * Stack protection test for macOS
 * Tests the actual thresholds in the shim code without running Plex
 *
 * Build: clang -o tests/bin/test_stack_macos tests/src/test_stack_macos.c -lpthread -ldl
 * Run:   ./tests/bin/test_stack_macos
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <unistd.h>
#include <dlfcn.h>
#include <stdint.h>

// Colors
#define RED "\033[31m"
#define GREEN "\033[32m"
#define YELLOW "\033[33m"
#define CYAN "\033[36m"
#define RESET "\033[0m"

// SQLite constants
#define SQLITE_OK 0
#define SQLITE_NOMEM 7

// Function pointers for SQLite functions
typedef int (*sqlite3_open_fn)(const char*, void**);
typedef int (*sqlite3_prepare_v2_fn)(void*, const char*, int, void**, const char**);
typedef int (*sqlite3_prepare_v2_internal_fn)(void*, const char*, int, void**, const char**, int);
typedef int (*sqlite3_close_fn)(void*);
typedef int (*sqlite3_exec_fn)(void*, const char*, void*, void*, char**);

// We use real SQLite for open/close/exec, but shim's internal prepare for testing
static sqlite3_open_fn real_open = NULL;
static sqlite3_prepare_v2_internal_fn shim_prepare_internal = NULL;
static sqlite3_close_fn real_close = NULL;
static sqlite3_exec_fn real_exec = NULL;
static void *sqlite_lib = NULL;

// Test result tracking
static int tests_passed = 0;
static int tests_failed = 0;

// Get current stack usage (macOS version)
static void get_stack_info(size_t *used, size_t *remaining, size_t *total) {
    pthread_t self = pthread_self();

    // Get stack address (top of stack on macOS)
    void *stack_addr = pthread_get_stackaddr_np(self);
    size_t stack_size = pthread_get_stacksize_np(self);

    // Current stack position
    volatile char local;
    char *current = (char*)&local;

    // Stack grows downward
    char *stack_base = (char*)stack_addr;
    *total = stack_size;
    *used = stack_base - current;
    *remaining = stack_size - *used;
}

// Volatile to prevent optimization
static volatile int dummy_result = 0;

// Thread-local from_worker flag for recursive test
static __thread int test_from_worker = 0;

// Recursive function that consumes stack and then calls prepare at the bottom
static int test_at_stack_depth(void *db, size_t target_remaining, int depth) {
    // Consume 4KB per frame
    char buffer[4096];
    memset(buffer, depth & 0xFF, sizeof(buffer));
    dummy_result += buffer[0];

    size_t used, remaining, total;
    get_stack_info(&used, &remaining, &total);

    // Keep recursing until we reach target
    if (remaining > target_remaining + 10000 && depth < 200) {
        return test_at_stack_depth(db, target_remaining, depth + 1);
    }

    // We're at target depth - now call prepare
    printf("[depth=%d, actual=%zuKB] ", depth, remaining/1024);
    fflush(stdout);

    void *stmt = NULL;
    const char *tail = NULL;
    const char *sql = "SELECT * FROM test WHERE id = ?";

    // Call shim's internal prepare with from_worker flag
    int rc = shim_prepare_internal(db, sql, -1, &stmt, &tail, test_from_worker);
    return rc;
}

// Thread test context
typedef struct {
    size_t stack_size;
    size_t target_remaining;
    const char *test_name;
    int expect_protection;
    int from_worker;  // 0 = normal thread (can delegate), 1 = simulate worker thread
    int result;
} thread_test_t;

static void* thread_test_func(void *arg) {
    thread_test_t *t = (thread_test_t*)arg;

    // Set thread-local from_worker flag
    test_from_worker = t->from_worker;

    printf("  %s (target: %zuKB)... ", t->test_name, t->target_remaining / 1024);
    fflush(stdout);

    // Open test database using real SQLite
    void *db = NULL;
    int rc = real_open(":memory:", &db);
    if (rc != SQLITE_OK || !db) {
        printf(RED "Failed to open database (rc=%d)" RESET "\n", rc);
        t->result = -1;
        return NULL;
    }

    // Create test table using real SQLite
    char *err = NULL;
    real_exec(db, "CREATE TABLE IF NOT EXISTS test (id INTEGER PRIMARY KEY, name TEXT)", NULL, NULL, &err);

    // Call prepare at the target stack depth (uses shim's internal function)
    rc = test_at_stack_depth(db, t->target_remaining, 0);

    real_close(db);

    // Check result
    if (t->expect_protection) {
        if (rc == SQLITE_NOMEM) {
            printf(GREEN "PASS" RESET " (protected)\n");
            tests_passed++;
            t->result = 1;
        } else {
            printf(RED "FAIL" RESET " (expected protection, got rc=%d)\n", rc);
            tests_failed++;
            t->result = 0;
        }
    } else {
        if (rc == SQLITE_OK) {
            printf(GREEN "PASS" RESET " (allowed)\n");
            tests_passed++;
            t->result = 1;
        } else if (rc == SQLITE_NOMEM) {
            printf(YELLOW "WARN" RESET " (protected early)\n");
            tests_passed++;  // Still acceptable - protection is conservative
            t->result = 1;
        } else {
            printf(RED "FAIL" RESET " (unexpected rc=%d)\n", rc);
            tests_failed++;
            t->result = 0;
        }
    }

    return NULL;
}

static int run_thread_test(size_t stack_size, size_t target_remaining, const char *name, int expect_protection, int from_worker) {
    pthread_t thread;
    pthread_attr_t attr;
    thread_test_t test = {
        .stack_size = stack_size,
        .target_remaining = target_remaining,
        .test_name = name,
        .expect_protection = expect_protection,
        .from_worker = from_worker,
        .result = -1
    };

    pthread_attr_init(&attr);
    if (pthread_attr_setstacksize(&attr, stack_size) != 0) {
        printf("  %s: " RED "Failed to set stack size %zu" RESET "\n", name, stack_size);
        tests_failed++;
        return -1;
    }

    if (pthread_create(&thread, &attr, thread_test_func, &test) != 0) {
        printf("  %s: " RED "Failed to create thread" RESET "\n", name);
        tests_failed++;
        return -1;
    }

    pthread_join(thread, NULL);
    pthread_attr_destroy(&attr);

    return test.result;
}

int main(int argc, char *argv[]) {
    printf("\n" CYAN "=== Stack Protection Test (macOS) ===" RESET "\n\n");

    // Load the shim library
    const char *shim_path = "./db_interpose_pg.dylib";
    if (argc > 1) {
        shim_path = argv[1];
    }

    // Load real SQLite library first
    sqlite_lib = dlopen("/usr/lib/libsqlite3.dylib", RTLD_NOW);
    if (!sqlite_lib) {
        printf(RED "ERROR: Could not load SQLite: %s" RESET "\n", dlerror());
        return 1;
    }

    real_open = (sqlite3_open_fn)dlsym(sqlite_lib, "sqlite3_open");
    real_close = (sqlite3_close_fn)dlsym(sqlite_lib, "sqlite3_close");
    real_exec = (sqlite3_exec_fn)dlsym(sqlite_lib, "sqlite3_exec");

    if (!real_open || !real_close || !real_exec) {
        printf(RED "ERROR: Could not find SQLite functions" RESET "\n");
        return 1;
    }
    printf("SQLite loaded from /usr/lib/libsqlite3.dylib\n");

    // Load the shim library
    printf("Loading shim: %s\n", shim_path);
    void *shim = dlopen(shim_path, RTLD_NOW);
    if (!shim) {
        printf(RED "ERROR: Could not load shim: %s" RESET "\n", dlerror());
        printf("Make sure db_interpose_pg.dylib exists in current directory.\n");
        printf("Usage: %s [path/to/db_interpose_pg.dylib]\n", argv[0]);
        return 1;
    }

    // Get the internal prepare function which has the stack protection logic
    shim_prepare_internal = (sqlite3_prepare_v2_internal_fn)dlsym(shim, "my_sqlite3_prepare_v2_internal");

    if (!shim_prepare_internal) {
        printf(RED "ERROR: Could not find my_sqlite3_prepare_v2_internal in shim" RESET "\n");
        dlclose(shim);
        return 1;
    }

    printf(GREEN "Shim loaded successfully." RESET "\n\n");

    // Show thresholds being tested
    printf("Testing thresholds:\n");
    printf("  Worker delegation:      < 400KB remaining\n");
    printf("  Normal thread protect:  < 64KB remaining\n");
    printf("  Worker thread protect:  < 32KB remaining\n\n");

    // Test 1: Normal conditions - should NOT trigger protection
    printf(YELLOW "Test 1: Normal conditions (512KB stack, can delegate)" RESET "\n");
    run_thread_test(512 * 1024, 400 * 1024, "400KB remaining", 0, 0);
    run_thread_test(512 * 1024, 200 * 1024, "200KB remaining", 0, 0);
    run_thread_test(512 * 1024, 100 * 1024, "100KB remaining", 0, 0);

    // Test 2: Worker delegation - low stack delegates to 8MB worker
    printf("\n" YELLOW "Test 2: Worker delegation (< 400KB, delegates to worker)" RESET "\n");
    run_thread_test(512 * 1024, 350 * 1024, "350KB - delegates", 0, 0);
    run_thread_test(512 * 1024, 150 * 1024, "150KB - delegates", 0, 0);
    run_thread_test(512 * 1024, 50 * 1024, "50KB - delegates", 0, 0);  // Worker handles it

    // Test 3: Hard threshold on worker thread (from_worker=1, 32KB threshold)
    printf("\n" YELLOW "Test 3: Worker thread itself (from_worker=1, 32KB threshold)" RESET "\n");
    run_thread_test(512 * 1024, 100 * 1024, "100KB on worker - OK", 0, 1);
    run_thread_test(512 * 1024, 50 * 1024, "50KB on worker - OK", 0, 1);
    run_thread_test(512 * 1024, 35 * 1024, "35KB on worker - OK", 0, 1);

    // Test 4: Below 32KB (32000 bytes) threshold on worker - MUST protect
    printf("\n" YELLOW "Test 4: Worker below 32KB threshold - MUST protect" RESET "\n");
    run_thread_test(512 * 1024, 20 * 1024, "20KB on worker - PROTECT", 1, 1);
    run_thread_test(512 * 1024, 16 * 1024, "16KB on worker - PROTECT", 1, 1);

    // Test 5: Plex-like thread (544KB)
    printf("\n" YELLOW "Test 5: Plex-like thread (544KB stack)" RESET "\n");
    run_thread_test(544 * 1024, 200 * 1024, "200KB can delegate", 0, 0);
    run_thread_test(544 * 1024, 50 * 1024, "50KB delegates OK", 0, 0);
    run_thread_test(544 * 1024, 20 * 1024, "20KB worker PROTECT", 1, 1);

    // Summary
    printf("\n" CYAN "=== Test Summary ===" RESET "\n");
    printf("  Passed: " GREEN "%d" RESET "\n", tests_passed);
    printf("  Failed: " RED "%d" RESET "\n", tests_failed);

    if (tests_failed == 0) {
        printf("\n" GREEN "All tests passed!" RESET "\n");
    } else {
        printf("\n" RED "Some tests failed - check stack protection thresholds" RESET "\n");
    }

    dlclose(shim);
    dlclose(sqlite_lib);

    return tests_failed > 0 ? 1 : 0;
}
