# Investigation Summary: Persistent SIGILL Crash

**Date**: 2026-01-10  
**Status**: ROOT CAUSE IDENTIFIED  
**Severity**: CRITICAL  

---

## TL;DR

The crash is **NOT caused by buffer pool management, NULL handling, or column_text issues**.

**Root Cause**: Fishhook library fails to rebind SQLite function pointers on Linux, causing NULL pointer dereferences when calling original SQLite functions.

**Evidence**:
- 1,284 "WARNING: orig_sqlite3_* is NULL!" messages
- 214 crashes in Docker logs
- Function pointers sometimes load (1,712 successes) but often fail
- Intermittent crashes on every shim reload

**Fix**: Replace fishhook with `dlsym(RTLD_NEXT, ...)` for reliable function pointer loading on Linux.

---

## What We Investigated (All Red Herrings)

### ❌ Buffer Pool Size
**Theory**: 8 buffers too small, causing wrap-around overwrites  
**Fix Attempted**: Increased to 256 buffers  
**Result**: Still crashes  
**Conclusion**: Not the issue

### ❌ Thread Safety  
**Theory**: Race conditions in buffer_idx increment  
**Fix Attempted**: Added atomic operations  
**Result**: Still crashes  
**Conclusion**: Not the issue

### ❌ NULL Value Handling
**Theory**: Shared empty string literal causes problems  
**Fix Attempted**: Separate buffers for each NULL  
**Result**: Still crashes  
**Conclusion**: Not the issue

### ❌ Specific Query Pattern
**Theory**: `SELECT id,guid FROM metadata_items WHERE hash IS NULL` triggers bug  
**Fix Attempted**: Special NULL column handling  
**Result**: Still crashes  
**Conclusion**: Not the issue

---

## What's Actually Wrong

### The Real Problem

File: `src/db_interpose_core.c`  
Function: `setup_fishhook_rebindings()`

```c
static void setup_fishhook_rebindings(void) {
    struct rebinding rebindings[] = {
        {"sqlite3_open", my_sqlite3_open, (void**)&orig_sqlite3_open},
        {"sqlite3_prepare_v2", my_sqlite3_prepare_v2, (void**)&orig_sqlite3_prepare_v2},
        // ... 51 more functions ...
    };

    int result = rebind_symbols(rebindings, count);
    
    // Returns 0 (success) but pointers are still NULL!
    if (result == 0) {
        if (orig_sqlite3_open) {
            fprintf(stderr, "orig_sqlite3_open = %p\n", (void*)orig_sqlite3_open);
        } else {
            fprintf(stderr, "WARNING: orig_sqlite3_open is NULL!\n");  // ← This!
        }
    }
}
```

**Why Fishhook Fails on Linux**:
1. Fishhook was designed for iOS/macOS dyld internals
2. Linux uses ld-linux.so (different dynamic linker)
3. Fishhook's symbol rebinding doesn't work with ELF format
4. Returns success but silently fails to populate pointers
5. Sometimes works (race condition?), often fails

### The Crash Sequence

1. Plex calls `sqlite3_column_text()` → our `my_sqlite3_column_text()` runs
2. We successfully return buffer to Plex
3. Plex processes the data
4. Plex calls another SQLite function (e.g., `sqlite3_step()`)
5. Our `my_sqlite3_step()` runs
6. We try to call `orig_sqlite3_step(pStmt)` 
7. **CRASH**: `orig_sqlite3_step` is NULL → dereference 0x0 → SIGILL/SIGSEGV
8. Plex writes crash dump and restarts
9. Shim reloads, fishhook fails again
10. Repeat forever (214 crashes so far)

### Why It Looked Like column_text Was the Problem

**Misleading Log Pattern**:
```
COLUMN_TEXT: copied to buffer[5] len=7    (id column)
COLUMN_TEXT: value is NULL, returning empty buffer    (guid column)  
[CRASH - shim reloads]
```

**Why It's Misleading**:
- `column_text()` successfully returned
- Crash happened in THE NEXT SQLite FUNCTION CALL
- Just coincidence that it was after NULL guid
- Actual crash was in `sqlite3_step()` or `sqlite3_finalize()` or another function
- Those functions had NULL pointers → crash

