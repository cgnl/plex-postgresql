/// Module: pg_query_cache
///
/// Thread-local query result cache for identical queries.
///
/// **Phase 3 migration**: This module now owns all cache state management,
/// replacing the C implementation in `src/pg_query_cache.c`. The C file
/// becomes a thin shim that forwards calls to these Rust FFI functions.
///
/// ## Memory safety fixes (vs. C original)
///
/// - **HIGH #4 fix**: The C `cache_destructor()` called `free(cache)` unconditionally
///   even when entries still had `ref_count > 0`, causing use-after-free.
///   The Rust cache never reclaims or overwrites an entry while it is still
///   referenced, and thread-exit cleanup force-frees only after the owning
///   TLS cache is being torn down.
///
/// ## Design
///
/// - Thread-local cache (no cross-thread sharing, no mutexes)
/// - Cache key: FNV-1a hash of (SQL + bound parameters)
/// - Cache TTL: 1 second (configurable via constants)
/// - LRU eviction when cache is full (64 entries)
/// - `cached_result_t` is `#[repr(C)]` so C code can read fields directly
///
/// ## FFI exports
///
///   - `rust_fnv1a_hash`           — FNV-1a hash of arbitrary bytes
///   - `rust_get_time_ms`          — current time in milliseconds
///   - `rust_query_cache_init`     — initialize cache (ensure TLS key created)
///   - `rust_query_cache_cleanup`  — no-op (cleanup happens via TLS Drop)
///   - `rust_query_cache_key`      — compute cache key from SQL + params
///   - `rust_query_cache_lookup`   — find cached result, return pointer + increment ref
///   - `rust_query_cache_store`    — copy PGresult data into cache
///   - `rust_query_cache_invalidate` — remove entry matching statement's key
///   - `rust_query_cache_release`  — decrement ref_count on a cached result
///   - `rust_query_cache_stats`    — get hit/miss counters
use std::cell::RefCell;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::LazyLock;

use crate::env_utils;
use crate::log_debug_lazy;
use crate::log_info_lazy;

// ─── Constants ────────────────────────────────────────────────────────────────

/// Number of cached queries per thread.
const QUERY_CACHE_SIZE: usize = 64;

/// Cache TTL in milliseconds (1 second).
const QUERY_CACHE_TTL_MS: u64 = 1000;

/// Don't cache results with more than this many rows.
const QUERY_CACHE_MAX_ROWS: i32 = 5;

/// Max total cached bytes per entry (1 MB).
const QUERY_CACHE_MAX_BYTES: usize = 1024 * 1024;

/// Max parameters per statement for cache key computation.
/// Acts as a safety cap — PgStmt param arrays are now Vec-based.
const MAX_PARAMS: usize = 128;

// ─── FNV-1a constants ────────────────────────────────────────────────────────

const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

// ─── Internal pure helpers ────────────────────────────────────────────────────

