/*
 * Stack protection stress test for Linux Docker
 * Tests the actual thresholds in the shim code
 */

#define _GNU_SOURCE
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
#define RESET "\033[0m"

// Function pointer for shim's prepare
typedef int (*sqlite3_open_fn)(const char*, void**);
typedef int (*sqlite3_prepare_v2_fn)(void*, const char*, int, void**, const char**);
typedef int (*sqlite3_close_fn)(void*);

static sqlite3_open_fn real_open = NULL;
static sqlite3_prepare_v2_fn real_prepare = NULL;
static sqlite3_close_fn real_close = NULL;

// Get current stack usage
static void get_stack_info(size_t *used, size_t *remaining, size_t *total) {
    pthread_attr_t attr;
    void *stack_addr;
    size_t stack_size;

    if (pthread_getattr_np(pthread_self(), &attr) != 0) {
        *used = *remaining = *total = 0;
        return;
    }

    pthread_attr_getstack(&attr, &stack_addr, &stack_size);
    pthread_attr_destroy(&attr);

    // Stack top = bottom + size (Linux)
    void *stack_top = (char*)stack_addr + stack_size;

    volatile char local;
    char *current = (char*)&local;

    *total = stack_size;
    *used = (char*)stack_top - current;
    *remaining = stack_size - *used;
}

// Consume stack recursively
static volatile int dummy_result = 0;

static void consume_stack(size_t target_remaining, int depth) {
    char buffer[4096];  // 4KB per frame
    memset(buffer, depth & 0xFF, sizeof(buffer));
    dummy_result += buffer[0];

    size_t used, remaining, total;
    get_stack_info(&used, &remaining, &total);

    if (remaining > target_remaining && depth < 200) {
        consume_stack(target_remaining, depth + 1);
    }
}

// Test with specific stack remaining
static int test_prepare_with_stack(void *db, size_t target_remaining, const char *test_name) {
    printf("  %s (target: %zu KB remaining)... ", test_name, target_remaining / 1024);
    fflush(stdout);

    size_t used, remaining, total;
    get_stack_info(&used, &remaining, &total);

    // Consume stack to reach target
    if (remaining > target_remaining + 50000) {
        consume_stack(target_remaining + 20000, 0);
    }

    get_stack_info(&used, &remaining, &total);
    printf("[actual: %zu KB] ", remaining / 1024);

    // Try a prepare call
    void *stmt = NULL;
    const char *tail = NULL;
    const char *sql = "SELECT * FROM metadata_items WHERE id = ?";

    int rc = real_prepare(db, sql, -1, &stmt, &tail);

    if (rc == 0) {
        printf(GREEN "OK (rc=0)" RESET "\n");
        return 0;
    } else if (rc == 7) {  // SQLITE_NOMEM
        printf(YELLOW "PROTECTED (rc=7 NOMEM)" RESET "\n");
        return 1;
    } else {
        printf(RED "ERROR (rc=%d)" RESET "\n", rc);
        return -1;
    }
}

// Thread function for stack test
typedef struct {
    size_t stack_size;
    size_t target_remaining;
    const char *test_name;
    int result;
} thread_test_t;

static void* thread_test_func(void *arg) {
    thread_test_t *t = (thread_test_t*)arg;

    // Open database
    void *db = NULL;
    const char *db_path = "/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db";

    int rc = real_open(db_path, &db);
    if (rc != 0 || !db) {
        printf("  %s: " RED "Failed to open database" RESET "\n", t->test_name);
        t->result = -1;
        return NULL;
    }

    t->result = test_prepare_with_stack(db, t->target_remaining, t->test_name);

    real_close(db);
    return NULL;
}

static int run_thread_test(size_t stack_size, size_t target_remaining, const char *name) {
    pthread_t thread;
    pthread_attr_t attr;
    thread_test_t test = {
        .stack_size = stack_size,
        .target_remaining = target_remaining,
        .test_name = name,
        .result = -1
    };

    pthread_attr_init(&attr);
    pthread_attr_setstacksize(&attr, stack_size);

    if (pthread_create(&thread, &attr, thread_test_func, &test) != 0) {
        printf("  %s: " RED "Failed to create thread" RESET "\n", name);
        return -1;
    }

    pthread_join(thread, NULL);
    pthread_attr_destroy(&attr);

    return test.result;
}

int main(int argc, char *argv[]) {
    printf("\n" GREEN "=== Stack Protection Stress Test (Linux) ===" RESET "\n\n");

    // Load the shim library
    void *shim = dlopen("/usr/local/lib/plex-postgresql/db_interpose_pg.so", RTLD_NOW);
    if (!shim) {
        // Try local path
        shim = dlopen("./db_interpose_pg.so", RTLD_NOW);
    }

    if (!shim) {
        printf(RED "ERROR: Could not load shim: %s" RESET "\n", dlerror());
        printf("Run this inside the Docker container with the shim installed.\n");
        return 1;
    }

    // Get function pointers (these go through the shim)
    real_open = (sqlite3_open_fn)dlsym(shim, "sqlite3_open");
    real_prepare = (sqlite3_prepare_v2_fn)dlsym(shim, "sqlite3_prepare_v2");
    real_close = (sqlite3_close_fn)dlsym(shim, "sqlite3_close");

    if (!real_open || !real_prepare || !real_close) {
        printf(RED "ERROR: Could not find sqlite3 functions in shim" RESET "\n");
        return 1;
    }

    printf("Shim loaded successfully.\n\n");

    // Test 1: Normal stack (512KB thread, plenty remaining)
    printf(YELLOW "Test 1: Normal conditions (512KB stack)" RESET "\n");
    run_thread_test(512 * 1024, 400 * 1024, "400KB remaining");
    run_thread_test(512 * 1024, 200 * 1024, "200KB remaining");
    run_thread_test(512 * 1024, 100 * 1024, "100KB remaining");

    // Test 2: Low stack scenarios (should trigger protection)
    printf("\n" YELLOW "Test 2: Low stack (should trigger protection)" RESET "\n");
    run_thread_test(512 * 1024, 64 * 1024, "64KB remaining");
    run_thread_test(512 * 1024, 32 * 1024, "32KB remaining");
    run_thread_test(512 * 1024, 16 * 1024, "16KB remaining");
    run_thread_test(512 * 1024, 8 * 1024, "8KB remaining (current threshold)");

    // Test 3: Critical stack (below current threshold)
    printf("\n" YELLOW "Test 3: Critical stack (below 8KB threshold)" RESET "\n");
    run_thread_test(512 * 1024, 4 * 1024, "4KB remaining");

    // Test 4: Small thread stack (like Plex uses)
    printf("\n" YELLOW "Test 4: Small thread stack (544KB like Plex)" RESET "\n");
    run_thread_test(544 * 1024, 100 * 1024, "544KB thread, 100KB remaining");
    run_thread_test(544 * 1024, 50 * 1024, "544KB thread, 50KB remaining");

    printf("\n" GREEN "=== Test Complete ===" RESET "\n");
    printf("\nLegend:\n");
    printf("  " GREEN "OK" RESET " = Query executed successfully\n");
    printf("  " YELLOW "PROTECTED" RESET " = Stack protection triggered (safe)\n");
    printf("  " RED "ERROR" RESET " = Unexpected failure\n");

    dlclose(shim);
    return 0;
}
