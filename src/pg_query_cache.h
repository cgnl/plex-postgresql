/*
 * PostgreSQL Shim - Query Result Cache
 * 
 * Caches query results for identical queries within a short time window.
 * This is critical for Plex's OnDeck endpoint which executes the same query
 * 2000+ times in a loop.
 *
 * Design:
 * - Thread-local cache (no locking needed)
 * - Cache key: hash of (SQL + bound parameters)
 * - Cache TTL: 1 second (data freshness)
 * - LRU eviction when cache is full
 */

#ifndef PG_QUERY_CACHE_H
#define PG_QUERY_CACHE_H

#include <stdint.h>
#include <libpq-fe.h>
#include "pg_types.h"  // For cached_result_t, cached_row_t, pg_stmt_t

// Cache configuration
#define QUERY_CACHE_SIZE 64         // Number of cached queries per thread
#define QUERY_CACHE_TTL_MS 1000     // Cache TTL in milliseconds (1 second)
#define QUERY_CACHE_MAX_ROWS 5      // Don't cache results with more than this many rows (TEST: tiny)
#define QUERY_CACHE_MAX_BYTES 1024*1024  // Max total cached bytes per entry (1MB)

// Thread-local cache structure
typedef struct {
    cached_result_t entries[QUERY_CACHE_SIZE];
    int count;
    uint64_t total_hits;
    uint64_t total_misses;
} query_cache_t;

// Initialize/cleanup
void pg_query_cache_init(void);
void pg_query_cache_cleanup(void);

// Cache operations
// Returns cached result if found and not expired, NULL otherwise
cached_result_t* pg_query_cache_lookup(pg_stmt_t *stmt);

// Store result in cache (makes a copy of all data)
// PGresult* is from libpq - passed as void* to avoid header dependency
void pg_query_cache_store(pg_stmt_t *stmt, void *result);

// Invalidate cache entry for a statement (call on reset/finalize)
void pg_query_cache_invalidate(pg_stmt_t *stmt);

// Compute cache key from statement SQL and bound parameters
uint64_t pg_query_cache_key(pg_stmt_t *stmt);

// Get stats (for logging)
void pg_query_cache_stats(uint64_t *hits, uint64_t *misses);

#endif // PG_QUERY_CACHE_H
