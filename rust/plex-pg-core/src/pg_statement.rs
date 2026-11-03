/// Module: pg_statement
///
/// Statement root for the PostgreSQL shim. It owns shared statement state and
/// re-exports the focused submodules that implement the FFI surface.
///
/// **Phase 3 migration**: the Rust side now owns the statement registry
/// (hash map), TLS cached statement list, and fake sqlite3_value pool.
/// The C file `pg_statement.c` is only a thin shim.
///
/// ## Memory safety fixes (vs. C original)
///
/// - **HIGH #2 fix**: Refcount ABA race in `atomic_fetch_sub` — Rust uses
///   `fetch_sub` with `Ordering::AcqRel` plus explicit underflow detection
///   that restores the count to 0 instead of going negative.
///
/// - **HIGH #3 fix**: TLS destructor frees statement still in global registry.
///   The Rust TLS `Drop` calls `unref` which only triggers `pg_stmt_free`
///   when the last reference is gone. The registry holds its own reference.
///
/// ## Design
///
/// - Statement registry: `RwLock<HashMap<usize, usize>>` for O(1) lookup
///   (key = sqlite3_stmt* as usize, value = pg_stmt_t* as usize)
/// - TLS cached stmts: `thread_local!` with `Drop` that unrefs all entries
/// - pg_value pool: lock-free circular buffer with `AtomicU32`
/// - All existing pure helpers (OID mapping, upsert, metadata ID) unchanged
///
/// Focused submodules now hold:
///
/// - registry/refcount API
/// - TLS cached statement API
/// - metadata/decltype helper API
/// - statement construction
/// - lifecycle logic
/// - value pool logic
use std::sync::{Once, RwLock};

#[cfg(test)]
use crate::ffi_types::PgStmt;

pub(crate) mod c_abi;
mod metadata_api;
mod metadata_helpers;
mod registry;
mod registry_api;
mod stmt_constants;
mod stmt_factory;
mod stmt_lifecycle;
mod stmt_support;
mod stream_spill;
mod tls_cache;
mod tls_cache_api;
mod value_pool;

pub use c_abi::*;
pub use metadata_api::*;
pub(crate) use metadata_helpers::{
    convert_metadata_settings_upsert, extract_metadata_id, oid_to_sqlite_decltype,
    oid_to_sqlite_type,
};
use registry::StmtRegistry;
pub use registry_api::*;
use stmt_constants::{
    DECLTYPE_CASE_DT_INTEGER_8, DECLTYPE_CASE_NONE, DECLTYPE_CASE_NULL, SQLITE_BLOB, SQLITE_FLOAT,
    SQLITE_INTEGER, SQLITE_NULL, SQLITE_TEXT,
};
pub use stmt_factory::rust_stmt_create;
pub use stmt_lifecycle::{rust_stmt_clear_result, rust_stmt_free};
pub(crate) use stream_spill::{
    abort_live_stream_locked, release_deferred_spill_refs, spill_live_stream_locked,
};
use stmt_support::{
    leak_enabled, stmt_cache_disabled, stmt_ref_ptr, stmt_unref_ptr, MAX_CACHED_STMTS_PER_THREAD,
};
use tls_cache::with_tls_cache;
#[cfg(test)]
use tls_cache::ThreadCachedStmts;
pub use tls_cache_api::*;
pub use value_pool::{rust_create_column_value, rust_is_our_value, PgValue};
#[cfg(test)]
use value_pool::{MAX_PG_VALUES, PG_VALUE_MAGIC};

// ─── Statement Registry (global, RwLock-protected) ───────────────────────────

/// Global statement registry: maps sqlite3_stmt* → pg_stmt_t*.
///
/// Uses `usize` as the key/value to avoid carrying raw pointer types through
/// the RwLock. The C shim casts to/from `void*`.
///
/// A secondary reverse map tracks pg_stmt_t* → sqlite3_stmt* for the
/// `pg_is_our_stmt` lookup (which searches by pg_stmt, not sqlite_stmt).
pub(in crate::pg_statement) static REGISTRY: std::sync::LazyLock<RwLock<StmtRegistry>> =
    std::sync::LazyLock::new(|| RwLock::new(StmtRegistry::new()));
pub(in crate::pg_statement) static STMT_INIT: Once = Once::new();

// ═══════════════════════════════════════════════════════════════════════════════
// Unit tests
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
#[path = "pg_statement/tests.rs"]
mod tests;
