/*
 * Micro-benchmark: Measure individual operation costs
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <sqlite3.h>

#define ITERATIONS 10000

static inline double now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1e9 + ts.tv_nsec;
}

int main(int argc, char **argv) {
    const char *db_path = argc > 1 ? argv[1] : 
        "/Users/sander/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db";
    
    sqlite3 *db;
    if (sqlite3_open(db_path, &db) != SQLITE_OK) {
        fprintf(stderr, "Cannot open database\n");
        return 1;
    }
    
    const char *sql = "SELECT id, title FROM metadata_items WHERE id = ?";
    sqlite3_stmt *stmt;
    double start, total;
    
    // Warmup
    for (int i = 0; i < 1000; i++) {
        sqlite3_prepare_v2(db, sql, -1, &stmt, NULL);
        sqlite3_bind_int(stmt, 1, i);
        sqlite3_step(stmt);
        sqlite3_finalize(stmt);
    }
    
    printf("Micro-benchmark (%d iterations)\n", ITERATIONS);
    printf("================================\n\n");
    
    // Test 1: Just prepare
    start = now_ns();
    for (int i = 0; i < ITERATIONS; i++) {
        sqlite3_prepare_v2(db, sql, -1, &stmt, NULL);
        sqlite3_finalize(stmt);
    }
    total = now_ns() - start;
    printf("1. prepare + finalize:     %6.1f ns/op\n", total / ITERATIONS);
    
    // Test 2: Prepare once, then bind+step+reset many times
    sqlite3_prepare_v2(db, sql, -1, &stmt, NULL);
    start = now_ns();
    for (int i = 0; i < ITERATIONS; i++) {
        sqlite3_bind_int(stmt, 1, i % 1000 + 1);
        sqlite3_step(stmt);
        sqlite3_reset(stmt);
    }
    total = now_ns() - start;
    printf("2. bind + step + reset:    %6.1f ns/op\n", total / ITERATIONS);
    sqlite3_finalize(stmt);
    
    // Test 3: Just step (no results expected for high IDs)
    sqlite3_prepare_v2(db, sql, -1, &stmt, NULL);
    sqlite3_bind_int(stmt, 1, 999999999);  // Non-existent ID
    start = now_ns();
    for (int i = 0; i < ITERATIONS; i++) {
        sqlite3_step(stmt);
        sqlite3_reset(stmt);
    }
    total = now_ns() - start;
    printf("3. step + reset (no row):  %6.1f ns/op\n", total / ITERATIONS);
    sqlite3_finalize(stmt);
    
    // Test 4: Step with actual row
    sqlite3_prepare_v2(db, sql, -1, &stmt, NULL);
    sqlite3_bind_int(stmt, 1, 1);  // ID=1 should exist
    start = now_ns();
    for (int i = 0; i < ITERATIONS; i++) {
        sqlite3_step(stmt);
        sqlite3_reset(stmt);
    }
    total = now_ns() - start;
    printf("4. step + reset (with row):%6.1f ns/op\n", total / ITERATIONS);
    sqlite3_finalize(stmt);
    
    // Test 5: Full cycle with same prepared statement
    sqlite3_prepare_v2(db, sql, -1, &stmt, NULL);
    start = now_ns();
    for (int i = 0; i < ITERATIONS; i++) {
        sqlite3_bind_int(stmt, 1, i % 1000 + 1);
        while (sqlite3_step(stmt) == SQLITE_ROW) { }
        sqlite3_reset(stmt);
    }
    total = now_ns() - start;
    printf("5. bind+step(loop)+reset:  %6.1f ns/op\n", total / ITERATIONS);
    sqlite3_finalize(stmt);
    
    sqlite3_close(db);
    return 0;
}
