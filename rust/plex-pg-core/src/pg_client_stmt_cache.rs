use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::sync::{Mutex, OnceLock};

use crate::ffi_types::{PgConnection, STMT_NAME_LEN};
use crate::libpq_helpers::{rust_pq_clear, rust_pq_exec};
use crate::log_debug_lazy;
use crate::log_info_lazy;

pub(crate) const STMT_CACHE_SIZE: usize = 512;

/// Per-connection prepared statement cache entry.
#[derive(Clone)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct StmtCacheEntry {
    pub sql_hash: u64,
    pub stmt_name: [c_char; STMT_NAME_LEN],
    pub param_count: i32,
    pub last_used: i64,
}

/// Per-connection prepared statement cache (hash table with linear probing).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct StmtCache {
    entries: Vec<Option<StmtCacheEntry>>,
    count: usize,
}

#[cfg_attr(not(test), allow(dead_code))]
impl StmtCache {
    pub fn new() -> Self {
        Self {
            entries: (0..STMT_CACHE_SIZE).map(|_| None).collect(),
            count: 0,
        }
    }

    /// Lookup by sql_hash (mutable) to update last_used.
    pub fn lookup_mut(&mut self, sql_hash: u64) -> Option<&mut StmtCacheEntry> {
        if sql_hash == 0 {
            return None;
        }
        let start = (sql_hash as usize) & (STMT_CACHE_SIZE - 1);
        let entries_ptr = self.entries.as_mut_ptr();
        for i in 0..STMT_CACHE_SIZE {
            let idx = (start + i) & (STMT_CACHE_SIZE - 1);
            let slot = unsafe { &mut *entries_ptr.add(idx) };
            match slot {
                Some(entry) if entry.sql_hash == sql_hash => return Some(entry),
                None => return None, // empty slot = end of probe chain
                _ => continue,
            }
        }
        None
    }

    /// Lookup by sql_hash. Returns Some(&entry) on hit, None on miss.
    pub fn lookup(&self, sql_hash: u64) -> Option<&StmtCacheEntry> {
        if sql_hash == 0 {
            return None;
        }
        let start = (sql_hash as usize) & (STMT_CACHE_SIZE - 1);
        for i in 0..STMT_CACHE_SIZE {
            let idx = (start + i) & (STMT_CACHE_SIZE - 1);
            match &self.entries[idx] {
                Some(entry) if entry.sql_hash == sql_hash => return Some(entry),
                None => return None, // empty slot = end of probe chain
                _ => continue,       // collision, keep probing
            }
        }
        None
    }

    /// Add or update entry. Returns (index, evicted_name) if eviction occurred.
    pub fn add(
        &mut self,
        sql_hash: u64,
        stmt_name: [c_char; STMT_NAME_LEN],
        param_count: i32,
        now: i64,
    ) -> (i32, Option<[c_char; STMT_NAME_LEN]>) {
        if sql_hash == 0 {
            return (-1, None);
        }
        let start = (sql_hash as usize) & (STMT_CACHE_SIZE - 1);

        let mut oldest_idx: Option<usize> = None;
        let mut oldest_time = i64::MAX;

        // First pass: find existing or empty slot
        for i in 0..STMT_CACHE_SIZE {
            let idx = (start + i) & (STMT_CACHE_SIZE - 1);
            match self.entries[idx].as_mut() {
                Some(entry) => {
                    if entry.last_used < oldest_time {
                        oldest_time = entry.last_used;
                        oldest_idx = Some(idx);
                    }
                    if entry.sql_hash == sql_hash {
                        // Update existing
                        entry.stmt_name = stmt_name;
                        entry.param_count = param_count;
                        entry.last_used = now;
                        return (idx as i32, None);
                    }
                }
                None => {
                    // Empty slot, insert here
                    self.entries[idx] = Some(StmtCacheEntry {
                        sql_hash,
                        stmt_name,
                        param_count,
                        last_used: now,
                    });
                    self.count += 1;
                    return (idx as i32, None);
                }
            }
        }

        // Table is full, evict LRU entry
        if let Some(lru_idx) = oldest_idx {
            let evicted = self.entries[lru_idx].take().map(|e| e.stmt_name);
            self.entries[lru_idx] = Some(StmtCacheEntry {
                sql_hash,
                stmt_name,
                param_count,
                last_used: now,
            });
            return (lru_idx as i32, evicted);
        }

        (-1, None)
    }

    /// Clear all entries. Returns names of all evicted statements (for DEALLOCATE).
    pub fn clear(&mut self) -> Vec<[c_char; STMT_NAME_LEN]> {
        let mut evicted = Vec::new();
        for entry in &mut self.entries {
            if let Some(e) = entry.take() {
                evicted.push(e.stmt_name);
            }
        }
        self.count = 0;
        evicted
    }

