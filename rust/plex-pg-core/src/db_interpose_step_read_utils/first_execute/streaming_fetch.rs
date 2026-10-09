use super::*;
use crate::log_debug_lazy;

unsafe fn clear_streaming_state(s: &mut PgStmt) {
    s.streaming_mode = 0;
    if !s.streaming_conn.is_null() {
        let sc = &*s.streaming_conn;
        sc.streaming_active.store(0, Ordering::SeqCst);
    }
    s.streaming_conn = std::ptr::null_mut();
}

unsafe fn finish_streaming_done(
    s: &mut PgStmt,
    exec_conn_io: *mut *mut PgConnection,
    exec_conn: *mut PgConnection,
    stmt_guard: &mut Option<StmtGuard>,
) -> c_int {
    s.read_done = 1;
    *exec_conn_io = exec_conn;
    *stmt_guard = None; // unlock
    STEP_RESULT_DONE
}

pub(super) unsafe fn streaming_fetch_result(
    pg_stmt: *mut PgStmt,
    exec_conn_io: *mut *mut PgConnection,
    exec_conn: *mut PgConnection,
    stmt_guard: &mut Option<StmtGuard>,
    conn_guard: &mut PthreadMutexGuard,
    pg_conn_error_out: *mut c_int,
) -> c_int {
    let s = &mut *pg_stmt;
    let ec = &mut *exec_conn;
    s.streaming_mode = 1;
    s.streaming_conn = exec_conn;
    s.result_conn = exec_conn;
    ec.streaming_active.store(1, Ordering::SeqCst);
    s.metadata_only_result = 0;
    conn_guard.unlock();

    let first_res = crate::libpq_helpers::rust_pq_get_result(ec.conn);
    if first_res.is_null() {
        clear_streaming_state(s);
        s.result_conn = std::ptr::null_mut();
        s.read_done = -1;
        crate::db_interpose_exec::pg_path::set_pg_last_error(
            s.conn,
            "PostgreSQL stream returned no initial result",
        );
        *exec_conn_io = exec_conn;
        *stmt_guard = None;
        return STEP_RESULT_ERROR;
    }

    let first_status = crate::libpq_helpers::rust_pq_result_status(first_res);
    if first_status == PGRES_SINGLE_TUPLE {
        // Clear any pre-existing result (e.g. from metadata-only fetch) to prevent leak
        if !s.result.is_null() {
            crate::libpq_helpers::rust_pq_clear(s.result);
        }
        s.result = first_res;
        s.current_row = 0;
        s.num_rows = 1;
        s.num_cols = crate::libpq_helpers::rust_pq_nfields(first_res);
        s.ensure_column_capacity(s.num_cols as usize);
        resolve_column_tables(pg_stmt, exec_conn);
        trace_play_queue_result(pg_stmt, first_res, "STREAM FIRST");
        *exec_conn_io = exec_conn;
        *stmt_guard = None; // unlock
        return STEP_RESULT_ROW;
    }

    if first_status == PGRES_TUPLES_OK {
        log_debug_lazy!(
            "STREAM: zero rows returned for sql={:.200}",
            cstr_to_str(s.pg_sql)
        );
        crate::libpq_helpers::rust_pq_clear(first_res);
        let final_null = crate::libpq_helpers::rust_pq_get_result(ec.conn);
        if !final_null.is_null() {
            crate::libpq_helpers::rust_pq_clear(final_null);
        }
        clear_streaming_state(s);
        s.num_cols = 0;
        s.num_rows = 0;
        return finish_streaming_done(s, exec_conn_io, exec_conn, stmt_guard);
    }

    let err = crate::libpq_helpers::rust_pq_error_message(ec.conn);
    log_error(&format!(
        "STREAM first fetch error: {} (status={}) sql={:.200}",
        cstr_to_str(err),
        first_status,
        cstr_to_str(s.pg_sql)
    ));
    if is_stale_prepared_stmt(first_res) {
        crate::pg_client::rust_stmt_cache_clear_local(exec_conn as *mut c_void);
    }
    crate::db_interpose_exec::pg_path::record_pg_result_error(s.conn, exec_conn, first_res);
    crate::libpq_helpers::rust_pq_clear(first_res);
    let mut drain = crate::libpq_helpers::rust_pq_get_result(ec.conn);
    while !drain.is_null() {
        crate::libpq_helpers::rust_pq_clear(drain);
        drain = crate::libpq_helpers::rust_pq_get_result(ec.conn);
    }
    clear_streaming_state(s);
    s.result_conn = std::ptr::null_mut();
    s.read_done = -1;
    crate::pg_client::rust_pool_check_health(exec_conn as *mut c_void);
    *exec_conn_io = exec_conn;
    *stmt_guard = None; // unlock
    set_pg_conn_error(pg_conn_error_out);
    STEP_RESULT_ERROR
}
