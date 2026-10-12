use super::*;

pub(super) unsafe fn eager_fetch_result(
    pg_stmt: *mut PgStmt,
    exec_conn_io: *mut *mut PgConnection,
    exec_conn: *mut PgConnection,
    stmt_guard: &mut Option<StmtGuard>,
    conn_guard: &mut PthreadMutexGuard,
) -> c_int {
    let s = &mut *pg_stmt;
    let ec = &*exec_conn;
    // Clear any pre-existing result (e.g. from metadata-only fetch) to prevent leak
    if !s.result.is_null() {
        crate::libpq_helpers::rust_pq_clear(s.result);
    }
    s.result = crate::libpq_helpers::rust_pq_get_result(ec.conn);
    let mut trail = crate::libpq_helpers::rust_pq_get_result(ec.conn);
    while !trail.is_null() {
        crate::libpq_helpers::rust_pq_clear(trail);
        trail = crate::libpq_helpers::rust_pq_get_result(ec.conn);
    }
    conn_guard.unlock();

    if !s.result.is_null()
        && crate::libpq_helpers::rust_pq_result_status(s.result) == PGRES_TUPLES_OK
    {
        crate::db_interpose_exec::pg_path::clear_pg_last_error(s.conn);
        s.num_rows = crate::libpq_helpers::rust_pq_ntuples(s.result);
        s.num_cols = crate::libpq_helpers::rust_pq_nfields(s.result);
        s.ensure_column_capacity(s.num_cols as usize);
        s.current_row = 0;
        s.result_conn = exec_conn;
        s.metadata_only_result = 0;
        resolve_column_tables(pg_stmt, exec_conn);
        trace_play_queue_result(pg_stmt, s.result, "EAGER");
        if s.num_rows > 0 {
            *exec_conn_io = exec_conn;
            *stmt_guard = None; // unlock
            return STEP_RESULT_ROW;
        }
    } else if !s.result.is_null() {
        let err2 = crate::libpq_helpers::rust_pq_error_message(ec.conn);
        let ctx = CString::new("EAGER FALLBACK").unwrap();
        let fallback_err = if err2.is_null() {
            CString::new("?").unwrap()
        } else {
            CString::new(cstr_to_str(err2)).unwrap_or_else(|_| CString::new("?").unwrap())
        };
        log_sql_fallback(s.sql, s.pg_sql, fallback_err.as_ptr(), ctx.as_ptr());
        crate::libpq_helpers::rust_pq_clear(s.result);
        s.result = std::ptr::null_mut();
        crate::pg_client::rust_pool_check_health(exec_conn as *mut c_void);
    }

    s.read_done = 1;
    *exec_conn_io = exec_conn;
    *stmt_guard = None; // unlock
    STEP_RESULT_DONE
}
