/*
 * Benchmark: SQL translation time
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include "sql_translator.h"

#define NUM_QUERIES 10
#define ITERATIONS 100000

const char *test_queries[NUM_QUERIES] = {
    "SELECT * FROM metadata_items WHERE id = ?",
    "SELECT id, title, rating FROM metadata_items WHERE parent_id = ? ORDER BY \"index\"",
    "INSERT INTO metadata_items (title, added_at) VALUES (?, ?)",
    "UPDATE metadata_items SET updated_at = ? WHERE id = ?",
    "SELECT * FROM media_parts WHERE media_item_id = ? COLLATE NOCASE",
    "SELECT id, title FROM metadata_items WHERE title LIKE ? COLLATE NOCASE",
    "SELECT * FROM metadata_items WHERE added_at > strftime('%s', 'now', '-7 days')",
    "SELECT iif(rating > 5, 'good', 'bad') FROM metadata_items WHERE id = ?",
    "SELECT IFNULL(title, 'Unknown') FROM metadata_items",
    "SELECT * FROM metadata_items WHERE id IN (?, ?, ?) ORDER BY RANDOM() LIMIT 10",
};

int main(void) {
    sql_translator_init();
    
    printf("SQL Translation Benchmark\n");
    printf("=========================\n\n");
    
    // Warmup
    for (int i = 0; i < NUM_QUERIES; i++) {
        sql_translation_t result = sql_translate(test_queries[i]);
        sql_translation_free(&result);
    }
    
    // Benchmark each query
    long long total_ns = 0;
    
    for (int q = 0; q < NUM_QUERIES; q++) {
        struct timespec start, end;
        clock_gettime(CLOCK_MONOTONIC, &start);
        
        for (int i = 0; i < ITERATIONS; i++) {
            sql_translation_t result = sql_translate(test_queries[q]);
            sql_translation_free(&result);
        }
        
        clock_gettime(CLOCK_MONOTONIC, &end);
        long long elapsed_ns = (end.tv_sec - start.tv_sec) * 1000000000LL + (end.tv_nsec - start.tv_nsec);
        double avg_ns = (double)elapsed_ns / ITERATIONS;
        total_ns += elapsed_ns;
        
        printf("Query %d: %7.1f ns/op  (%.60s...)\n", q+1, avg_ns, test_queries[q]);
    }
    
    double overall_avg = (double)total_ns / (NUM_QUERIES * ITERATIONS);
    printf("\n");
    printf("Overall average: %.1f ns/op\n", overall_avg);
    printf("Total ops:       %d\n", NUM_QUERIES * ITERATIONS);
    
    // Compare with cache lookup (simulated)
    printf("\n--- With cache (simulated 28ns lookup) ---\n");
    printf("Cache hit:  28 ns/op\n");
    printf("Cache miss: %.1f ns/op (translate) + 28 ns (store) = %.1f ns\n", 
           overall_avg, overall_avg + 28);
    printf("\nSpeedup with 100%% cache hit: %.1fx\n", overall_avg / 28.0);
    printf("Speedup with 95%% cache hit:  %.1fx\n", overall_avg / (0.05 * overall_avg + 0.95 * 28.0));
    
    sql_translator_cleanup();
    return 0;
}