    pub fn count(&self) -> usize {
        self.count
    }
}

// ─── Per-connection StmtCache registry (keyed by conn ptr) ───────────────────

/// Maps pg_connection_t* (as usize) → StmtCache.
/// Each pool connection has its own prepared statement cache.
static STMT_CACHES: OnceLock<Mutex<HashMap<usize, StmtCache>>> = OnceLock::new();

fn stmt_caches() -> &'static Mutex<HashMap<usize, StmtCache>> {
    STMT_CACHES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn clear_all_stmt_caches() {
    if let Some(caches) = STMT_CACHES.get() {
        caches.lock().unwrap().clear();
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn stmt_name_from_c(ptr: *const c_char) -> Option<[c_char; STMT_NAME_LEN]> {
    if ptr.is_null() {
        return None;
    }
    let bytes = unsafe { CStr::from_ptr(ptr) }.to_bytes();
    let mut buf = [0 as c_char; STMT_NAME_LEN];
    let len = bytes.len().min(STMT_NAME_LEN.saturating_sub(1));
    for (i, b) in bytes[..len].iter().enumerate() {
        buf[i] = *b as c_char;
    }
    buf[len] = 0;
    Some(buf)
}

fn stmt_name_to_string(name: &[c_char; STMT_NAME_LEN]) -> String {
    unsafe { CStr::from_ptr(name.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

fn deallocate_stmt(conn: *mut c_void, stmt_name: &[c_char; STMT_NAME_LEN]) {
    if conn.is_null() {
        return;
    }
    let pg_conn = unsafe { (*(conn as *mut PgConnection)).conn };
    if pg_conn.is_null() {
        return;
    }
    let name = stmt_name_to_string(stmt_name);
    if name.is_empty() {
        return;
    }
    let sql = format!("DEALLOCATE {}", name);
    if let Ok(cs) = CString::new(sql) {
        let res = rust_pq_exec(pg_conn, cs.as_ptr());
        if !res.is_null() {
            rust_pq_clear(res);
        }
    }
}

/// Lookup statement in cache by hash.
/// Returns 1 on hit, 0 on miss. Writes stmt_name_out to cached name on hit.
pub fn rust_stmt_cache_lookup(
    conn: *mut c_void,
    sql_hash: u64,
    stmt_name_out: *mut *const c_char,
) -> i32 {
    if stmt_name_out.is_null() {
        return 0;
    }
    unsafe {
        *stmt_name_out = std::ptr::null();
    }
    if conn.is_null() || sql_hash == 0 {
        return 0;
    }

    let mut caches = stmt_caches().lock().unwrap();
    let cache = match caches.get_mut(&(conn as usize)) {
        Some(c) => c,
        None => return 0,
    };

    if let Some(entry) = cache.lookup_mut(sql_hash) {
        entry.last_used = now_secs();
        unsafe {
            *stmt_name_out = entry.stmt_name.as_ptr();
        }
        return 1;
    }
    0
}

/// Add statement to cache. Returns index on success, -1 on failure.
pub fn rust_stmt_cache_add(
    conn: *mut c_void,
    sql_hash: u64,
    stmt_name: *const c_char,
    param_count: i32,
) -> i32 {
    if conn.is_null() || sql_hash == 0 {
        return -1;
    }
    let name = match stmt_name_from_c(stmt_name) {
        Some(n) => n,
        None => return -1,
    };

    let now = now_secs();
    let (idx, evicted) = {
        let mut caches = stmt_caches().lock().unwrap();
        let cache = caches.entry(conn as usize).or_insert_with(StmtCache::new);
        cache.add(sql_hash, name, param_count, now)
    };

    if let Some(evicted_name) = evicted {
        deallocate_stmt(conn, &evicted_name);
        log_debug_lazy!(
            "Evicted prepared statement from cache: {}",
            stmt_name_to_string(&evicted_name)
        );
    }

    idx
}

/// Clear local prepared statement cache without sending DEALLOCATE to server.
pub fn rust_stmt_cache_clear_local(conn: *mut c_void) {
    if conn.is_null() {
        return;
    }
    let mut caches = stmt_caches().lock().unwrap();
    if let Some(cache) = caches.get_mut(&(conn as usize)) {
        cache.clear();
    }
    log_info_lazy!(
        "Cleared prepared statement cache (local only) for connection {:p}",
        conn
    );
}

/// Clear all cached statements for a connection (includes DEALLOCATE).
pub fn rust_stmt_cache_clear(conn: *mut c_void) {
    if conn.is_null() {
        return;
    }

    let evicted = {
        let mut caches = stmt_caches().lock().unwrap();
        if let Some(cache) = caches.get_mut(&(conn as usize)) {
            cache.clear()
        } else {
            Vec::new()
        }
    };

    for name in &evicted {
        deallocate_stmt(conn, name);
    }

    log_debug_lazy!("Cleared prepared statement cache for connection {:p}", conn);
}

/// Drop cache entry for a connection (no DEALLOCATE).
pub fn rust_stmt_cache_drop(conn: *mut c_void) {
    if conn.is_null() {
        return;
    }
    let mut caches = stmt_caches().lock().unwrap();
    caches.remove(&(conn as usize));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stmt_cache_initial_empty() {
        let cache = StmtCache::new();
        assert_eq!(cache.count(), 0);
    }

    #[test]
    fn stmt_cache_add_and_lookup() {
        let mut cache = StmtCache::new();
        let mut name = [0 as c_char; STMT_NAME_LEN];
        name[0] = b'a' as c_char;
        let (idx, evicted) = cache.add(123, name, 1, 1);
        assert!(idx >= 0);
        assert!(evicted.is_none());
        let hit = cache.lookup(123);
        assert!(hit.is_some());
    }

    #[test]
    fn stmt_cache_lookup_miss() {
        let cache = StmtCache::new();
        assert!(cache.lookup(999).is_none());
    }

    #[test]
    fn stmt_cache_lookup_zero_hash_returns_none() {
        let cache = StmtCache::new();
        assert!(cache.lookup(0).is_none());
    }

    #[test]
    fn stmt_cache_add_zero_hash_is_noop() {
        let mut cache = StmtCache::new();
        let name = [0 as c_char; STMT_NAME_LEN];
        let (idx, evicted) = cache.add(0, name, 0, 0);
        assert_eq!(idx, -1);
        assert!(evicted.is_none());
    }

    #[test]
    fn stmt_cache_update_existing() {
        let mut cache = StmtCache::new();
        let mut name = [0 as c_char; STMT_NAME_LEN];
        name[0] = b'a' as c_char;
        cache.add(123, name, 1, 1);
        let mut name2 = [0 as c_char; STMT_NAME_LEN];
        name2[0] = b'b' as c_char;
        let (idx, evicted) = cache.add(123, name2, 2, 2);
        assert!(idx >= 0);
        assert!(evicted.is_none());
        let hit = cache.lookup(123).unwrap();
        assert_eq!(hit.param_count, 2);
        assert_eq!(hit.stmt_name, name2);
    }

    #[test]
    fn stmt_cache_multiple_entries() {
        let mut cache = StmtCache::new();
        let mut name = [0 as c_char; STMT_NAME_LEN];
        name[0] = b'a' as c_char;
        cache.add(1, name, 1, 1);
        let mut name2 = [0 as c_char; STMT_NAME_LEN];
        name2[0] = b'b' as c_char;
        cache.add(2, name2, 1, 2);
        assert!(cache.lookup(1).is_some());
        assert!(cache.lookup(2).is_some());
    }

    #[test]
    fn stmt_cache_clear_returns_names() {
        let mut cache = StmtCache::new();
        let mut name = [0 as c_char; STMT_NAME_LEN];
        name[0] = b'a' as c_char;
        cache.add(1, name, 1, 1);
        let evicted = cache.clear();
        assert_eq!(evicted.len(), 1);
    }

    #[test]
    fn stmt_cache_clear_empty_returns_empty() {
        let mut cache = StmtCache::new();
        let evicted = cache.clear();
        assert!(evicted.is_empty());
    }

    #[test]
    fn stmt_cache_eviction_when_full() {
        let mut cache = StmtCache::new();
        for i in 1..=STMT_CACHE_SIZE as u64 {
            let mut name = [0 as c_char; STMT_NAME_LEN];
            name[0] = (i % 255) as u8 as c_char;
            cache.add(i, name, 1, i as i64);
        }
        assert_eq!(cache.count(), STMT_CACHE_SIZE);
        let mut name = [0 as c_char; STMT_NAME_LEN];
        name[0] = b'z' as c_char;
        let (_idx, evicted) = cache.add(9999, name, 1, (STMT_CACHE_SIZE as i64) + 1);
        assert!(evicted.is_some());
    }

    #[test]
    fn stmt_cache_linear_probing_handles_collision() {
        let mut cache = StmtCache::new();
        let mut name = [0 as c_char; STMT_NAME_LEN];
        name[0] = b'a' as c_char;
        cache.add(1, name, 1, 1);
        let mut name2 = [0 as c_char; STMT_NAME_LEN];
        name2[0] = b'b' as c_char;
        let h2 = 1 + STMT_CACHE_SIZE as u64; // same bucket
        cache.add(h2, name2, 1, 2);
        assert!(cache.lookup(1).is_some());
        assert!(cache.lookup(h2).is_some());
    }
}
