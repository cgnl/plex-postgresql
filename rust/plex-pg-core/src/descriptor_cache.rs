//! Process-wide cache of result column descriptors, keyed by translated SQL.
//!
//! Every prepared row-producing statement must be described before it is
//! published (prepare-time errors and column metadata, as SQLite provides them).
//! Plex prepares the same few hundred statements over and over, so describing
//! each one on the server cost a network round trip per prepare (issue #59).
//! A successful description is remembered and handed out as a private copy
//! (`PQcopyResult` with column attributes only); no network traffic.
//!
//! Only successful descriptions are cached, so invalid SQL still fails at
//! prepare. The cache is cleared whenever DDL passes through the shim, and an
//! entry is dropped when an execution result disagrees with it (schema changed
//! by another process).

use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::c_int;
use std::sync::{LazyLock, Mutex};

use crate::libpq_helpers::{self, PGresult, PG_COPYRES_ATTRS};

/// Plex uses a few hundred distinct statements; this bound only stops growth
/// from pathological dynamic SQL.
const MAX_ENTRIES: usize = 4096;

type Key = (Vec<u8>, c_int);

/// Values are owned `PGresult` pointers stored as addresses (libpq results are
/// immutable after creation and safe to copy from any thread).
static CACHE: LazyLock<Mutex<HashMap<Key, usize>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn with_cache<T>(f: impl FnOnce(&mut HashMap<Key, usize>) -> T) -> T {
    let mut cache = CACHE.lock().unwrap_or_else(|poison| poison.into_inner());
    f(&mut cache)
}

fn clear_entries(cache: &mut HashMap<Key, usize>) {
    for (_, res) in cache.drain() {
        libpq_helpers::rust_pq_clear(res as *mut PGresult);
    }
}

/// A private copy of the cached descriptor for `sql`, or null on a miss.
pub(crate) fn lookup(sql: &CStr, param_count: c_int) -> *mut PGresult {
    with_cache(|cache| match cache.get(&(sql.to_bytes().to_vec(), param_count)) {
        Some(&res) => unsafe { libpq_helpers::pq_copy_result(res as *const PGresult, PG_COPYRES_ATTRS) },
        None => std::ptr::null_mut(),
    })
}

/// Remember a successful description (stores its own copy).
pub(crate) fn store(sql: &CStr, param_count: c_int, descriptor: *const PGresult) {
    let copy = unsafe { libpq_helpers::pq_copy_result(descriptor, PG_COPYRES_ATTRS) };
    if copy.is_null() {
        return;
    }
    with_cache(|cache| {
        if cache.len() >= MAX_ENTRIES {
            clear_entries(cache);
        }
        if let Some(previous) = cache.insert((sql.to_bytes().to_vec(), param_count), copy as usize) {
            libpq_helpers::rust_pq_clear(previous as *mut PGresult);
        }
    });
}

/// Drop the entry for `sql` (an execution returned a different shape).
pub(crate) fn forget(sql: &CStr, param_count: c_int) {
    with_cache(|cache| {
        if let Some(res) = cache.remove(&(sql.to_bytes().to_vec(), param_count)) {
            libpq_helpers::rust_pq_clear(res as *mut PGresult);
        }
    });
}

/// Drop everything (the schema may have changed).
pub(crate) fn clear() {
    with_cache(clear_entries);
}

/// Cheap check for CREATE/ALTER/DROP at the start of `sql` (after whitespace),
/// usable on hot paths where `pg_config::is_ddl_str` (full lowercasing) is not.
pub(crate) fn starts_with_ddl(sql: &[u8]) -> bool {
    let start = sql.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(sql.len());
    let head = &sql[start..];
    [b"create".as_slice(), b"alter", b"drop"].iter().any(|keyword| {
        head.len() > keyword.len()
            && head[..keyword.len()].eq_ignore_ascii_case(keyword)
            && !head[keyword.len()].is_ascii_alphanumeric()
            && head[keyword.len()] != b'_'
    })
}

#[cfg(test)]
mod tests {
    use super::starts_with_ddl;

    #[test]
    fn ddl_prefix_detection() {
        assert!(starts_with_ddl(b"CREATE TABLE t (id int)"));
        assert!(starts_with_ddl(b"  alter table t add column c int"));
        assert!(starts_with_ddl(b"\nDROP INDEX i"));
        assert!(!starts_with_ddl(b"SELECT created_at FROM t"));
        assert!(!starts_with_ddl(b"created_at"));
        assert!(!starts_with_ddl(b"drop_target"));
        assert!(!starts_with_ddl(b"update t set c = 1"));
        assert!(!starts_with_ddl(b""));
    }
}