/// FNV-1a 64-bit hash of a byte slice.
pub(crate) fn fnv1a_hash_slice(data: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Current time as milliseconds since the Unix epoch, using `SystemTime`.
///
/// `SystemTime` is not strictly monotonic, but the difference from
/// `CLOCK_MONOTONIC` is negligible for cache TTL purposes and avoids
/// pulling in platform-specific APIs.
pub(crate) fn get_time_ms_impl() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ─── C-compatible struct definitions ──────────────────────────────────────────
//
// These match the C definitions in pg_types.h EXACTLY so that C code can
// read fields directly through the pointer. The memory layout is identical.

/// Cached row data — matches C `cached_row_t` in pg_types.h.
#[repr(C)]
pub struct CachedRow {
    /// Array of string values (NULL for NULL values). Length = num_cols.
    pub values: *mut *mut c_char,
    /// Length of each value. Length = num_cols.
    pub lengths: *mut i32,
    /// 1 if value is NULL. Length = num_cols.
    pub is_null: *mut i32,
}

/// Cached query result — matches C `cached_result_t` in pg_types.h.
///
/// This struct is `#[repr(C)]` so C callers in `db_interpose_column.c` and
/// `db_interpose_step.c` can read fields directly (num_rows, num_cols,
/// col_types, col_names, rows, etc).
#[repr(C)]
pub struct CachedResult {
    /// Hash of SQL + params (0 = empty/free slot).
    pub cache_key: u64,
    /// Timestamp when cached (ms since epoch).
    pub created_ms: u64,
    /// Reference count — don't free while > 0.
    pub ref_count: AtomicI32,
    /// Number of rows.
    pub num_rows: i32,
    /// Number of columns.
    pub num_cols: i32,
    /// PostgreSQL type OIDs per column. Length = num_cols.
    pub col_types: *mut u32, // Oid = unsigned int = u32
    /// Column names. Length = num_cols.
    pub col_names: *mut *mut c_char,
    /// Array of cached rows. Length = num_rows.
    pub rows: *mut CachedRow,
    /// Number of cache hits (for stats).
    pub hit_count: i32,
}

// ─── CachedResult memory management ──────────────────────────────────────────

impl CachedResult {
    /// Create a zeroed (empty) entry.
    fn empty() -> Self {
        Self {
            cache_key: 0,
            created_ms: 0,
            ref_count: AtomicI32::new(0),
            num_rows: 0,
            num_cols: 0,
            col_types: std::ptr::null_mut(),
            col_names: std::ptr::null_mut(),
            rows: std::ptr::null_mut(),
            hit_count: 0,
        }
    }

    /// Returns true if this entry is in use (has a cache key).
    fn is_active(&self) -> bool {
        self.cache_key != 0
    }

    fn is_referenced(&self) -> bool {
        self.ref_count.load(Ordering::Acquire) > 0
    }

    fn mark_expired(&mut self) {
        if self.is_active() {
            self.created_ms = 0;
        }
    }

    /// Free all heap-allocated data owned by this entry, but only if ref_count is 0.
    /// Returns true if the entry was actually freed.
    ///
    /// # Safety
    /// All pointers in this entry must have been allocated via `libc::malloc`/`libc::calloc`
    /// (which they are, since `store` uses libc allocation to match the C ABI).
    unsafe fn free_data(&mut self) -> bool {
        if !self.is_active() {
            return false;
        }

        // Don't free if still referenced by a pg_stmt_t somewhere
        if self.is_referenced() {
            return false;
        }

        // Free column types
        if !self.col_types.is_null() {
            libc::free(self.col_types as *mut libc::c_void);
            self.col_types = std::ptr::null_mut();
        }

        // Free column names
        if !self.col_names.is_null() {
            for i in 0..self.num_cols as usize {
                let name = *self.col_names.add(i);
                if !name.is_null() {
                    libc::free(name as *mut libc::c_void);
                }
            }
            libc::free(self.col_names as *mut libc::c_void);
            self.col_names = std::ptr::null_mut();
        }

        // Free rows
        if !self.rows.is_null() {
            for r in 0..self.num_rows as usize {
                let row = &mut *self.rows.add(r);
                if !row.values.is_null() {
                    for c in 0..self.num_cols as usize {
                        let val = *row.values.add(c);
                        if !val.is_null() {
                            libc::free(val as *mut libc::c_void);
                        }
                    }
                    libc::free(row.values as *mut libc::c_void);
                }
                if !row.lengths.is_null() {
                    libc::free(row.lengths as *mut libc::c_void);
                }
                if !row.is_null.is_null() {
                    libc::free(row.is_null as *mut libc::c_void);
                }
            }
            libc::free(self.rows as *mut libc::c_void);
            self.rows = std::ptr::null_mut();
        }

        self.cache_key = 0;
        self.num_rows = 0;
        self.num_cols = 0;
        true
    }

    /// Force-free all data regardless of ref_count.
    /// Used during thread exit — any outstanding refs are from stmts on this
    /// same thread that are being destroyed.
    ///
    /// # Safety
    /// Same requirements as `free_data`.
    unsafe fn force_free_data(&mut self) {
        // Temporarily set ref_count to 0 so free_data proceeds
        self.ref_count.store(0, Ordering::Release);
        self.free_data();
    }
}

// ─── QueryCache (thread-local) ───────────────────────────────────────────────

/// Thread-local query result cache.
///
/// Holds up to `QUERY_CACHE_SIZE` entries in a flat array with linear scan
/// and LRU eviction. This is efficient because the cache is small (64 entries).
struct QueryCache {
    entries: Vec<CachedResult>,
    count: i32,
    total_hits: u64,
    total_misses: u64,
}

impl QueryCache {
    fn new() -> Self {
        let mut entries = Vec::with_capacity(QUERY_CACHE_SIZE);
        for _ in 0..QUERY_CACHE_SIZE {
            entries.push(CachedResult::empty());
        }
        Self {
            entries,
            count: 0,
            total_hits: 0,
            total_misses: 0,
        }
    }
}

impl Drop for QueryCache {
    fn drop(&mut self) {
        // Log stats before cleanup (via Rust logging FFI)
        if self.total_hits > 0 || self.total_misses > 0 {
            let ratio = if self.total_hits + self.total_misses > 0 {
                100.0 * self.total_hits as f64 / (self.total_hits + self.total_misses) as f64
            } else {
                0.0
            };
            // Use the Rust logging system
            let msg = format!(
                "QUERY_CACHE thread exit: hits={} misses={} ratio={:.1}%",
                self.total_hits, self.total_misses, ratio
            );
            log_info(&msg);
        }

        // HIGH #4 FIX: The C code called free(cache) unconditionally, even when
        // entries had ref_count > 0. We use force_free_data which sets ref_count
        // to 0 first. This is safe because:
        //   1. The cache is thread-local — only this thread accesses it
        //   2. Thread exit means all pg_stmt_t on this thread are being destroyed
        //   3. Any cached_result pointer in a pg_stmt_t is about to become invalid
        //      anyway since the thread (and its TLS) is dying
        for entry in &mut self.entries {
            unsafe {
                entry.force_free_data();
            }
        }
    }
}

// ─── Thread-local storage ────────────────────────────────────────────────────

thread_local! {
    static THREAD_CACHE: RefCell<Option<Box<QueryCache>>> = const { RefCell::new(None) };
}

/// Query cache feature gate.
///
/// Disabled when `PLEX_PG_DISABLE_QUERY_CACHE` is set to anything except "0".
static QUERY_CACHE_DISABLED: LazyLock<bool> = LazyLock::new(|| {
    env_utils::env_string("PLEX_PG_DISABLE_QUERY_CACHE")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
});

#[inline]
fn query_cache_enabled() -> bool {
    !*QUERY_CACHE_DISABLED
}

/// Get or create the thread-local cache. Returns a raw pointer for use in
/// FFI functions. The pointer is valid for the lifetime of the thread.
fn with_cache<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut QueryCache) -> R,
{
    THREAD_CACHE
        .try_with(|cell| {
            let mut borrow = cell.borrow_mut();
            let cache = borrow.get_or_insert_with(|| Box::new(QueryCache::new()));
            Some(f(cache))
        })
        .ok()
        .flatten()
}

// ─── Logging helper ──────────────────────────────────────────────────────────

/// Log at INFO level via the Rust logging backend.
fn log_info(msg: &str) {
    // Use the same FFI function that pg_mem_telemetry uses
    extern "C" {
        fn rust_logging_write(level: i32, message: *const c_char);
    }
    if let Ok(cmsg) = std::ffi::CString::new(msg) {
        unsafe {
            rust_logging_write(1, cmsg.as_ptr()); // 1 = LEVEL_INFO
        }
    }
}

/// Log at DEBUG level via the Rust logging backend.
#[allow(dead_code)]
fn log_debug(msg: &str) {
    extern "C" {
        fn rust_logging_write(level: i32, message: *const c_char);
    }
    if let Ok(cmsg) = std::ffi::CString::new(msg) {
        unsafe {
            rust_logging_write(2, cmsg.as_ptr()); // 2 = LEVEL_DEBUG
        }
    }
}

// ─── Opaque pg_stmt_t field access ───────────────────────────────────────────
//
// We access pg_stmt_t fields via raw pointer offsets. The struct layout is
// defined in pg_types.h. Rather than reproducing the entire 30+ field struct
// in Rust, we define accessor functions that read specific fields by offset.
//
// This is intentionally kept minimal — we only need:
//   - pg_sql (char*) for cache key computation
//   - param_values (char*[MAX_PARAMS]) for cache key computation
//   - param_count (int)
//   - cached_result (cached_result_t*) for setting the pointer on cache hit

/// Opaque handle to C pg_stmt_t. Never constructed in Rust.
///
/// We don't reproduce the full pg_stmt_t layout here — the C shim
/// extracts relevant fields and passes them as separate FFI parameters.
#[repr(C)]
pub struct PgStmt {
    _opaque: [u8; 0],
}

// ─── Cache key computation ───────────────────────────────────────────────────

/// Compute cache key from SQL string and parameter values.
///
/// This is the pure Rust implementation. The C shim extracts the relevant
/// fields from `pg_stmt_t` and passes them to this function.
fn compute_cache_key(pg_sql: &[u8], param_values: &[Option<&[u8]>]) -> u64 {
    if pg_sql.is_empty() {
        return 0;
    }

    // Start with SQL hash
    let mut hash = fnv1a_hash_slice(pg_sql);

    // Mix in parameter values
    for param in param_values {
        match param {
            Some(val) => {
                let param_hash = fnv1a_hash_slice(val);
                hash ^= param_hash;
                hash = hash.wrapping_mul(FNV_PRIME);
            }
            None => {
                // NULL parameter — use sentinel value
                hash ^= 0xDEADBEEF_u64;
                hash = hash.wrapping_mul(FNV_PRIME);
            }
        }
    }

    hash
}

