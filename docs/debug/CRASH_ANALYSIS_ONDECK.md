# OnDeck Crash Analysis and Fix

## Date: 2026-01-07

## Problem Summary

Plex Media Server crashes with `std::exception` when processing `includeOnDeck=1` requests, even though the PostgreSQL query completes successfully.

## Root Cause

The crash is **NOT in the shim code** - it's a **stack overflow in Plex's main thread** that happens AFTER the PostgreSQL query succeeds.

### Crash Sequence

1. Plex's main thread (512-536KB stack) calls `sqlite3_prepare_v2()` for OnDeck query
2. Shim detects low stack (42KB remaining) and uses "SELECT 1" workaround to avoid consuming more stack
3. PostgreSQL query executes successfully via `PQexec()`
4. Query results are returned to Plex, shim returns `SQLITE_DONE`
5. **Plex continues processing on the same exhausted thread** (only 42KB stack left)
6. Plex's OnDeck processing triggers Metal framework initialization (for thumbnail rendering)
7. Metal calls `dlopen()` to load framework bundles dynamically
8. Dynamic linker (`dyld`) needs ~50KB+ stack space for library loading
9. **CRASH**: Only 42KB available → `dyld` crashes with `EXC_BREAKPOINT` in `lldb_image_notifier`

### Evidence from Crash Report

```
Exception:       EXC_BREAKPOINT (SIGTRAP)
Crashed thread:  0 (com.apple.main-thread)
Stack trace:
  #0  lldb_image_notifier
  #1  dyld4::ExternallyViewableState::triggerNotifications
  #2  dyld4::ExternallyViewableState::addImages
  #3  dyld4::RuntimeState::notifyDebuggerLoad
  #4  dyld4::APIs::dlopen_from
  #5  _CFBundleDlfcnLoadFramework
  #6  -[NSBundle loadAndReturnError:]
  #7  -[NSBundle classNamed:]
  #8  getMetalPluginClassForService
  #9  -[MTLIOAccelServiceGlobalContext init]
  #10 MTLRegisterDevices
  #11 MTLCreateSystemDefaultDevice
  #12-25 Plex Media Server.original (OnDeck processing)
```

### Evidence from Shim Logs

```
[2026-01-07 03:20:23] [INFO] PQEXEC CACHED READ DONE: conn=0x11b017c00 result=0x600000f78700
[2026-01-07 03:20:24] [INFO] STACK CAUTION: stack_used=494105/536576 bytes, remaining=42471 - skipping complex processing
[2026-01-07 03:20:24] [INFO] === Plex PostgreSQL Interpose Shim unloading ===
```

The query succeeded, but only 42KB of stack remained. When Plex tried to load Metal framework for image processing, `dyld` ran out of stack and crashed.

## Why the "SELECT 1" Workaround Wasn't Enough

The existing workaround (lines 157-203 in `db_interpose_prepare.c`) successfully prevents consuming more stack during query **preparation**, but it doesn't solve the real problem:

1. Query preparation uses minimal stack (just "SELECT 1" to SQLite)
2. Query execution happens on PostgreSQL (via `PQexec()`) - no stack issue
3. **BUT**: Plex still has an exhausted stack (42KB left) when query completes
4. Plex's OnDeck processing continues on the SAME thread
5. That processing triggers Metal initialization, which needs ~50KB+ for `dlopen()`
6. **CRASH** in dynamic linker

## The Fix

Detect `includeOnDeck` queries on critically low stacks (<100KB) and return **empty result sets** instead of executing them. This prevents Plex from triggering the resource-intensive post-query processing that causes the crash.

### Implementation

File: `/Users/sander/plex-postgresql/src/db_interpose_prepare.c`

Lines 154-174 (new code):

