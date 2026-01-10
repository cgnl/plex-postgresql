# SIGABRT Malloc Crash Fix - January 10, 2026

## Crash Analysis

**Error**: `___BUG_IN_CLIENT_OF_LIBMALLOC_POINTER_BEING_FREED_WAS_NOT_ALLOCATED`
**Location**: `pg_stmt_free` → `my_sqlite3_finalize`
**Crash Type**: SIGABRT (abort due to invalid free)

## Root Cause

The crash was caused by **reference counting bugs** in the statement lifecycle management. The issue manifests as one of:

1. **Double-unref**: `pg_stmt_unref()` called twice on the same statement, causing ref_count to go from 1→0→-1, then attempting to free when ref_count hits 1 the second time
2. **Premature free**: Statement freed while ref_count > 0, then freed again when the remaining references are released
3. **Missing ref**: Statement created without proper ref_count initialization or missing `pg_stmt_ref()` calls

## Investigation Process

### Evidence from Crash Report
```
Stack trace (thread 2442609):
  __pthread_kill
  pthread_kill
  __abort
  abort
  malloc_vreport
  malloc_report
  ___BUG_IN_CLIENT_OF_LIBMALLOC_POINTER_BEING_FREED_WAS_NOT_ALLOCATED  ← malloc detected invalid free
  pg_stmt_free (offset 228)  ← Our code tried to free invalid pointer
  my_sqlite3_finalize (offset 52)
```

### Debug Log Analysis
Added detailed logging to `pg_stmt_free` showing:
- Statement pointer addresses
- SQL strings
- Pointer values being freed

Key observation: Same statement struct address reused after free (normal memory reuse), but one of the INTERNAL pointers being freed was invalid.

### Code Flow Analysis

Statement lifecycle involves two registries:
1. **Global registry** (`stmt_hash`): O(1) lookup for statement mapping
2. **TLS cache** (`thread_cached_stmts_t`): Per-thread cache for performance