// ─── Public C FFI functions ───────────────────────────────────────────────────

/// FNV-1a 64-bit hash of arbitrary bytes.
///
/// # Safety
/// `data` must point to at least `len` readable bytes, or be NULL when `len` is 0.
pub fn rust_fnv1a_hash(data: *const u8, len: usize) -> u64 {
    if len == 0 {
        return FNV_OFFSET_BASIS;
    }
    let slice = unsafe { std::slice::from_raw_parts(data, len) };
    fnv1a_hash_slice(slice)
}

/// Current time in milliseconds (via `SystemTime`).
pub fn rust_get_time_ms() -> u64 {
    get_time_ms_impl()
}

/// Initialize the query cache. Called once at startup.
///
/// The actual TLS allocation happens lazily on first use, so this just
/// logs that the cache is ready.
pub fn rust_query_cache_init() {
    if !query_cache_enabled() {
        log_info("QUERY_CACHE disabled via PLEX_PG_DISABLE_QUERY_CACHE");
        return;
    }
    log_info_lazy!(
        "Query result cache initialized (size={}, ttl={}ms) [Rust]",
        QUERY_CACHE_SIZE,
        QUERY_CACHE_TTL_MS
    );
}

/// Cleanup the query cache. No-op — cleanup happens via TLS Drop.
pub fn rust_query_cache_cleanup() {
    if query_cache_enabled() {
        // Thread-local cleanup happens automatically via Drop
    }
}

/// Compute cache key from SQL string and parameters.
///
/// The C shim extracts these from the pg_stmt_t and passes them here.
///
/// # Safety
/// - `pg_sql` must be a valid null-terminated C string (or NULL).
/// - `param_values` must point to `param_count` pointers, each either NULL
///   or pointing to a null-terminated C string.
/// - `param_count` must be <= MAX_PARAMS.
pub fn rust_query_cache_key(
    pg_sql: *const c_char,
    param_values: *const *const c_char,
    param_count: i32,
) -> u64 {
    if !query_cache_enabled() {
        return 0;
    }
    if pg_sql.is_null() {
        return 0;
    }

    let sql_bytes = unsafe { CStr::from_ptr(pg_sql).to_bytes() };
    if sql_bytes.is_empty() {
        return 0;
    }

    let count = (param_count as usize).min(MAX_PARAMS);
    let mut params: Vec<Option<&[u8]>> = Vec::with_capacity(count);

    if !param_values.is_null() {
        for i in 0..count {
            let val_ptr = unsafe { *param_values.add(i) };
            if val_ptr.is_null() {
                params.push(None);
            } else {
                let val_bytes = unsafe { CStr::from_ptr(val_ptr).to_bytes() };
                params.push(Some(val_bytes));
            }
        }
    }

    compute_cache_key(sql_bytes, &params)
}

/// Look up a cached result by cache key.
///
/// Returns a pointer to the `cached_result_t` if found and not expired,
/// with `ref_count` incremented. The caller MUST call `rust_query_cache_release`
/// when done.
///
/// Returns NULL if not found or expired.
pub fn rust_query_cache_lookup(cache_key: u64) -> *mut CachedResult {
    if !query_cache_enabled() {
        return std::ptr::null_mut();
    }
    if cache_key == 0 {
        return std::ptr::null_mut();
    }

    let now = get_time_ms_impl();

    let result = with_cache(|cache| {
        for entry in cache.entries.iter_mut() {
            if entry.cache_key == cache_key {
                // Check TTL
                if now.wrapping_sub(entry.created_ms) < QUERY_CACHE_TTL_MS {
                    // Cache hit — increment ref_count
                    entry.ref_count.fetch_add(1, Ordering::AcqRel);
                    entry.hit_count += 1;
                    cache.total_hits += 1;
                    return entry as *mut CachedResult;
                } else {
                    // Expired — try to free
                    unsafe {
                        entry.free_data();
                    }
                    cache.total_misses += 1;
                    return std::ptr::null_mut();
                }
            }
        }
        cache.total_misses += 1;
        std::ptr::null_mut()
    });

    result.unwrap_or(std::ptr::null_mut())
}