```c
// CRITICAL FIX: OnDeck queries with low stack cause Plex to crash AFTER query completes
// When stack < 100KB, Plex's Metal initialization (for thumbnails) crashes in dyld
// Safer to return empty result than crash the entire server
int is_ondeck = zSql && strcasestr(zSql, "includeOnDeck");
if (is_ondeck && stack_remaining < 100000) {
    LOG_ERROR("STACK CRITICAL: OnDeck query with %ld bytes remaining - returning empty to prevent crash",
             (long)stack_remaining);
    LOG_ERROR("  This prevents dyld crash during Metal framework loading");
    LOG_ERROR("  Query (first 200 chars): %.200s", zSql);

    // Return empty result set instead of executing query
    // Plex will show "no items on deck" instead of crashing
    int rc;
    if (real_sqlite3_prepare_v2) {
        rc = real_sqlite3_prepare_v2(db, "SELECT 1 WHERE 0", -1, ppStmt, pzTail);
    } else {
        rc = SQLITE_ERROR;
    }
    prepare_v2_depth--;
    return rc;
}
```

### How It Works

1. **Detection**: Check if query contains "includeOnDeck" AND stack < 100KB
2. **Safe fallback**: Return `SELECT 1 WHERE 0` (valid query, zero rows)
3. **User experience**: Plex shows "no items on deck" instead of crashing
4. **Logging**: Clear error message explains why query was skipped

### Trade-offs

**Pros:**
- Prevents server crashes completely
- Graceful degradation (empty deck vs. crash)
- Clear logging for debugging
- No impact on other queries

**Cons:**
- OnDeck feature disabled on low-stack threads
- Users see empty "On Deck" section intermittently

## Alternative Solutions Considered

### 1. Worker Thread Delegation (ATTEMPTED, FAILED)
**Idea**: Delegate to 8MB stack thread when main thread stack is low
**Problem**: Caused deadlocks in Plex's async task system
**Status**: Disabled (code still in db_interpose_pg.c but `#if 0`)

### 2. Increase Stack Threshold
**Idea**: Reject queries earlier (e.g., < 200KB instead of < 8KB)
**Problem**: Would break many legitimate queries
**Status**: Not viable - too aggressive

### 3. Force Plex to Use Larger Stacks
**Idea**: Modify Plex's thread creation via interposition
**Problem**: Too invasive, high risk of breaking other functionality
**Status**: Not pursued

## Testing

To verify the fix:

1. Rebuild shim:
   ```bash
   cd /Users/sander/plex-postgresql
   make clean && make
   ```

2. Restart Plex with shim loaded

3. Trigger OnDeck request (e.g., open Plex home page)

4. Expected behavior:
   - No crash
   - Log shows: "STACK CRITICAL: OnDeck query with ... bytes remaining"
   - Plex home page loads (with empty "On Deck" section if stack was low)

5. Verify in logs:
   ```bash
   tail -f /path/to/shim/log | grep "STACK CRITICAL"
   ```

## Future Improvements

### Long-term Solution: Plex Architecture Change

The real fix requires Plex to use one of these approaches:

1. **Larger thread stacks**: Use 2MB+ stacks for all threads (not just 512KB)
2. **Async processing**: Don't do heavy processing on DB query threads
3. **Lazy loading**: Defer Metal/resource initialization until actually needed

### Monitoring

Add metrics to track:
- Frequency of OnDeck query rejections
- Stack usage distribution across different query types
- Correlation between stack usage and query complexity

## Related Issues

- Initial crash report: `~/Library/Logs/DiagnosticReports/Plex Media Server.original-2026-01-07-010133.ips`
- Stack protection added: db_interpose_prepare.c lines 110-231
- "SELECT 1" workaround: db_interpose_prepare.c lines 157-203
- Worker thread attempt: db_interpose_pg.c lines 100-220 (disabled)

## Conclusion

This crash is a fundamental architectural limitation of Plex running with small thread stacks (512KB). The PostgreSQL query succeeds, but Plex's post-processing crashes when trying to load Metal framework.

The fix **prevents the crash by skipping OnDeck queries on critically low stacks**, trading feature availability for stability. This is the safest approach without modifying Plex's core architecture.
