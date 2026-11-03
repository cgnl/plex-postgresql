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
//! prepare. Invalidation:
//! - DDL seen anywhere in the shim (prepare, free, `sqlite3_exec`) clears the
//!   cache and bumps a generation; a description taken before the bump is not
//!   stored afterwards.
//! - While DDL may be uncommitted, nothing is stored, and the end of the next
//!   transaction clears the cache again (another session may have described
//!   the old shape before the commit, or this session the new shape before a
//!   rollback).
//! - Statements that name a temporary table are never cached (temporary
//!   tables are per session; the cache is per process).
//! - An execution whose columns differ from the description replaces it (see
//!   `refresh_descriptor_if_stale`), so a schema change made by another process
//!   heals at the first execution.

use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use crate::libpq_helpers::{self, PGresult, PG_COPYRES_ATTRS};

/// Plex uses a few hundred distinct statements; these bounds only stop growth
/// from pathological dynamic SQL (e.g. long inlined id lists).
const MAX_ENTRIES: usize = 2048;
const MAX_SQL_BYTES: usize = 4096;

/// SQL text -> (parameter count, owned `PGresult` address). libpq results are
/// immutable after creation and safe to copy from any thread.
struct Cache {
    entries: HashMap<Vec<u8>, Vec<(c_int, usize)>>,
    len: usize,
    temporary_tables: Vec<Vec<u8>>,
}

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| {
    Mutex::new(Cache {
        entries: HashMap::new(),
        len: 0,
        temporary_tables: Vec::new(),
    })
});
static GENERATION: AtomicU64 = AtomicU64::new(0);
static DDL_PENDING: AtomicBool = AtomicBool::new(false);

fn with_cache<T>(f: impl FnOnce(&mut Cache) -> T) -> T {
    let mut cache = CACHE.lock().unwrap_or_else(|poison| poison.into_inner());
    f(&mut cache)
}

fn clear_entries(cache: &mut Cache) {
    for (_, variants) in cache.entries.drain() {
        for (_, res) in variants {
            libpq_helpers::rust_pq_clear(res as *mut PGresult);
        }
    }
    cache.len = 0;
}

/// Read before describing; pass to `store`.
pub(crate) fn generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

/// A private copy of the cached descriptor for `sql`, or null on a miss.
pub(crate) fn lookup(sql: &CStr, param_count: c_int) -> *mut PGresult {
    with_cache(|cache| {
        let found = cache
            .entries
            .get(sql.to_bytes())
            .and_then(|variants| variants.iter().find(|(count, _)| *count == param_count));
        match found {
            Some(&(_, res)) => unsafe { libpq_helpers::pq_copy_result(res as *const PGresult, PG_COPYRES_ATTRS) },
            None => std::ptr::null_mut(),
        }
    })
}

/// Remember a successful description (stores its own copy) unless the schema
/// may have changed since `described_at` (a `generation()` value).
pub(crate) fn store(sql: &CStr, param_count: c_int, descriptor: *const PGresult, described_at: u64) {
    let bytes = sql.to_bytes();
    if bytes.len() > MAX_SQL_BYTES || DDL_PENDING.load(Ordering::SeqCst) {
        return;
    }
    with_cache(|cache| {
        // Checked under the lock: `clear` bumps the generation under it too.
        if GENERATION.load(Ordering::SeqCst) != described_at
            || cache.temporary_tables.iter().any(|name| contains_word(bytes, name))
        {
            return;
        }
        let copy = unsafe { libpq_helpers::pq_copy_result(descriptor, PG_COPYRES_ATTRS) };
        if copy.is_null() {
            return;
        }
        if cache.len >= MAX_ENTRIES {
            clear_entries(cache);
        }
        let variants = cache.entries.entry(bytes.to_vec()).or_default();
        if let Some(slot) = variants.iter_mut().find(|(count, _)| *count == param_count) {
            libpq_helpers::rust_pq_clear(slot.1 as *mut PGresult);
            slot.1 = copy as usize;
        } else {
            variants.push((param_count, copy as usize));
            cache.len += 1;
        }
    });
}

/// Drop the entry for `sql` (an execution returned a different shape).
pub(crate) fn forget(sql: &CStr, param_count: c_int) {
    with_cache(|cache| {
        let Some(variants) = cache.entries.get_mut(sql.to_bytes()) else {
            return;
        };
        if let Some(position) = variants.iter().position(|(count, _)| *count == param_count) {
            let (_, res) = variants.swap_remove(position);
            libpq_helpers::rust_pq_clear(res as *mut PGresult);
            cache.len -= 1;
        }
        if variants.is_empty() {
            cache.entries.remove(sql.to_bytes());
        }
    });
}

/// Drop everything and invalidate descriptions still in flight.
pub(crate) fn clear() {
    with_cache(|cache| {
        GENERATION.fetch_add(1, Ordering::SeqCst);
        clear_entries(cache);
    });
}

/// A CREATE/ALTER/DROP is about to run or has run. Stops caching until the
/// end of the next transaction, which clears again.
pub(crate) fn note_ddl(sql: &[u8]) {
    DDL_PENDING.store(true, Ordering::SeqCst);
    let temporary = temporary_table_name(sql);
    with_cache(|cache| {
        GENERATION.fetch_add(1, Ordering::SeqCst);
        clear_entries(cache);
        if let Some(name) = temporary {
            if !cache.temporary_tables.contains(&name) {
                cache.temporary_tables.push(name);
            }
        }
    });
}