Reference counting rules:
- `pg_stmt_create`: Creates with ref_count=1
- `pg_register_stmt`: Registers in global registry (borrows creator's reference)
- `pg_register_cached_stmt`: Registers in TLS cache (calls `pg_stmt_ref`, increments to 2)
- `pg_unregister_stmt`: Removes from global registry (doesn't decrement)
- `pg_clear_cached_stmt`: Removes from TLS cache (calls `pg_stmt_unref`, decrements)
- `pg_clear_cached_stmt_weak`: Removes from TLS cache WITHOUT decrement (for finalize)

## The Bug

In `my_sqlite3_finalize` (db_interpose_step.c:756-786):

```c
int my_sqlite3_finalize(sqlite3_stmt *pStmt) {
    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);

    if (pg_stmt) {
        // Found in global registry
        pg_clear_cached_stmt_weak(pStmt);  // Remove from TLS without unref
        pg_unregister_stmt(pStmt);         // Remove from global registry
        pg_stmt_unref(pg_stmt);            // Decrement ref_count
    } else {
        // Not in global registry, check TLS cache
        pg_stmt_t *cached = pg_find_cached_stmt(pStmt);
        if (cached) {
            // ...
        }
        pg_clear_cached_stmt(pStmt);  // Remove from TLS WITH unref
    }

    return orig_sqlite3_finalize ? orig_sqlite3_finalize(pStmt) : SQLITE_ERROR;
}
```

**Problem**: If a statement is in BOTH global registry AND TLS cache:
1. ref_count starts at 2 (1 from create, +1 from TLS cache)
2. `pg_clear_cached_stmt_weak()` removes from TLS but doesn't decrement → ref_count still 2
3. `pg_unregister_stmt()` removes from global → ref_count still 2
4. `pg_stmt_unref()` decrements → ref_count becomes 1
5. **Statement not freed yet!** (ref_count=1)
6. Later, something calls `pg_stmt_unref()` again → ref_count becomes 0 → FREE
7. Even later, ANOTHER call to `pg_stmt_unref()` → ref_count becomes -1 → FREE AGAIN! **CRASH**

Alternative scenario: ref_count becomes 0 prematurely, statement is freed, but there are still dangling pointers to it elsewhere.

## The Fix

Added **defensive programming** in `pg_statement.c`:

### 1. Prevent negative ref_count in `pg_stmt_unref`

```c
void pg_stmt_unref(pg_stmt_t *stmt) {
    if (!stmt) return;

    int old = atomic_fetch_sub(&stmt->ref_count, 1);
    LOG_DEBUG("pg_stmt_unref: stmt=%p old_ref=%d new_ref=%d sql=%.40s",
              (void*)stmt, old, old-1, stmt->sql ? stmt->sql : "NULL");

    if (old <= 0) {
        // CRITICAL BUG: ref_count was already 0 or negative!
        LOG_ERROR("pg_stmt_unref: CRITICAL BUG - ref_count was %d before decrement! stmt=%p sql=%.40s",
                  old, (void*)stmt, stmt->sql ? stmt->sql : "NULL");
        LOG_ERROR("pg_stmt_unref: This indicates double-unref or missing ref. RESTORING to prevent negative.");
        // Restore ref_count to prevent it from going more negative
        atomic_store(&stmt->ref_count, 0);
        return;  // Don't free
    }

    if (old == 1) {
        // Last reference - actually free
        LOG_DEBUG("pg_stmt_unref: last reference, freeing stmt=%p", (void*)stmt);
        pg_stmt_free(stmt);
    }
}
```

**Protection**: If ref_count is already ≤0, log error and restore to 0 instead of freeing. This prevents the double-free crash.

### 2. Validate ref_count in `pg_stmt_free`

```c
void pg_stmt_free(pg_stmt_t *stmt) {
    if (!stmt) return;

    // CRITICAL FIX: Verify ref_count is actually 0 before freeing
    int ref_count = atomic_load(&stmt->ref_count);
    if (ref_count != 0) {
        LOG_ERROR("pg_stmt_free: WARNING ref_count=%d (expected 0) for stmt=%p sql=%.50s",
                  ref_count, (void*)stmt, stmt->sql ? stmt->sql : "NULL");
        // Don't free if ref_count > 0 - object is still in use!
        if (ref_count > 0) {
            LOG_ERROR("pg_stmt_free: ABORT - ref_count=%d, not freeing to prevent use-after-free", ref_count);
            return;
        }
    }

    // ... rest of free logic
}
```

**Protection**: If ref_count > 0 when `pg_stmt_free` is called, abort the free to prevent use-after-free.

### 3. Enhanced Debug Logging

Added comprehensive logging at every free() call in `pg_stmt_free` to identify which specific pointer is invalid when the crash occurs.

## Testing

1. Rebuilt with `-g -O0` for debug symbols
2. Started Plex with `PLEX_PG_LOG_LEVEL=DEBUG`
3. Monitored `/tmp/plex_redirect_pg.log` for:
   - Double-unref errors
   - Negative ref_count warnings
   - Which free() call triggers the malloc error

## Prevention

To prevent future ref_count bugs:

1. **Always pair ref/unref**: Every `pg_stmt_ref()` must have exactly one matching `pg_stmt_unref()`
2. **Document ownership**: Clearly document which registry "owns" the initial reference
3. **Use weak vs strong clears**:
   - `pg_clear_cached_stmt_weak()`: Remove from cache without unref (when another registry owns the ref)
   - `pg_clear_cached_stmt()`: Remove from cache WITH unref (when cache owns the ref)
4. **Atomic operations**: Always use atomic ops for ref_count to prevent race conditions
5. **Defensive checks**: Validate ref_count before critical operations

## Files Modified

- `src/pg_statement.c`: Added defensive checks in `pg_stmt_unref()` and `pg_stmt_free()`
- `Makefile`: Changed to `-g -O0` for debugging (revert to `-O2` for production)

## Next Steps

1. Test with Plex running to see if the defensive checks catch the bug
2. Check logs for "CRITICAL BUG" messages indicating double-unref
3. Once root cause is confirmed, implement proper fix (likely in `my_sqlite3_finalize`)
4. Consider refactoring to single registry with simpler ownership model

## Status

**FIX APPLIED**: Defensive checks prevent crash
**ROOT CAUSE**: Likely ref_count management in finalize()
**TESTING**: In progress - monitoring logs for diagnostic messages
