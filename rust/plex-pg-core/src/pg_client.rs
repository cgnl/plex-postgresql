/// Module: pg_client
///
/// Pool manager, registry, and prepared-statement cache logic extracted
/// from `pg_client.c`. libpq calls are routed through Rust wrappers so
/// the C side can stay as a thin shim.
///
/// FFI surface (called from `src/pg_client.c`):
///   rust_hash_sql(sql)              → u64   FNV-1a hash for prepared-stmt cache keys
///   rust_is_stale_sqlstate(s)       → i32   1 if SQLSTATE == "26000"
///   rust_is_duplicate_sqlstate(s)   → i32   1 if SQLSTATE == "42P05"
mod c_abi;
#[path = "pg_client_config.rs"]
mod config;
mod connection_helpers;
mod connection_lifecycle;
mod globals;
mod hash_sqlstate;
mod pool_acquire;
mod pool_api;
mod pool_lookup;
mod pool_runtime;
mod pool_state;
#[path = "pg_client_registry.rs"]
mod registry;
mod registry_api;
mod session;
mod support;
mod threading;
mod tls_cache;
pub(crate) mod transaction;

#[allow(unused_imports)]
use crate::db_interpose_conn_utils::{log_debug, log_error, log_info};
#[allow(unused_imports)]
use crate::ffi_types::PgConnection;
pub use crate::pg_client_stmt_cache::{
    rust_stmt_cache_add, rust_stmt_cache_clear, rust_stmt_cache_clear_local, rust_stmt_cache_drop,
    rust_stmt_cache_lookup,
};
pub use c_abi::*;
use config::parse_positive_env_or_default;
pub(crate) use connection_helpers::current_thread_has_other_streaming_connection;
use connection_helpers::{conn_db_path, conn_is_pg_active_ptr};
use connection_lifecycle::{close_handle_connection, destroy_pool_connection};
pub use connection_lifecycle::{rust_pg_close, rust_pg_connect, rust_pg_ensure_connection};
pub use globals::{
    rust_get_global_last_insert_rowid, rust_get_global_metadata_id,
    rust_set_global_last_insert_rowid, rust_set_global_metadata_id,
};
#[allow(unused_imports)]
use hash_sqlstate::{fnv1a_str, is_duplicate_sqlstate, is_stale_sqlstate};
pub use hash_sqlstate::{rust_hash_sql, rust_is_duplicate_sqlstate, rust_is_stale_sqlstate};
use pool_acquire::{pool_get_connection_inner, pool_get_connection_inner_excluding};
pub use pool_api::*;
use pool_lookup::{is_library_db, pool_find_connection_for_db};
#[allow(unused_imports)]
pub(crate) use pool_state::{
    pool, PoolManager, PoolSlot, POOL, POOL_SIZE_DEFAULT, SLOT_ERROR, SLOT_FREE, SLOT_READY,
    SLOT_RECONNECTING, SLOT_RESERVED,
};
#[allow(unused_imports)]
use registry::{ConnectionRegistry, DbToPool};
pub use registry_api::*;
use support::{cbuf_to_string, conn_config, write_str_to_cbuf};
pub(crate) use support::{ConnConfig, CLIENT_INIT};

// ─── Pool algorithm helpers ──────────────────────────────────────────────────

// Pool acquisition is implemented in `pg_client/pool_acquire.rs`.

// ─── Find Connection (for pg_find_connection) ────────────────────────────────

// Pool/client FFI and init are implemented in `pg_client/pool_api.rs`.

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