/// Store a query result in the cache.
///
/// Copies data from the PGresult into a cache entry. The C shim calls
/// libpq functions to extract row/column data and passes it via this
/// structured interface.
///
/// # Parameters
/// - `cache_key`: precomputed cache key (from `rust_query_cache_key`)
/// - `num_rows`: number of rows in the result
/// - `num_cols`: number of columns in the result
/// - `col_types`: array of `num_cols` OIDs (Oid = u32)
/// - `col_names`: array of `num_cols` null-terminated C strings
/// - `values`: flattened 2D array of `num_rows * num_cols` null-terminated C strings
///             (NULL for SQL NULL values)
/// - `lengths`: flattened 2D array of `num_rows * num_cols` value lengths
/// - `is_null`: flattened 2D array of `num_rows * num_cols` null flags (1 = NULL)
/// - `pg_sql`: SQL string for logging (may be NULL)
///
/// This function allocates memory via `libc::malloc` for all cached data,
/// matching the C convention so that `free_data()` can use `libc::free`.
pub fn rust_query_cache_store(
    cache_key: u64,
    num_rows: i32,
    num_cols: i32,
    col_types: *const u32,
    col_names: *const *const c_char,
    values: *const *const c_char,
    lengths: *const i32,
    is_null: *const i32,
    pg_sql: *const c_char,
) {
    if !query_cache_enabled() {
        return;
    }
    if cache_key == 0 || num_cols <= 0 {
        return;
    }

    // Don't cache huge or empty results
    if num_rows > QUERY_CACHE_MAX_ROWS || num_rows <= 0 {
        return;
    }

    let nr = num_rows as usize;
    let nc = num_cols as usize;

    with_cache(|cache| {
        // Find slot: exact match we can safely replace, free slot, expired slot
        // with no live refs, or oldest reclaimable slot. Never overwrite a
        // still-referenced entry: that leaks its heap allocations and can
        // mutate data still visible through an outstanding cached_result*.
        let now = get_time_ms_impl();
        let mut exact_match_slot: Option<usize> = None;
        let mut empty_slot: Option<usize> = None;
        let mut expired_slot: Option<usize> = None;
        let mut oldest_reclaimable_slot: Option<usize> = None;
        let mut oldest_reclaimable_time = u64::MAX;

        for (i, entry) in cache.entries.iter().enumerate() {
            if entry.cache_key == cache_key {
                if entry.is_referenced() {
                    log_debug_lazy!(
                        "QUERY_CACHE STORE: skip overwrite of live entry key={:x} refs={}",
                        cache_key,
                        entry.ref_count.load(Ordering::Acquire)
                    );
                    return;
                }
                exact_match_slot = Some(i);
                break;
            }
            if !entry.is_active() {
                empty_slot.get_or_insert(i);
                continue;
            }

            let expired = now.wrapping_sub(entry.created_ms) >= QUERY_CACHE_TTL_MS;
            if expired && !entry.is_referenced() {
                expired_slot.get_or_insert(i);
                continue;
            }

            if !entry.is_referenced() && entry.created_ms < oldest_reclaimable_time {
                oldest_reclaimable_time = entry.created_ms;
                oldest_reclaimable_slot = Some(i);
            }
        }

        let slot = exact_match_slot
            .or(empty_slot)
            .or(expired_slot)
            .or(oldest_reclaimable_slot);
        let Some(slot) = slot else {
            log_debug_lazy!(
                "QUERY_CACHE STORE: no reclaimable slot for key={:x}, skipping",
                cache_key
            );
            return;
        };

        // Free existing entry in this slot. This should always succeed for an
        // active slot because selection filtered out referenced entries.
        let slot_was_active = cache.entries[slot].is_active();
        if slot_was_active {
            let reclaimed = unsafe { cache.entries[slot].free_data() };
            if !reclaimed {
                log_debug_lazy!(
                    "QUERY_CACHE STORE: slot {} became busy before reclaim, skipping key={:x}",
                    slot,
                    cache_key
                );
                return;
            }
            cache.count = cache.count.saturating_sub(1);
        }

        // Allocate and populate the new entry
        // All allocations use libc::malloc/calloc to match C ABI
        let entry = &mut cache.entries[slot];

        unsafe {
            let mut total_size: usize = 0;

            // Allocate column types
            let types_ptr = libc::malloc(nc * std::mem::size_of::<u32>()) as *mut u32;
            if types_ptr.is_null() {
                return;
            }
            entry.col_types = types_ptr;

            // Allocate column names array
            let names_ptr =
                libc::calloc(nc, std::mem::size_of::<*mut c_char>()) as *mut *mut c_char;
            if names_ptr.is_null() {
                cleanup_partial(entry);
                return;
            }
            entry.col_names = names_ptr;

            // Copy column types and names
            for c in 0..nc {
                *types_ptr.add(c) = *col_types.add(c);

                let name = *col_names.add(c);
                if !name.is_null() {
                    let name_str = CStr::from_ptr(name);
                    let name_len = name_str.to_bytes().len();
                    let name_copy = libc::malloc(name_len + 1) as *mut c_char;
                    if !name_copy.is_null() {
                        std::ptr::copy_nonoverlapping(
                            name.cast::<u8>(),
                            name_copy.cast::<u8>(),
                            name_len + 1,
                        );
                        *names_ptr.add(c) = name_copy;
                        total_size += name_len + 1;
                    }
                }
            }

            // Allocate rows
            let rows_ptr = libc::calloc(nr, std::mem::size_of::<CachedRow>()) as *mut CachedRow;
            if rows_ptr.is_null() {
                cleanup_partial(entry);
                return;
            }
            entry.rows = rows_ptr;

            // Copy row data
            for r in 0..nr {
                let row = &mut *rows_ptr.add(r);

                row.values =
                    libc::calloc(nc, std::mem::size_of::<*mut c_char>()) as *mut *mut c_char;
                row.lengths = libc::calloc(nc, std::mem::size_of::<i32>()) as *mut i32;
                row.is_null = libc::calloc(nc, std::mem::size_of::<i32>()) as *mut i32;

                if row.values.is_null() || row.lengths.is_null() || row.is_null.is_null() {
                    // Set metadata so cleanup knows how far we got
                    entry.num_rows = r as i32 + 1;
                    entry.num_cols = num_cols;
                    entry.cache_key = 1; // Mark as active so free_data works
                    entry.ref_count.store(0, Ordering::Release);
                    cleanup_partial(entry);
                    return;
                }

                let base = r * nc;
                for c_idx in 0..nc {
                    let flat_idx = base + c_idx;
                    let null_flag = *is_null.add(flat_idx);
                    *row.is_null.add(c_idx) = null_flag;

                    if null_flag != 0 {
                        *row.values.add(c_idx) = std::ptr::null_mut();
                        *row.lengths.add(c_idx) = 0;
                    } else {
                        let len = *lengths.add(flat_idx);
                        *row.lengths.add(c_idx) = len;

                        total_size += len as usize + 1;
                        if total_size > QUERY_CACHE_MAX_BYTES {
                            // Too large — clean up
                            entry.num_rows = r as i32 + 1;
                            entry.num_cols = num_cols;
                            entry.cache_key = 1;
                            entry.ref_count.store(0, Ordering::Release);
                            cleanup_partial(entry);
                            return;
                        }

                        let val_src = *values.add(flat_idx);
                        let val_copy = libc::malloc(len as usize + 1) as *mut c_char;
                        if val_copy.is_null() {
                            entry.num_rows = r as i32 + 1;
                            entry.num_cols = num_cols;
                            entry.cache_key = 1;
                            entry.ref_count.store(0, Ordering::Release);
                            cleanup_partial(entry);
                            return;
                        }

                        if !val_src.is_null() {
                            std::ptr::copy_nonoverlapping(
                                val_src.cast::<u8>(),
                                val_copy.cast::<u8>(),
                                len as usize,
                            );
                        }
                        // Null-terminate
                        *(val_copy.cast::<u8>()).add(len as usize) = 0;
                        *row.values.add(c_idx) = val_copy;
                    }
                }
            }

            // Success — fill in metadata
            entry.cache_key = cache_key;
            entry.created_ms = get_time_ms_impl();
            entry.ref_count.store(0, Ordering::Release);
            entry.num_rows = num_rows;
            entry.num_cols = num_cols;
            entry.hit_count = 0;
            cache.count += 1;

            // Debug log
            let sql_preview = if !pg_sql.is_null() {
                let s = CStr::from_ptr(pg_sql).to_bytes();
                let truncated = &s[..s.len().min(60)];
                String::from_utf8_lossy(truncated).into_owned()
            } else {
                String::from("?")
            };
            log_debug_lazy!(
                "QUERY_CACHE STORE: key={:x} rows={} cols={} size={} sql={}",
                cache_key,
                num_rows,
                num_cols,
                total_size,
                sql_preview
            );
        }
    });
}

/// Clean up a partially-allocated cache entry after an allocation failure.
///
/// # Safety
/// Entry must have valid `num_rows` and `num_cols` set to indicate how much
/// was allocated.
unsafe fn cleanup_partial(entry: &mut CachedResult) {
    entry.ref_count.store(0, Ordering::Release);
    entry.free_data();
}

