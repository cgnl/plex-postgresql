use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicI32, Ordering};

mod cached_write;
mod connection;
mod logging;
mod special_insert;
mod support;
mod write_exec;

use crate::byte_utils::{
    contains_bytes, contains_icase_bytes, cstr_bytes, starts_with_icase_bytes,
};
use crate::db_interpose_conn_utils::{
    apply_pg_session_settings, connect_new, cstr_prefix, cstr_to_string_or, log_error, log_info,
    PgConnConfig, PthreadMutexGuard,
};
use crate::env_utils;
use crate::ffi_types::{sqlite3, sqlite3_stmt, PgConnection, PgStmt, StmtGuard, STMT_NAME_LEN};
use crate::libpq_helpers::PGresult;
pub use cached_write::{
    rust_step_cached_write_build_exec_sql, rust_step_cached_write_execute_and_finalize,
    rust_step_cached_write_should_noop, rust_step_pg_write_should_noop,
};
pub use connection::{rust_step_pick_thread_connection, rust_step_write_prepare_connection};
pub use logging::{rust_step_log_step_exit_trace, rust_step_write_log_debug_context};
pub use special_insert::rust_step_write_should_skip_special_insert;
use support::{
    is_duplicate_prepared_stmt, is_stale_prepared_stmt, malloc_cstring, owned_db_path, param_at,
    skip_stats_resources_update,
};
pub use write_exec::rust_step_write_execute_and_finalize;

const SQLITE_DONE: c_int = 101;
const SQLITE_ROW: c_int = 100;
const SQLITE_ERROR: c_int = 1;
const STEP_RESULT_DONE: c_int = SQLITE_DONE;
const STEP_RESULT_ERROR: c_int = SQLITE_ERROR;

const PGRES_COMMAND_OK: c_int = 1;
const PGRES_TUPLES_OK: c_int = 2;
const CONNECTION_OK: c_int = 0;
const PG_DIAG_SQLSTATE: c_int = b'C' as c_int;

static SKIP_STATS_RESOURCES_UPDATE: AtomicI32 = AtomicI32::new(-1);

extern "C" {
    fn sqlite3_db_handle(stmt: *mut sqlite3_stmt) -> *mut sqlite3;
    fn log_sql_fallback(
        original_sql: *const c_char,
        translated_sql: *const c_char,
        error_msg: *const c_char,
        context: *const c_char,
    );
    fn platform_print_backtrace(reason: *const c_char, skip_frames: c_int);
    fn pg_config_get() -> *mut PgConnConfig;
}