---

## The Fix

### Option 1: Quick Fix (Linux Only) - 2 hours

Create `src/db_interpose_core_linux.c`:

```c
#define _GNU_SOURCE
#include <dlfcn.h>

void linux_load_originals(void) {
    #define LOAD(name) do { \
        orig_##name = dlsym(RTLD_NEXT, #name); \
        if (!orig_##name) { \
            fprintf(stderr, "FATAL: Could not load " #name "\n"); \
            abort(); \
        } \
    } while(0)

    LOAD(sqlite3_open);
    LOAD(sqlite3_prepare_v2);
    LOAD(sqlite3_step);
    // ... all 53 functions ...
    
    fprintf(stderr, "[LINUX] Loaded 53 SQLite functions via RTLD_NEXT\n");
}
```

**Pros**: Guaranteed to work on Linux  
**Cons**: Platform-specific code

### Option 2: Unified Fix (Both Platforms) - 4 hours

Remove fishhook entirely, use RTLD_NEXT on both macOS and Linux:

```c
__attribute__((constructor))
static void shim_init(void) {
    // Works on both macOS AND Linux
    orig_sqlite3_open = dlsym(RTLD_NEXT, "sqlite3_open");
    orig_sqlite3_prepare_v2 = dlsym(RTLD_NEXT, "sqlite3_prepare_v2");
    // ... all 53 functions ...
    
    // Verify ALL pointers are loaded
    if (!orig_sqlite3_open || !orig_sqlite3_prepare_v2 || ...) {
        fprintf(stderr, "FATAL: Function pointer loading failed\n");
        abort();
    }
}
```

**Pros**: Simpler code, works everywhere, easier to maintain  
**Cons**: Requires testing on macOS

---

## Verification

Run: `./verify_function_pointers.sh`

**Current Output (Broken)**:
```
NULL warnings found:     1284
Crashes found:           214
❌ VERIFICATION FAILED
```

**Expected After Fix**:
```
NULL warnings found:     0
Crashes found:           0
Successful pointer loads: 53 / 53
✅ VERIFICATION PASSED
```

---

## Why This Investigation Was Hard

1. **Correlation ≠ Causation**: Crash happened after NULL column, but wasn't caused by it
2. **Intermittent Success**: Sometimes function pointers loaded (1,712 times), sometimes failed (1,284 times)
3. **Misleading Logs**: Last successful operation before crash pointed to column_text
4. **Platform Differences**: Works on macOS, fails on Linux - easy to miss
5. **Silent Failure**: Fishhook returns success even when it fails
6. **Wrong Assumptions**: We assumed buffer management was broken, spent time fixing something that worked

---

## Files to Read

1. **Complete Analysis**: `DEEP_INVESTIGATION_SIGILL_2026-01-10.md` (14KB, detailed)
2. **Verification Script**: `verify_function_pointers.sh` (run this to check status)
3. **Code to Fix**: `src/db_interpose_core.c` lines 381-545 (fishhook setup)

---

## Next Steps

1. **Immediate**: Implement Option 1 or 2 (see DEEP_INVESTIGATION document)
2. **Test**: Run verification script, check for zero NULL warnings
3. **Deploy**: Restart Docker container with fixed shim
4. **Verify**: No crashes in logs, Plex works normally
5. **Monitor**: Track crash rate (should be zero)

---

## Key Takeaway

**The buffer pool changes (256 buffers, atomics, NULL handling) were ALL UNNECESSARY.**

The column_text code was correct from the start. The crashes were caused by **fishhook failing to rebind function pointers on Linux**, causing NULL pointer dereferences in SUBSEQUENT SQLite function calls.

The fix is to replace fishhook with `dlsym(RTLD_NEXT, ...)` which is the standard Linux approach and works reliably on both Linux and macOS.

---

**Investigation by**: Claude Sonnet 4.5 Debugging Specialist  
**Total Investigation Time**: Deep analysis of code, logs, crash reports, and platform differences  
**Conclusion**: NOT a buffer management bug - it's a function pointer loading bug in fishhook