/// Invalidate the cache entry matching the given key.
///
/// Frees the entry's data if ref_count is 0.
pub fn rust_query_cache_invalidate(cache_key: u64) {
    if !query_cache_enabled() {
        return;
    }
    if cache_key == 0 {
        return;
    }

    with_cache(|cache| {
        for entry in cache.entries.iter_mut() {
            if entry.cache_key == cache_key {
                let freed = unsafe { entry.free_data() };
                if freed {
                    cache.count = cache.count.saturating_sub(1);
                } else {
                    // Keep live readers safe, but prevent future lookups from
                    // hitting this entry. Once refs drop to 0, a later lookup
                    // or store can reclaim it.
                    entry.mark_expired();
                }
                return;
            }
        }
    });
}

/// Decrement the ref_count on a cached result entry.
///
/// MUST be called when `pg_stmt->cached_result` is cleared.
///
/// # Safety
/// `entry` must point to a valid `CachedResult` that was returned by
/// `rust_query_cache_lookup`.
pub fn rust_query_cache_release(entry: *mut CachedResult) {
    if !query_cache_enabled() {
        return;
    }
    if entry.is_null() {
        return;
    }
    unsafe {
        let e = &*entry;
        let old = e.ref_count.fetch_sub(1, Ordering::AcqRel);
        if old <= 1 {
            log_debug_lazy!(
                "CACHE_RELEASE: entry {:p} now has 0 refs, eligible for eviction",
                entry
            );
        }
    }
}

