use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicI32, Ordering};

mod first_execute;
mod next_result;
mod play_queue_trace;
mod reexecution;
mod support;

use crate::byte_utils::{contains_icase_bytes, cstr_bytes};
use crate::db_interpose_conn_utils::{
    apply_pg_session_settings, connect_new, log_debug, log_error, log_info, PgConnConfig,
    PthreadMutexGuard,
};
use crate::ffi_types::{sqlite3, sqlite3_stmt, PgConnection, PgStmt, StmtGuard};
use crate::libpq_helpers::PGresult;
use first_execute::first_execute_impl;
use next_result::{
    advance_cached_result_impl, eager_next_impl, log_debug_context_impl, streaming_next_impl,
};
use play_queue_trace::{trace_play_queue_params, trace_play_queue_result};
pub use reexecution::rust_step_read_prepare_reexecution_state;
#[allow(unused_imports)]
use reexecution::{
    adopt_materialized_result_owner, should_clear_cross_thread_result, should_use_streaming,
};
use support::{
    cstr_to_str, is_duplicate_prepared_stmt, is_stale_prepared_stmt, owned_db_path,
    step_read_clear_row_caches,
};

const SQLITE_DONE: c_int = 101;
const SQLITE_ROW: c_int = 100;
const SQLITE_ERROR: c_int = 1;
const STEP_RESULT_DONE: c_int = SQLITE_DONE;
const STEP_RESULT_ROW: c_int = SQLITE_ROW;
const STEP_RESULT_ERROR: c_int = SQLITE_ERROR;

const PGRES_COMMAND_OK: c_int = 1;
const PGRES_TUPLES_OK: c_int = 2;
const PGRES_SINGLE_TUPLE: c_int = 9;
const CONNECTION_OK: c_int = 0;
const PG_DIAG_SQLSTATE: c_int = b'C' as c_int;

use crate::c_abi::resolve_column_tables;

extern "C" {
    fn sqlite3_db_handle(stmt: *mut sqlite3_stmt) -> *mut sqlite3;
    fn log_sql_fallback(
        original_sql: *const c_char,
        translated_sql: *const c_char,
        error_msg: *const c_char,
        context: *const c_char,
    );
    fn platform_print_backtrace(reason: *const c_char, skip_frames: c_int);
    fn pg_exception_note_query(sql: *const c_char);
    fn pg_config_get() -> *mut PgConnConfig;
}

#[no_mangle]
pub extern "C" fn rust_step_read_advance_cached_result(stmt: *mut PgStmt) -> c_int {
    advance_cached_result_impl(stmt)
}

#[no_mangle]
pub extern "C" fn rust_step_read_streaming_next(
    p_stmt: *mut sqlite3_stmt,
    stmt: *mut PgStmt,
) -> c_int {
    streaming_next_impl(p_stmt, stmt)
}

#[no_mangle]
pub extern "C" fn rust_step_read_eager_next(stmt: *mut PgStmt) -> c_int {
    eager_next_impl(stmt)
}

#[no_mangle]
pub extern "C" fn rust_step_read_first_execute(
    pg_stmt: *mut PgStmt,
    exec_conn_io: *mut *mut PgConnection,
    param_values: *const *const c_char,
    pg_conn_error_out: *mut c_int,
) -> c_int {
    first_execute_impl(pg_stmt, exec_conn_io, param_values, pg_conn_error_out)
}

#[no_mangle]
pub extern "C" fn rust_step_read_log_debug_context(
    stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
) {
    log_debug_context_impl(stmt, exec_conn)
}

#[cfg(test)]
mod tests {
    use super::{
        adopt_materialized_result_owner, rust_step_read_prepare_reexecution_state,
        should_clear_cross_thread_result, should_use_streaming,
    };
    use crate::ffi_types::{PgConnection, PgStmt};
    use crate::libpq_helpers::PGresult;
    use crate::pg_statement::{rust_stmt_create, rust_stmt_free};
    use std::ffi::CString;

    fn make_stmt() -> *mut PgStmt {
        let sql = CString::new("SELECT 1").unwrap();
        let stmt = rust_stmt_create(std::ptr::null_mut(), sql.as_ptr(), std::ptr::null_mut());
        assert!(!stmt.is_null());
        stmt
    }

    #[test]
    fn cross_thread_reexecution_only_clears_streaming_results() {
        let stmt_ptr = make_stmt();
        let old_conn = 0x1234usize as *mut PgConnection;
        let new_conn = 0x5678usize as *mut PgConnection;

        let s = unsafe { &mut *stmt_ptr };
        s.result_conn = old_conn;
        s.streaming_mode = 1;

        assert!(should_clear_cross_thread_result(stmt_ptr, new_conn));

        rust_stmt_free(stmt_ptr);
    }

