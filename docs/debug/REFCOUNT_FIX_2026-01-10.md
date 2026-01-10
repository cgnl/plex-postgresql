# Reference Counting Bug Fix - 2026-01-10

## Root Cause Analysis

The crash was caused by incorrect reference counting in statement finalization. The code uses two caches:
1. **Global registry**: Hash table for all statements (weak references - no ref_count change)
2. **TLS cache**: Thread-local cache for frequently used statements (strong references - increments ref_count)

### Reference Counting Model

- `pg_stmt_create()` → ref_count = 1 (initial owner's reference)
- `pg_register_stmt()` → no ref_count change (weak reference)
- `pg_register_cached_stmt()` → ref_count++ (strong reference)
- `pg_unregister_stmt()` → no ref_count change (weak)
- `pg_clear_cached_stmt()` → ref_count-- (releases strong reference)

### The Bug

There were TWO bugs in `my_sqlite3_finalize()`:

#### Bug 1: Statement in both global + TLS caches
When a statement from prepare() was later added to TLS cache:
1. `pg_stmt_create()` → ref=1
2. `pg_register_stmt()` → ref=1 (no change, weak)
3. `pg_register_cached_stmt()` → ref=2 (increments for TLS)
4. `my_sqlite3_finalize()`:
   - Found in global registry
   - Called `pg_clear_cached_stmt_weak()` → ref=2 (BUG: didn't decrement TLS ref!)
   - Called `pg_stmt_unref()` → ref=1
   - **Statement leaked with ref=1**

#### Bug 2: Statement only in TLS cache
When step() created a TLS-only statement:
1. `pg_stmt_create()` → ref=1
2. `pg_register_cached_stmt()` → ref=2 (BUG: increments even though TLS is the only owner!)
3. `my_sqlite3_finalize()`:
   - NOT found in global registry
   - Called `pg_clear_cached_stmt()` → ref=1
   - **Statement leaked with ref=1**

### The Fix

Modified `my_sqlite3_finalize()` in `/Users/sander/plex-postgresql/src/db_interpose_step.c`:

**For statements in global registry:**
- Check if also in TLS cache
- If yes, call `pg_clear_cached_stmt()` (not weak) to properly decrement the TLS reference
- Then unregister from global and unref once more

**For TLS-only statements:**
- Call `pg_clear_cached_stmt()` to remove from TLS (unrefs once: ref 2→1)
- Call `pg_stmt_unref()` again to actually free (unrefs again: ref 1→0)

This compensates for the extra increment in `pg_register_cached_stmt()` that happens even when the TLS cache is the sole owner.

## Code Changes

File: `src/db_interpose_step.c`

```c
int my_sqlite3_finalize(sqlite3_stmt *pStmt) {
    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    int is_pg_only = 0;

    if (pg_stmt) {
        // Check if this is a PostgreSQL-only statement before cleaning up
        is_pg_only = (pg_stmt->is_pg == 2);

        // Statement is in global registry
        // Check if it's also in TLS cache - if so, need to decrement the TLS reference too
        pg_stmt_t *cached = pg_find_cached_stmt(pStmt);
        if (cached == pg_stmt) {
            // Same statement in both caches - TLS added an extra reference
            // Use normal clear to properly decrement the ref_count
            LOG_DEBUG("finalize: stmt in both global and TLS, clearing TLS ref");
            pg_clear_cached_stmt(pStmt);
        } else if (cached) {
            // Different statement in TLS cache (shouldn't happen, but be defensive)
            LOG_ERROR("finalize: BUG - different pg_stmt in global vs TLS for same sqlite_stmt!");
            pg_clear_cached_stmt(pStmt);
        }

        pg_unregister_stmt(pStmt);
        pg_stmt_unref(pg_stmt);
    } else {
        // Statement might only be in TLS cache - check it too
        pg_stmt_t *cached = pg_find_cached_stmt(pStmt);
        if (cached) {
            is_pg_only = (cached->is_pg == 2);
            // TLS-only statement - but pg_register_cached_stmt incremented ref_count
            // so it's now at 2 instead of 1. Need to unref twice.
            LOG_DEBUG("finalize: stmt only in TLS (ref_count=%d), clearing",
                     atomic_load(&cached->ref_count));
            pg_clear_cached_stmt(pStmt);  // This unrefs once
            pg_stmt_unref(cached);         // Unref again to actually free
        }
    }

    // If this was a PostgreSQL-only statement, don't call real SQLite
    if (is_pg_only) {
        return SQLITE_OK;
    }

    return orig_sqlite3_finalize ? orig_sqlite3_finalize(pStmt) : SQLITE_ERROR;
}
```

## Testing Plan

1. Run Plex with DEBUG logging enabled
2. Monitor for ERROR messages about ref_count anomalies
3. Check that statements are properly freed (no leaks)
4. Verify no crashes from double-free or use-after-free

## Future Improvements

The dual cache system with mixed weak/strong references is confusing. Consider:

1. **Option A**: Make TLS cache also use weak references
   - Don't increment ref_count in `pg_register_cached_stmt()`
   - Don't decrement in `pg_clear_cached_stmt()`
   - Simpler, but need to ensure statements aren't freed while in TLS cache

2. **Option B**: Make global registry also use strong references
   - Increment ref_count in `pg_register_stmt()`
   - Decrement in `pg_unregister_stmt()`
   - More symmetric, but changes more code

3. **Option C**: Start ref_count at 0 instead of 1
   - First cache that adds it increments to 1
   - Last cache that removes it decrements to 0 and frees
   - Most correct, but requires changing `pg_stmt_create()` semantics
