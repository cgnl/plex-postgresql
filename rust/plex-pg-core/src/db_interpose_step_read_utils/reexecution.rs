use super::*;
use crate::log_debug_lazy;

pub(crate) fn should_clear_cross_thread_result(
    stmt: *const PgStmt,
    exec_conn: *mut PgConnection,
) -> bool {
    if stmt.is_null() || exec_conn.is_null() {
        return false;
    }
    let stmt = unsafe { &*stmt };
    if stmt.result_conn == exec_conn {
        return false;
    }
    // A stream that was read ahead no longer needs its connection: its
    // remaining rows are local. Requerying it would deliver from row 0 again.
    stmt.streaming_mode != 0 && stmt.spill_state == 0
}

pub(crate) fn should_use_streaming(stmt: *const PgStmt, disable_streaming_env: bool) -> bool {
    if disable_streaming_env || stmt.is_null() {
        return false;
    }
    let stmt = unsafe { &*stmt };
    if stmt.needs_requery != 0 {
        return false;
    }

    let pg_sql = unsafe { cstr_bytes(stmt.pg_sql) };
    if contains_icase_bytes(pg_sql, b"limit 1") {
        return false;
    }

    let sql = unsafe { cstr_bytes(stmt.sql) };
    if contains_icase_bytes(sql, b"limit 1") {
        return false;
    }

    true
}

pub(crate) unsafe fn adopt_materialized_result_owner(
    stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
) -> bool {
    if stmt.is_null() || exec_conn.is_null() {
        return false;
    }
    let stmt = &mut *stmt;
    if stmt.result.is_null() || stmt.streaming_mode != 0 || stmt.result_conn == exec_conn {
        return false;
    }

    stmt.result_conn = exec_conn;
    stmt.executing_thread = libc::pthread_self();
    true
}

#[no_mangle]
pub extern "C" fn rust_step_read_prepare_reexecution_state(
    stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
) {
    if stmt.is_null() {
        return;
    }
    let stmt_ref = unsafe { &mut *stmt };
    let _stmt_guard = unsafe { PgStmt::lock_mutex(stmt) };
    if should_clear_cross_thread_result(stmt, exec_conn) {
        stmt_ref.needs_requery = 1;
        log_debug_lazy!(
            "STEP: Streaming stmt crossed threads; forcing eager requery (result_conn={:p} exec_conn={:p})",
            stmt_ref.result_conn,
            exec_conn
        );
        crate::pg_statement::rust_stmt_clear_result(stmt);
    } else if unsafe { adopt_materialized_result_owner(stmt, exec_conn) } {
        log_debug_lazy!(
            "STEP: Reusing materialized eager result across threads (result_conn={:p} exec_conn={:p})",
            stmt_ref.result_conn,
            exec_conn
        );
    }

    if !stmt_ref.result.is_null() && stmt_ref.metadata_only_result != 0 {
        log_debug("STEP: Clearing metadata-only result for re-execution");
        crate::libpq_helpers::rust_pq_clear(stmt_ref.result);
        stmt_ref.result = std::ptr::null_mut();
        stmt_ref.metadata_only_result = 0;
        stmt_ref.current_row = -1;
    }
}