/// Get cache stats for the current thread.
///
/// # Safety
/// `hits` and `misses` must be valid pointers (or NULL).
pub fn rust_query_cache_stats(hits: *mut u64, misses: *mut u64) {
    if !query_cache_enabled() {
        if !hits.is_null() {
            unsafe {
                *hits = 0;
            }
        }
        if !misses.is_null() {
            unsafe {
                *misses = 0;
            }
        }
        return;
    }

    let result = with_cache(|cache| {
        if !hits.is_null() {
            unsafe {
                *hits = cache.total_hits;
            }
        }
        if !misses.is_null() {
            unsafe {
                *misses = cache.total_misses;
            }
        }
    });
    if result.is_none() {
        // No cache yet — return zeros
        if !hits.is_null() {
            unsafe {
                *hits = 0;
            }
        }
        if !misses.is_null() {
            unsafe {
                *misses = 0;
            }
        }
    }
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn store_single_text_value(cache_key: u64, value: &str) {
        let col_types: [u32; 1] = [25];
        let col_name = std::ffi::CString::new("val").unwrap();
        let col_names: [*const c_char; 1] = [col_name.as_ptr()];
        let val = std::ffi::CString::new(value).unwrap();
        let values: [*const c_char; 1] = [val.as_ptr()];
        let lengths: [i32; 1] = [value.len() as i32];
        let is_null: [i32; 1] = [0];

        rust_query_cache_store(
            cache_key,
            1,
            1,
            col_types.as_ptr(),
            col_names.as_ptr(),
            values.as_ptr(),
            lengths.as_ptr(),
            is_null.as_ptr(),
            std::ptr::null(),
        );
    }

    unsafe fn cached_text_at(entry: *mut CachedResult, row_idx: usize, col_idx: usize) -> Vec<u8> {
        let row = &*(*entry).rows.add(row_idx);
        CStr::from_ptr(*row.values.add(col_idx)).to_bytes().to_vec()
    }
    use std::time::{SystemTime, UNIX_EPOCH};

    // ── fnv1a_hash — correctness ─────────────────────────────────────────────

    #[test]
    fn fnv1a_empty_returns_offset_basis() {
        assert_eq!(fnv1a_hash_slice(b""), FNV_OFFSET_BASIS);
    }

    #[test]
    fn fnv1a_single_null_byte() {
        let expected = FNV_OFFSET_BASIS.wrapping_mul(FNV_PRIME);
        assert_eq!(fnv1a_hash_slice(&[0x00]), expected);
    }

    #[test]
    fn fnv1a_single_byte() {
        let byte = b'A';
        let expected = (FNV_OFFSET_BASIS ^ byte as u64).wrapping_mul(FNV_PRIME);
        assert_eq!(fnv1a_hash_slice(&[byte]), expected);
    }

    #[test]
    fn fnv1a_consistent() {
        let a = fnv1a_hash_slice(b"hello world");
        let b = fnv1a_hash_slice(b"hello world");
        assert_eq!(a, b);
    }

    #[test]
    fn fnv1a_different_inputs_differ() {
        let h1 = fnv1a_hash_slice(b"SELECT * FROM foo");
        let h2 = fnv1a_hash_slice(b"SELECT * FROM bar");
        assert_ne!(h1, h2);
    }

    #[test]
    fn fnv1a_order_sensitive() {
        assert_ne!(fnv1a_hash_slice(b"ab"), fnv1a_hash_slice(b"ba"));
    }

    #[test]
    fn fnv1a_hello_known_value() {
        assert_eq!(fnv1a_hash_slice(b"hello"), 0xa430d84680aabd0b);
    }

    #[test]
    fn fnv1a_foobar_known_value() {
        assert_eq!(fnv1a_hash_slice(b"foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn fnv1a_multi_zero_differs_from_single_zero() {
        let single = fnv1a_hash_slice(&[0x00]);
        let multi = fnv1a_hash_slice(&[0x00, 0x00]);
        assert_ne!(single, multi);
    }

    #[test]
    fn fnv1a_prefix_differs_from_full() {
        let full = fnv1a_hash_slice(b"SELECT 1");
        let prefix = fnv1a_hash_slice(b"SELECT");
        assert_ne!(full, prefix);
    }

    // ── rust_fnv1a_hash FFI ──────────────────────────────────────────────────

    #[test]
    fn ffi_fnv1a_empty_len_zero() {
        assert_eq!(rust_fnv1a_hash(std::ptr::null(), 0), FNV_OFFSET_BASIS);
    }

    #[test]
    fn ffi_fnv1a_matches_pure() {
        let data = b"SELECT * FROM metadata";
        let expected = fnv1a_hash_slice(data);
        assert_eq!(rust_fnv1a_hash(data.as_ptr(), data.len()), expected);
    }

    // ── get_time_ms ──────────────────────────────────────────────────────────

    #[test]
    fn get_time_ms_reasonable_value() {
        const Y2K_MS: u64 = 946_684_800_000;
        assert!(
            get_time_ms_impl() > Y2K_MS,
            "clock appears to be before year 2000"
        );
    }

    #[test]
    fn get_time_ms_non_decreasing() {
        let t1 = get_time_ms_impl();
        let t2 = get_time_ms_impl();
        assert!(t2 >= t1, "time went backwards: t1={t1} t2={t2}");
    }

    #[test]
    fn get_time_ms_advances_after_sleep() {
        let before = get_time_ms_impl();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let after = get_time_ms_impl();
        assert!(
            after > before,
            "time did not advance after sleep: before={before} after={after}"
        );
    }

    #[test]
    fn ffi_get_time_ms_matches_system_time() {
        let ffi_val = rust_get_time_ms();
        let direct = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let diff = direct.abs_diff(ffi_val);
        assert!(
            diff < 100,
            "FFI time {ffi_val} deviates too far from direct read {direct}"
        );
    }

    // ── compute_cache_key ────────────────────────────────────────────────────

    #[test]
    fn cache_key_empty_sql_returns_zero() {
        assert_eq!(compute_cache_key(b"", &[]), 0);
    }

    #[test]
    fn cache_key_no_params() {
        let key = compute_cache_key(b"SELECT 1", &[]);
        assert_ne!(key, 0);
    }

    #[test]
    fn cache_key_with_params() {
        let key1 = compute_cache_key(b"SELECT $1", &[Some(b"hello")]);
        let key2 = compute_cache_key(b"SELECT $1", &[Some(b"world")]);
        assert_ne!(key1, key2);
    }

    #[test]
    fn cache_key_null_param_differs() {
        let key1 = compute_cache_key(b"SELECT $1", &[Some(b"hello")]);
        let key2 = compute_cache_key(b"SELECT $1", &[None]);
        assert_ne!(key1, key2);
    }

    #[test]
    fn cache_key_same_sql_same_params_equal() {
        let key1 = compute_cache_key(b"SELECT $1", &[Some(b"42")]);
        let key2 = compute_cache_key(b"SELECT $1", &[Some(b"42")]);
        assert_eq!(key1, key2);
    }

    #[test]
    fn cache_key_different_sql_differ() {
        let key1 = compute_cache_key(b"SELECT 1", &[]);
        let key2 = compute_cache_key(b"SELECT 2", &[]);
        assert_ne!(key1, key2);
    }

    #[test]
    fn cache_key_param_order_matters() {
        let key1 = compute_cache_key(b"SELECT $1, $2", &[Some(b"a"), Some(b"b")]);
        let key2 = compute_cache_key(b"SELECT $1, $2", &[Some(b"b"), Some(b"a")]);
        assert_ne!(key1, key2);
    }

    // ── rust_query_cache_key FFI ─────────────────────────────────────────────

    #[test]
    fn ffi_cache_key_null_sql_returns_zero() {
        assert_eq!(
            rust_query_cache_key(std::ptr::null(), std::ptr::null(), 0),
            0
        );
    }

    #[test]
    fn ffi_cache_key_empty_sql_returns_zero() {
        let sql = b"\0";
        assert_eq!(
            rust_query_cache_key(sql.as_ptr() as *const c_char, std::ptr::null(), 0),
            0
        );
    }

    #[test]
    fn ffi_cache_key_matches_pure() {
        let sql = std::ffi::CString::new("SELECT 1").unwrap();
        let ffi_key = rust_query_cache_key(sql.as_ptr(), std::ptr::null(), 0);
        let pure_key = compute_cache_key(b"SELECT 1", &[]);
        assert_eq!(ffi_key, pure_key);
    }

    #[test]
    fn ffi_cache_key_with_params() {
        let sql = std::ffi::CString::new("SELECT $1").unwrap();
        let param = std::ffi::CString::new("hello").unwrap();
        let params: [*const c_char; 1] = [param.as_ptr()];

        let ffi_key = rust_query_cache_key(sql.as_ptr(), params.as_ptr(), 1);
        let pure_key = compute_cache_key(b"SELECT $1", &[Some(b"hello")]);
        assert_eq!(ffi_key, pure_key);
    }

    // ── lookup/store/release lifecycle ───────────────────────────────────────

    #[test]
    fn lookup_nonexistent_returns_null() {
        let ptr = rust_query_cache_lookup(12345);
        assert!(ptr.is_null());
    }

    #[test]
    fn store_and_lookup_roundtrip() {
        // Prepare test data
        let cache_key: u64 = 0xDEAD_BEEF_CAFE_1234;
        let col_types: [u32; 2] = [23, 25]; // INT4, TEXT
        let col_name_0 = std::ffi::CString::new("id").unwrap();
        let col_name_1 = std::ffi::CString::new("name").unwrap();
        let col_names: [*const c_char; 2] = [col_name_0.as_ptr(), col_name_1.as_ptr()];

        let val_0 = std::ffi::CString::new("42").unwrap();
        let val_1 = std::ffi::CString::new("Alice").unwrap();
        let values: [*const c_char; 2] = [val_0.as_ptr(), val_1.as_ptr()];
        let lengths: [i32; 2] = [2, 5];
        let is_null: [i32; 2] = [0, 0];

        let sql = std::ffi::CString::new("SELECT id, name FROM users").unwrap();

        // Store
        rust_query_cache_store(
            cache_key,
            1, // num_rows
            2, // num_cols
            col_types.as_ptr(),
            col_names.as_ptr(),
            values.as_ptr(),
            lengths.as_ptr(),
            is_null.as_ptr(),
            sql.as_ptr(),
        );

        // Lookup
        let result = rust_query_cache_lookup(cache_key);
        assert!(!result.is_null(), "lookup should find stored entry");

        unsafe {
            let entry = &*result;
            assert_eq!(entry.cache_key, cache_key);
            assert_eq!(entry.num_rows, 1);
            assert_eq!(entry.num_cols, 2);
            assert_eq!(entry.ref_count.load(Ordering::Relaxed), 1); // lookup incremented

            // Check column types
            assert_eq!(*entry.col_types.add(0), 23);
            assert_eq!(*entry.col_types.add(1), 25);

            // Check column names
            let name0 = CStr::from_ptr(*entry.col_names.add(0));
            assert_eq!(name0.to_bytes(), b"id");
            let name1 = CStr::from_ptr(*entry.col_names.add(1));
            assert_eq!(name1.to_bytes(), b"name");

            // Check row data
            let row = &*entry.rows.add(0);
            assert_eq!(*row.is_null.add(0), 0);
            assert_eq!(*row.is_null.add(1), 0);
            let v0 = CStr::from_ptr(*row.values.add(0));
            assert_eq!(v0.to_bytes(), b"42");
            let v1 = CStr::from_ptr(*row.values.add(1));
            assert_eq!(v1.to_bytes(), b"Alice");

            // Release
            rust_query_cache_release(result);
            assert_eq!(entry.ref_count.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn store_null_values_roundtrip() {
        let cache_key: u64 = 0x1111_2222_3333_4444;
        let col_types: [u32; 1] = [25]; // TEXT
        let col_name = std::ffi::CString::new("val").unwrap();
        let col_names: [*const c_char; 1] = [col_name.as_ptr()];

        // One row with a NULL value
        let values: [*const c_char; 1] = [std::ptr::null()];
        let lengths: [i32; 1] = [0];
        let is_null: [i32; 1] = [1];

        rust_query_cache_store(
            cache_key,
            1,
            1,
            col_types.as_ptr(),
            col_names.as_ptr(),
            values.as_ptr(),
            lengths.as_ptr(),
            is_null.as_ptr(),
            std::ptr::null(),
        );

        let result = rust_query_cache_lookup(cache_key);
        assert!(!result.is_null());

        unsafe {
            let entry = &*result;
            let row = &*entry.rows.add(0);
            assert_eq!(*row.is_null.add(0), 1);
            assert!((*row.values.add(0)).is_null());

            rust_query_cache_release(result);
        }
    }

    #[test]
    fn invalidate_removes_entry() {
        let cache_key: u64 = 0xAAAA_BBBB_CCCC_DDDD;
        let col_types: [u32; 1] = [23];
        let col_name = std::ffi::CString::new("x").unwrap();
        let col_names: [*const c_char; 1] = [col_name.as_ptr()];
        let val = std::ffi::CString::new("1").unwrap();
        let values: [*const c_char; 1] = [val.as_ptr()];
        let lengths: [i32; 1] = [1];
        let is_null: [i32; 1] = [0];

        rust_query_cache_store(
            cache_key,
            1,
            1,
            col_types.as_ptr(),
            col_names.as_ptr(),
            values.as_ptr(),
            lengths.as_ptr(),
            is_null.as_ptr(),
            std::ptr::null(),
        );

        // Verify it's there
        let result = rust_query_cache_lookup(cache_key);
        assert!(!result.is_null());
        rust_query_cache_release(result);

        // Invalidate
        rust_query_cache_invalidate(cache_key);

        // Should be gone
        let result2 = rust_query_cache_lookup(cache_key);
        assert!(result2.is_null());
    }

    #[test]
    fn store_does_not_overwrite_live_exact_match_entry() {
        let cache_key: u64 = 0xABCD_EF01_2345_6789;
        store_single_text_value(cache_key, "Alice");

        let live = rust_query_cache_lookup(cache_key);
        assert!(!live.is_null());

        store_single_text_value(cache_key, "Bob");

        unsafe {
            assert_eq!(cached_text_at(live, 0, 0), b"Alice");
        }

        let lookup_again = rust_query_cache_lookup(cache_key);
        assert!(!lookup_again.is_null());
        unsafe {
            assert_eq!(cached_text_at(lookup_again, 0, 0), b"Alice");
        }

        rust_query_cache_release(lookup_again);
        rust_query_cache_release(live);
    }

    #[test]
    fn invalidate_live_entry_blocks_future_hits_without_clobbering_live_reader() {
        let cache_key: u64 = 0x1357_2468_1357_2468;
        store_single_text_value(cache_key, "Alice");

        let live = rust_query_cache_lookup(cache_key);
        assert!(!live.is_null());

        rust_query_cache_invalidate(cache_key);

        let miss = rust_query_cache_lookup(cache_key);
        assert!(
            miss.is_null(),
            "invalidated live entry should not be returned"
        );

        unsafe {
            assert_eq!(cached_text_at(live, 0, 0), b"Alice");
        }

        rust_query_cache_release(live);

        let miss_after_release = rust_query_cache_lookup(cache_key);
        assert!(
            miss_after_release.is_null(),
            "invalidated entry should stay gone after last release"
        );

        with_cache(|cache| {
            assert!(
                cache
                    .entries
                    .iter()
                    .all(|entry| entry.cache_key != cache_key),
                "invalidated entry should be reclaimed after final release"
            );
        });
    }

    #[test]
    fn lru_store_skips_live_oldest_entry() {
        let mut keys = Vec::with_capacity(QUERY_CACHE_SIZE);
        for i in 0..QUERY_CACHE_SIZE {
            let key = 0x9100_0000_0000_0000_u64 + i as u64;
            keys.push(key);
            store_single_text_value(key, &format!("v{i}"));
        }

        let live = rust_query_cache_lookup(keys[0]);
        assert!(!live.is_null());

        let new_key = 0x9200_0000_0000_0001_u64;
        store_single_text_value(new_key, "fresh");

        unsafe {
            assert_eq!(cached_text_at(live, 0, 0), b"v0");
        }

        let oldest_again = rust_query_cache_lookup(keys[0]);
        assert!(
            !oldest_again.is_null(),
            "live oldest entry should remain cached"
        );
        unsafe {
            assert_eq!(cached_text_at(oldest_again, 0, 0), b"v0");
        }

        let fresh = rust_query_cache_lookup(new_key);
        assert!(!fresh.is_null(), "new entry should use a reclaimable slot");
        unsafe {
            assert_eq!(cached_text_at(fresh, 0, 0), b"fresh");
        }

        rust_query_cache_release(fresh);
        rust_query_cache_release(oldest_again);
        rust_query_cache_release(live);
    }

    #[test]
    fn stats_track_hits_and_misses() {
        let mut hits: u64 = 0;
        let mut misses: u64 = 0;

        // Store an entry
        let cache_key: u64 = 0xFEED_FACE_DEAD_BEEF;
        let col_types: [u32; 1] = [23];
        let col_name = std::ffi::CString::new("n").unwrap();
        let col_names: [*const c_char; 1] = [col_name.as_ptr()];
        let val = std::ffi::CString::new("7").unwrap();
        let values: [*const c_char; 1] = [val.as_ptr()];
        let lengths: [i32; 1] = [1];
        let is_null: [i32; 1] = [0];

        rust_query_cache_store(
            cache_key,
            1,
            1,
            col_types.as_ptr(),
            col_names.as_ptr(),
            values.as_ptr(),
            lengths.as_ptr(),
            is_null.as_ptr(),
            std::ptr::null(),
        );

        // Hit
        let result = rust_query_cache_lookup(cache_key);
        assert!(!result.is_null());
        rust_query_cache_release(result);

        // Miss
        let result2 = rust_query_cache_lookup(0x9999);
        assert!(result2.is_null());

        rust_query_cache_stats(&mut hits, &mut misses);
        assert!(hits >= 1, "expected at least 1 hit, got {hits}");
        assert!(misses >= 1, "expected at least 1 miss, got {misses}");
    }

    #[test]
    fn lru_eviction_works() {
        // Fill cache with QUERY_CACHE_SIZE entries
        let col_types: [u32; 1] = [23];
        let col_name = std::ffi::CString::new("x").unwrap();
        let col_names: [*const c_char; 1] = [col_name.as_ptr()];
        let val = std::ffi::CString::new("1").unwrap();
        let values: [*const c_char; 1] = [val.as_ptr()];
        let lengths: [i32; 1] = [1];
        let is_null: [i32; 1] = [0];

        for i in 0..QUERY_CACHE_SIZE {
            let key = 0x7000_0000_0000_0000_u64 + i as u64;
            rust_query_cache_store(
                key,
                1,
                1,
                col_types.as_ptr(),
                col_names.as_ptr(),
                values.as_ptr(),
                lengths.as_ptr(),
                is_null.as_ptr(),
                std::ptr::null(),
            );
        }

        // Store one more — should evict the oldest
        let new_key = 0x8000_0000_0000_0000_u64;
        rust_query_cache_store(
            new_key,
            1,
            1,
            col_types.as_ptr(),
            col_names.as_ptr(),
            values.as_ptr(),
            lengths.as_ptr(),
            is_null.as_ptr(),
            std::ptr::null(),
        );

        // The new entry should be findable
        let result = rust_query_cache_lookup(new_key);
        assert!(!result.is_null());
        rust_query_cache_release(result);
    }

    #[test]
    fn expired_entry_returns_null() {
        let cache_key: u64 = 0x5555_6666_7777_8888;
        let col_types: [u32; 1] = [23];
        let col_name = std::ffi::CString::new("t").unwrap();
        let col_names: [*const c_char; 1] = [col_name.as_ptr()];
        let val = std::ffi::CString::new("1").unwrap();
        let values: [*const c_char; 1] = [val.as_ptr()];
        let lengths: [i32; 1] = [1];
        let is_null: [i32; 1] = [0];

        rust_query_cache_store(
            cache_key,
            1,
            1,
            col_types.as_ptr(),
            col_names.as_ptr(),
            values.as_ptr(),
            lengths.as_ptr(),
            is_null.as_ptr(),
            std::ptr::null(),
        );

        // Manually backdate the entry's timestamp to force expiry
        with_cache(|cache| {
            for entry in cache.entries.iter_mut() {
                if entry.cache_key == cache_key {
                    entry.created_ms = get_time_ms_impl() - QUERY_CACHE_TTL_MS - 1;
                    break;
                }
            }
        });

        // Should be expired now
        let result = rust_query_cache_lookup(cache_key);
        assert!(result.is_null(), "expired entry should return NULL");
    }

    #[test]
    fn too_many_rows_not_cached() {
        let cache_key: u64 = 0xAAAA_1111_2222_3333;
        let col_types: [u32; 1] = [23];
        let col_name = std::ffi::CString::new("x").unwrap();
        let col_names: [*const c_char; 1] = [col_name.as_ptr()];

        // 6 rows > QUERY_CACHE_MAX_ROWS (5)
        let num_rows = 6;
        let mut values_vec = Vec::new();
        let mut lengths_vec = Vec::new();
        let mut is_null_vec = Vec::new();
        let val_strs: Vec<std::ffi::CString> = (0..num_rows)
            .map(|i| std::ffi::CString::new(format!("{i}")).unwrap())
            .collect();

        for v in &val_strs {
            values_vec.push(v.as_ptr());
            lengths_vec.push(v.as_bytes().len() as i32);
            is_null_vec.push(0i32);
        }

        rust_query_cache_store(
            cache_key,
            num_rows,
            1,
            col_types.as_ptr(),
            col_names.as_ptr(),
            values_vec.as_ptr(),
            lengths_vec.as_ptr(),
            is_null_vec.as_ptr(),
            std::ptr::null(),
        );

        // Should not be cached
        let result = rust_query_cache_lookup(cache_key);
        assert!(result.is_null(), "too many rows should not be cached");
    }

    #[test]
    fn zero_rows_not_cached() {
        let cache_key: u64 = 0xBBBB_1111_2222_3333;
        let col_types: [u32; 1] = [23];
        let col_name = std::ffi::CString::new("x").unwrap();
        let col_names: [*const c_char; 1] = [col_name.as_ptr()];

        rust_query_cache_store(
            cache_key,
            0,
            1,
            col_types.as_ptr(),
            col_names.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
        );

        let result = rust_query_cache_lookup(cache_key);
        assert!(result.is_null(), "zero rows should not be cached");
    }

    #[test]
    fn multiple_rows_roundtrip() {
        let cache_key: u64 = 0xCCCC_1111_2222_3333;
        let col_types: [u32; 2] = [23, 25];
        let col_name_0 = std::ffi::CString::new("id").unwrap();
        let col_name_1 = std::ffi::CString::new("name").unwrap();
        let col_names: [*const c_char; 2] = [col_name_0.as_ptr(), col_name_1.as_ptr()];

        // 3 rows x 2 cols = 6 values
        let v00 = std::ffi::CString::new("1").unwrap();
        let v01 = std::ffi::CString::new("Alice").unwrap();
        let v10 = std::ffi::CString::new("2").unwrap();
        let v11 = std::ffi::CString::new("Bob").unwrap();
        let v20 = std::ffi::CString::new("3").unwrap();
        let v21 = std::ffi::CString::new("Charlie").unwrap();
        let values: [*const c_char; 6] = [
            v00.as_ptr(),
            v01.as_ptr(),
            v10.as_ptr(),
            v11.as_ptr(),
            v20.as_ptr(),
            v21.as_ptr(),
        ];
        let lengths: [i32; 6] = [1, 5, 1, 3, 1, 7];
        let is_null: [i32; 6] = [0; 6];

        rust_query_cache_store(
            cache_key,
            3,
            2,
            col_types.as_ptr(),
            col_names.as_ptr(),
            values.as_ptr(),
            lengths.as_ptr(),
            is_null.as_ptr(),
            std::ptr::null(),
        );

        let result = rust_query_cache_lookup(cache_key);
        assert!(!result.is_null());

        unsafe {
            let entry = &*result;
            assert_eq!(entry.num_rows, 3);
            assert_eq!(entry.num_cols, 2);

            // Check row 2, col 1 = "Charlie"
            let row2 = &*entry.rows.add(2);
            let val = CStr::from_ptr(*row2.values.add(1));
            assert_eq!(val.to_bytes(), b"Charlie");

            rust_query_cache_release(result);
        }
    }

    // ── Thread isolation ─────────────────────────────────────────────────────

    #[test]
    fn cache_is_thread_local() {
        let cache_key: u64 = 0xDDDD_1111_2222_3333;
        let col_types: [u32; 1] = [23];
        let col_name = std::ffi::CString::new("x").unwrap();
        let col_names: [*const c_char; 1] = [col_name.as_ptr()];
        let val = std::ffi::CString::new("1").unwrap();
        let values: [*const c_char; 1] = [val.as_ptr()];
        let lengths: [i32; 1] = [1];
        let is_null: [i32; 1] = [0];

        // Store on this thread
        rust_query_cache_store(
            cache_key,
            1,
            1,
            col_types.as_ptr(),
            col_names.as_ptr(),
            values.as_ptr(),
            lengths.as_ptr(),
            is_null.as_ptr(),
            std::ptr::null(),
        );

        // Another thread should NOT see it
        let handle = std::thread::spawn(move || {
            let result = rust_query_cache_lookup(cache_key);
            result.is_null()
        });

        assert!(
            handle.join().unwrap(),
            "cache entry should not be visible on another thread"
        );
    }
}