/// A transaction ended (COMMIT, ROLLBACK, RELEASE, ROLLBACK TO).
pub(crate) fn transaction_ended() {
    if DDL_PENDING.swap(false, Ordering::SeqCst) {
        clear();
    }
}

fn skip_space_and_comments(sql: &[u8]) -> &[u8] {
    let mut rest = sql;
    loop {
        let trimmed = rest.trim_ascii_start();
        if let Some(comment) = trimmed.strip_prefix(b"--") {
            rest = comment
                .iter()
                .position(|&b| b == b'\n')
                .map_or(&[][..], |end| &comment[end..]);
        } else if let Some(comment) = trimmed.strip_prefix(b"/*") {
            rest = comment
                .windows(2)
                .position(|w| w == b"*/")
                .map_or(&[][..], |end| &comment[end + 2..]);
        } else {
            return trimmed;
        }
    }
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn strip_keyword<'a>(sql: &'a [u8], keyword: &[u8]) -> Option<&'a [u8]> {
    let head = sql.get(..keyword.len())?;
    if !head.eq_ignore_ascii_case(keyword) || sql.get(keyword.len()).is_some_and(|&b| is_word_byte(b)) {
        return None;
    }
    Some(skip_space_and_comments(&sql[keyword.len()..]))
}

/// Cheap check for CREATE/ALTER/DROP at the start of `sql` (after whitespace
/// and comments), usable on hot paths.
pub(crate) fn starts_with_ddl(sql: &[u8]) -> bool {
    let head = skip_space_and_comments(sql);
    [b"create".as_slice(), b"alter", b"drop"]
        .iter()
        .any(|keyword| strip_keyword(head, keyword).is_some())
}

/// True when any `;`-separated statement of an exec batch is DDL. A `;`
/// inside a literal can only cause an unnecessary clear.
pub(crate) fn batch_contains_ddl(sql: &[u8]) -> bool {
    sql.split(|&b| b == b';').any(starts_with_ddl)
}

/// The lowercased table name of `CREATE TEMP[ORARY] TABLE [IF NOT EXISTS] name`.
fn temporary_table_name(sql: &[u8]) -> Option<Vec<u8>> {
    let rest = strip_keyword(skip_space_and_comments(sql), b"create")?;
    let rest = strip_keyword(rest, b"temp").or_else(|| strip_keyword(rest, b"temporary"))?;
    let mut rest = strip_keyword(rest, b"table")?;
    if let Some(after_if) = strip_keyword(rest, b"if") {
        rest = strip_keyword(strip_keyword(after_if, b"not")?, b"exists")?;
    }
    let rest = rest.strip_prefix(b"\"").unwrap_or(rest);
    let end = rest.iter().position(|&b| !is_word_byte(b)).unwrap_or(rest.len());
    (end > 0).then(|| rest[..end].to_ascii_lowercase())
}

/// Case-insensitive whole-word search of lowercase `word` in `sql`.
fn contains_word(sql: &[u8], word: &[u8]) -> bool {
    if word.is_empty() || sql.len() < word.len() {
        return false;
    }
    (0..=sql.len() - word.len()).any(|at| {
        sql[at..at + word.len()].eq_ignore_ascii_case(word)
            && (at == 0 || !is_word_byte(sql[at - 1]))
            && sql.get(at + word.len()).is_none_or(|&b| !is_word_byte(b))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddl_prefix_detection() {
        assert!(starts_with_ddl(b"CREATE TABLE t (id int)"));
        assert!(starts_with_ddl(b"  alter table t add column c int"));
        assert!(starts_with_ddl(b"\nDROP INDEX i"));
        assert!(starts_with_ddl(b"-- migrate\n/* v2 */ ALTER TABLE t ADD c int"));
        assert!(!starts_with_ddl(b"SELECT created_at FROM t"));
        assert!(!starts_with_ddl(b"created_at"));
        assert!(!starts_with_ddl(b"drop_target"));
        assert!(!starts_with_ddl(b"update t set c = 1"));
        assert!(!starts_with_ddl(b"-- drop\nselect 1"));
        assert!(!starts_with_ddl(b""));
    }

    #[test]
    fn exec_batches_with_ddl_are_detected() {
        assert!(batch_contains_ddl(b"BEGIN; ALTER TABLE t ADD c int; COMMIT"));
        assert!(!batch_contains_ddl(b"PRAGMA x; UPDATE t SET a = 1"));
    }

    #[test]
    fn temporary_table_names() {
        assert_eq!(temporary_table_name(b"CREATE TEMP TABLE scratch(id int)"), Some(b"scratch".to_vec()));
        assert_eq!(
            temporary_table_name(b"create temporary table if not exists \"Ids\" (id int)"),
            Some(b"ids".to_vec())
        );
        assert_eq!(temporary_table_name(b"CREATE TABLE t(id int)"), None);
        assert!(contains_word(b"SELECT * FROM Scratch WHERE id = 1", b"scratch"));
        assert!(!contains_word(b"SELECT * FROM scratch_items", b"scratch"));
    }
}
