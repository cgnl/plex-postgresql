/*
 * Benchmark: Direct shim performance (bypasses psycopg2)
 * Tests actual sqlite3_* API call overhead through our shim
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <sqlite3.h>

#define ITERATIONS 10000

// Test queries (typical Plex patterns)
const char *TEST_QUERIES[] = {
    "SELECT * FROM metadata_items WHERE id = ?",
    "SELECT id, title, rating FROM metadata_items WHERE parent_id = ? ORDER BY \"index\"",
    "INSERT INTO metadata_items (title, added_at) VALUES (?, ?)",
    "UPDATE metadata_items SET updated_at = ? WHERE id = ?",
    "SELECT id, title FROM metadata_items WHERE title LIKE ? COLLATE NOCASE",
};
#define NUM_QUERIES 5

double benchmark_prepare_step(sqlite3 *db) {
    struct timespec start, end;
    clock_gettime(CLOCK_MONOTONIC, &start);
    
    for (int i = 0; i < ITERATIONS; i++) {
        const char *sql = TEST_QUERIES[i % NUM_QUERIES];
        sqlite3_stmt *stmt;
        
        int rc = sqlite3_prepare_v2(db, sql, -1, &stmt, NULL);
        if (rc != SQLITE_OK) {
            fprintf(stderr, "Prepare failed: %s\n", sqlite3_errmsg(db));
            continue;
        }
        
        // Bind a parameter
        sqlite3_bind_int(stmt, 1, i);
        
        // Step (execute)
        rc = sqlite3_step(stmt);
        // We expect SQLITE_ROW or SQLITE_DONE
        
        sqlite3_finalize(stmt);
    }
    
    clock_gettime(CLOCK_MONOTONIC, &end);
    return (end.tv_sec - start.tv_sec) * 1e9 + (end.tv_nsec - start.tv_nsec);
}

double benchmark_exec(sqlite3 *db) {
    struct timespec start, end;
    clock_gettime(CLOCK_MONOTONIC, &start);
    
    for (int i = 0; i < ITERATIONS; i++) {
        char sql[256];
        snprintf(sql, sizeof(sql), "SELECT * FROM metadata_items WHERE id = %d", i);
        
        char *err = NULL;
        sqlite3_exec(db, sql, NULL, NULL, &err);
        if (err) {
            sqlite3_free(err);
        }
    }
    
    clock_gettime(CLOCK_MONOTONIC, &end);
    return (end.tv_sec - start.tv_sec) * 1e9 + (end.tv_nsec - start.tv_nsec);
}

// Benchmark with same SQL (tests cache effectiveness)
double benchmark_exec_cached(sqlite3 *db) {
    struct timespec start, end;
    const char *sql = "SELECT id, title FROM metadata_items WHERE id = 1";
    
    clock_gettime(CLOCK_MONOTONIC, &start);
    
    for (int i = 0; i < ITERATIONS; i++) {
        char *err = NULL;
        sqlite3_exec(db, sql, NULL, NULL, &err);
        if (err) {
            sqlite3_free(err);
        }
    }
    
    clock_gettime(CLOCK_MONOTONIC, &end);
    return (end.tv_sec - start.tv_sec) * 1e9 + (end.tv_nsec - start.tv_nsec);
}

// Benchmark parameterized query (realistic usage pattern)
double benchmark_parameterized(sqlite3 *db) {
    struct timespec start, end;
    const char *sql = "SELECT id, title, rating FROM metadata_items WHERE id = ?";
    
    clock_gettime(CLOCK_MONOTONIC, &start);
    
    for (int i = 0; i < ITERATIONS; i++) {
        sqlite3_stmt *stmt;
        int rc = sqlite3_prepare_v2(db, sql, -1, &stmt, NULL);
        if (rc != SQLITE_OK) continue;
        
        sqlite3_bind_int(stmt, 1, i % 1000 + 1);  // Vary the ID
        sqlite3_step(stmt);
        sqlite3_finalize(stmt);
    }
    
    clock_gettime(CLOCK_MONOTONIC, &end);
    return (end.tv_sec - start.tv_sec) * 1e9 + (end.tv_nsec - start.tv_nsec);
}

int main(int argc, char **argv) {
    const char *db_path = argc > 1 ? argv[1] : 
        "/Users/sander/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db";
    
    printf("Shim Performance Benchmark\n");
    printf("==========================\n");
    printf("Database: %s\n", db_path);
    printf("Iterations: %d\n\n", ITERATIONS);
    
    sqlite3 *db;
    int rc = sqlite3_open(db_path, &db);
    if (rc != SQLITE_OK) {
        fprintf(stderr, "Cannot open database: %s\n", sqlite3_errmsg(db));
        return 1;
    }
    
    // Warmup
    printf("Warming up...\n");
    benchmark_prepare_step(db);
    benchmark_exec(db);
    
    // Benchmark prepare/bind/step/finalize cycle
    printf("\n1. Prepare/Bind/Step/Finalize cycle:\n");
    double ns1 = benchmark_prepare_step(db);
    printf("   Total: %.2f ms\n", ns1 / 1e6);
    printf("   Per op: %.1f us (%.0f ns)\n", ns1 / ITERATIONS / 1000, ns1 / ITERATIONS);
    printf("   Ops/sec: %.0f\n", ITERATIONS / (ns1 / 1e9));
    
    // Benchmark exec (simpler path) - varies SQL each time
    printf("\n2. sqlite3_exec (varying SQL):\n");
    double ns2 = benchmark_exec(db);
    printf("   Total: %.2f ms\n", ns2 / 1e6);
    printf("   Per op: %.1f us (%.0f ns)\n", ns2 / ITERATIONS / 1000, ns2 / ITERATIONS);
    printf("   Ops/sec: %.0f\n", ITERATIONS / (ns2 / 1e9));
    
    // Benchmark exec with same SQL (tests cache)
    printf("\n3. sqlite3_exec (same SQL - cache test):\n");
    double ns3 = benchmark_exec_cached(db);
    printf("   Total: %.2f ms\n", ns3 / 1e6);
    printf("   Per op: %.1f us (%.0f ns)\n", ns3 / ITERATIONS / 1000, ns3 / ITERATIONS);
    printf("   Ops/sec: %.0f\n", ITERATIONS / (ns3 / 1e9));
    
    // Benchmark parameterized query (realistic Plex pattern)
    printf("\n4. Parameterized query (prepare/bind/step - same SQL):\n");
    double ns4 = benchmark_parameterized(db);
    printf("   Total: %.2f ms\n", ns4 / 1e6);
    printf("   Per op: %.1f us (%.0f ns)\n", ns4 / ITERATIONS / 1000, ns4 / ITERATIONS);
    printf("   Ops/sec: %.0f\n", ITERATIONS / (ns4 / 1e9));
    
    sqlite3_close(db);
    
    printf("\n");
    return 0;
}