    #[test]
    fn cross_thread_reexecution_keeps_read_ahead_streams() {
        let stmt_ptr = make_stmt();
        let s = unsafe { &mut *stmt_ptr };
        s.result_conn = 0x1234usize as *mut PgConnection;
        s.streaming_mode = 1;
        s.spill_state = 1;

        // The remaining rows are local; a requery would replay from row 0.
        assert!(!should_clear_cross_thread_result(stmt_ptr, 0x5678usize as *mut PgConnection));

        let s = unsafe { &mut *stmt_ptr };
        s.spill_state = 0;
        rust_stmt_free(stmt_ptr);
    }

    #[test]
    fn cross_thread_reexecution_keeps_materialized_eager_results() {
        let stmt_ptr = make_stmt();
        let result = 0x1234usize as *mut PGresult;
        let old_conn = 0x2345usize as *mut PgConnection;
        let new_conn = 0x3456usize as *mut PgConnection;

        let s = unsafe { &mut *stmt_ptr };
        s.result = result;
        s.result_conn = old_conn;
        s.streaming_mode = 0;

        assert!(!should_clear_cross_thread_result(stmt_ptr, new_conn));
        assert!(unsafe { adopt_materialized_result_owner(stmt_ptr, new_conn) });

        let s = unsafe { &mut *stmt_ptr };
        assert_eq!(s.result, result);
        assert_eq!(s.result_conn, new_conn);
        s.result = std::ptr::null_mut();
        s.result_conn = std::ptr::null_mut();

        rust_stmt_free(stmt_ptr);
    }

    #[test]
    fn prepare_reexecution_state_adopts_materialized_eager_result() {
        let stmt_ptr = make_stmt();
        let result = 0x1234usize as *mut PGresult;
        let old_conn = 0x4567usize as *mut PgConnection;
        let new_conn = 0x5678usize as *mut PgConnection;

        let s = unsafe { &mut *stmt_ptr };
        s.result = result;
        s.result_conn = old_conn;
        s.streaming_mode = 0;
        s.metadata_only_result = 0;

        rust_step_read_prepare_reexecution_state(stmt_ptr, new_conn);

        let s = unsafe { &mut *stmt_ptr };
        assert_eq!(s.result, result);
        assert_eq!(s.result_conn, new_conn);
        s.result = std::ptr::null_mut();
        s.result_conn = std::ptr::null_mut();

        rust_stmt_free(stmt_ptr);
    }

    #[test]
    fn prepare_reexecution_state_marks_cross_thread_streaming_stmt_for_eager_requery() {
        let stmt_ptr = make_stmt();
        let old_conn = 0x4567usize as *mut PgConnection;
        let new_conn = 0x5678usize as *mut PgConnection;

        let s = unsafe { &mut *stmt_ptr };
        s.streaming_mode = 1;
        s.streaming_conn = old_conn;
        s.result_conn = old_conn;
        s.needs_requery = 0;

        rust_step_read_prepare_reexecution_state(stmt_ptr, new_conn);

        let s = unsafe { &*stmt_ptr };
        assert_eq!(s.needs_requery, 1);
        assert_eq!(s.streaming_mode, 0);
        assert!(s.streaming_conn.is_null());
        assert!(s.result_conn.is_null());

        rust_stmt_free(stmt_ptr);
    }

    #[test]
    fn should_use_streaming_respects_cross_thread_eager_fallback_flag() {
        let stmt_ptr = make_stmt();

        let s = unsafe { &mut *stmt_ptr };
        s.needs_requery = 0;
        assert!(should_use_streaming(stmt_ptr, false));

        let s = unsafe { &mut *stmt_ptr };
        s.needs_requery = 1;
        assert!(!should_use_streaming(stmt_ptr, false));
        assert!(!should_use_streaming(stmt_ptr, true));

        rust_stmt_free(stmt_ptr);
    }

    #[test]
    fn should_use_streaming_disables_limit_one_pg_probes() {
        let sql = CString::new("select * from metadata_items limit 1").unwrap();
        let stmt_ptr = rust_stmt_create(std::ptr::null_mut(), sql.as_ptr(), std::ptr::null_mut());
        assert!(!stmt_ptr.is_null());

        let pg_sql = CString::new("SELECT * FROM metadata_items LIMIT 1").unwrap();
        let s = unsafe { &mut *stmt_ptr };
        s.pg_sql = unsafe { libc::strdup(pg_sql.as_ptr()) };

        assert!(!should_use_streaming(stmt_ptr, false));

        rust_stmt_free(stmt_ptr);
    }

    #[test]
    fn should_use_streaming_keeps_non_limit_queries() {
        let stmt_ptr = make_stmt();

        let s = unsafe { &mut *stmt_ptr };
        s.needs_requery = 0;

        assert!(should_use_streaming(stmt_ptr, false));

        rust_stmt_free(stmt_ptr);
    }
}
