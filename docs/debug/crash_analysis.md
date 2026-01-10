# Plex Stack Overflow Crash Analysis - 2026-01-06 22:13:44

## CRITICAL FINDINGS

### 1. CRASH LOCATION
- **Thread**: 28 (PMS ReqHandler) - TRIGGERED CRASH
- **PC (Program Counter)**: 0x10652ae5c (sqlite3_str_vappendf+72)
- **Exception**: EXC_BAD_ACCESS (KERN_PROTECTION_FAILURE)
- **Address**: 0x000000016e4a3ff8
- **Error**: Data Abort - Translation fault (tried to write outside valid memory)

### 2. STACK GUARD VIOLATION
```
vmRegionInfo:
  Stack                       16e418000-16e4a0000    [  544K] rw-/rwx SM=PRV  thread 25
--->  STACK GUARD              16e4a0000-16e4a4000    [   16K] ---/rwx SM=NUL  stack guard for thread 28
  Stack                       16e4a4000-16e52c000    [  544K] rw-/rwx SM=PRV  thread 28
```

**CRITICAL**: 
- Thread 28 stack: 0x16e4a4000 - 0x16e52c000 (544KB total)
- Crash address: 0x16e4a3ff8 (IN THE STACK GUARD!)
- Stack guard: 0x16e4a0000 - 0x16e4a4000 (16KB protection zone)
- Stack pointer at crash: 0x16e4a2f10

**STACK EXHAUSTION**: The thread tried to write at 0x16e4a3ff8, which is INSIDE the 16KB stack guard zone BELOW the actual stack. This means the stack grew downward and hit the guard page!

### 3. CALL STACK AT CRASH (Thread 28 - PMS ReqHandler)

Frame breakdown (newest to oldest):
1. **sqlite3_str_vappendf+72** <- CRASH HERE (string formatting in SQLite)
2. **sqlite3 frame 128324** (twice - RECURSION!)
3. **sqlite3 frames** (400580, 400372, 416064, 403736, 531604, 534648, 480096...)
4. **MASSIVE RECURSION**: Frame 8445316 repeats **218 TIMES!!!**
5. **my_sqlite3_prepare_v2+1232** <- Our shim function
6. **soci::sqlite3_statement_backend::prepare+164**
7. **soci::details::statement_impl::statement_impl+340**
8. **Plex frames** (768076, 1173468, 1173356, 9050620...)

### 4. ROOT CAUSE ANALYSIS

**The stack overflow happens INSIDE SQLite's query parser, NOT in our code!**

Our stack protection check:
```c
// Line 447-455 in db_interpose_pg.c
ptrdiff_t stack_remaining = (ptrdiff_t)stack_size - stack_used;

if (stack_remaining < 200000) {  // 200KB threshold
    skip_complex_processing = 1;
    LOG_INFO("STACK PROTECTION: ...");
}
```

**PROBLEM**: The check happens at the START of my_sqlite3_prepare_v2, but:
1. We calculated stack_remaining correctly
2. We decided to skip_complex_processing or not
3. Then we called `sqlite3_prepare_v2(db, sql_for_sqlite, ...)`
4. SQLite's parser goes into DEEP RECURSION
5. SQLite exhausts the remaining stack!

**CALCULATION ERROR**: 
- Thread stack size: 544KB
- Stack pointer at crash: 0x16e4a2f10
- Stack base: 0x16e52c000
- Stack used: 0x16e52c000 - 0x16e4a2f10 = 0x890F0 = 561,392 bytes ≈ 548KB
- **WE OVERFLOWED THE 544KB STACK!**

### 5. WHY STACK PROTECTION DIDN'T WORK

The issue is that our check uses `pthread_get_stackaddr_np()` and `pthread_get_stacksize_np()`, but:

1. **Stack grows DOWNWARD** on ARM64 macOS
2. **Stack base** (pthread_get_stackaddr_np): 0x16e52c000 (top)
3. **Stack size**: 544KB
4. **Stack bottom**: 0x16e4a4000 (base - size)
5. **Stack guard**: 0x16e4a0000 - 0x16e4a4000 (BELOW stack)

Our calculation (line 441):
```c
ptrdiff_t stack_used = stack_base - current_stack;
```

This SHOULD be correct, BUT we're checking at the WRONG TIME!

**THE REAL PROBLEM**: 
- We check stack at entry to my_sqlite3_prepare_v2
- At that point, maybe 300KB used, 244KB remaining > 200KB threshold
- We proceed to call SQLite
- SQLite's parser needs 250KB+ for this complex query
- SQLite overflows the stack!

### 6. THE RECURSIVE FRAME

Frame **8445316** appears **218 times** in the stack trace!

This is inside Plex's code, likely a query builder or SQL generator that creates MASSIVE SQL queries with:
- Deep nesting
- Many JOINs
- Complex WHERE clauses
- Subqueries

These queries cause SQLite's recursive descent parser to go VERY DEEP.

### 7. SOLUTION

We need a MORE AGGRESSIVE stack protection:

```c
// CRITICAL: Reserve MORE stack for SQLite
// SQLite can need 300-400KB for complex queries!
if (stack_remaining < 400000) {  // Increase from 200KB to 400KB
    // Don't just skip processing - REFUSE the query!
    LOG_ERROR("INSUFFICIENT STACK: %ld bytes remaining, need 400KB", stack_remaining);
    if (ppStmt) *ppStmt = NULL;
    return SQLITE_NOMEM;  // Return out of memory error
}
```

OR better yet - detect we're being called RECURSIVELY and bail out:

```c
static __thread int prepare_depth = 0;

static int my_sqlite3_prepare_v2(...) {
    prepare_depth++;
    
    if (prepare_depth > 50) {  // Too much recursion!
        LOG_ERROR("RECURSION LIMIT: prepare_v2 called %d times", prepare_depth);
        prepare_depth--;
        return SQLITE_ERROR;
    }
    
    // ... rest of function ...
    
    int rc = sqlite3_prepare_v2(...);
    prepare_depth--;
    return rc;
}
```

## CONCLUSION

The stack protection EXISTS but is INSUFFICIENT because:
1. **Threshold too low**: 200KB is not enough, SQLite needs 300-400KB
2. **Checked too early**: We check, then call SQLite which does the damage
3. **No recursion detection**: We don't detect that prepare_v2 is being called recursively
4. **No query complexity analysis**: Huge queries need more stack

The crash happens in `sqlite3_str_vappendf` which is SQLite's string formatter, used deep inside the parser when building error messages or processing complex expressions.
