use super::*;
use crate::log_debug_lazy;
use crate::byte_utils::{contains_icase_bytes, cstr_bytes};
use crate::db_interpose_conn_utils::{cstr_prefix, cstr_to_string_or};

pub(super) fn advance_cached_result_impl(stmt: *mut PgStmt) -> c_int {
    if stmt.is_null() {
        return STEP_RESULT_ERROR;
    }
    let stmt_ptr = stmt;
    let stmt = unsafe { &mut *stmt };
    if stmt.cached_result.is_null() {
        return STEP_RESULT_ERROR;
    }
    let _stmt_guard = unsafe { PgStmt::lock_mutex(stmt_ptr) };

    stmt.current_row += 1;
    if stmt.current_row >= stmt.num_rows {
        crate::pg_query_cache::rust_query_cache_release(stmt.cached_result);
        stmt.cached_result = std::ptr::null_mut();
        stmt.read_done = 1;
        return STEP_RESULT_DONE;
    }
    STEP_RESULT_ROW
}

pub(super) fn streaming_next_impl(p_stmt: *mut sqlite3_stmt, stmt: *mut PgStmt) -> c_int {
    if stmt.is_null() {
        return STEP_RESULT_ERROR;
    }
    let stmt_ptr = stmt;
    let stmt = unsafe { &mut *stmt };
    let stmt_guard = unsafe { PgStmt::lock_mutex(stmt_ptr) };

    if stmt.streaming_mode == 0 || stmt.streaming_conn.is_null() {
        return STEP_RESULT_ERROR;
    }

    if unsafe { (*stmt.streaming_conn).conn.is_null() } {
        log_error(&format!(
            "STREAM: conn disappeared before next row, forcing cleanup stmt={:p} sql={:.100} streaming_conn={:p}",
            p_stmt,
            cstr_to_str(stmt.pg_sql),
            stmt.streaming_conn
        ));
        unsafe {
            (*stmt.streaming_conn)
                .streaming_active
                .store(0, Ordering::SeqCst);
        }
        stmt.streaming_mode = 0;
        stmt.streaming_conn = std::ptr::null_mut();
        stmt.result_conn = std::ptr::null_mut();
        stmt.read_done = 1;
        return STEP_RESULT_DONE;
    }

    if !stmt.result.is_null() {
        crate::libpq_helpers::rust_pq_clear(stmt.result);
        stmt.result = std::ptr::null_mut();
    }
    step_read_clear_row_caches(stmt as *mut PgStmt);

    let row_res = crate::libpq_helpers::rust_pq_get_result(unsafe { (*stmt.streaming_conn).conn });
    if row_res.is_null() {
        log_error(&format!(
            "STREAM: NULL result (unexpected!) stmt={:p} sql={:.100} streaming_conn={:p}",
            p_stmt,
            cstr_to_str(stmt.pg_sql),
            stmt.streaming_conn
        ));
        stmt.streaming_mode = 0;
        if !stmt.streaming_conn.is_null() {
            unsafe {
                (*stmt.streaming_conn)
                    .streaming_active
                    .store(0, Ordering::SeqCst);
            }
        }
        stmt.streaming_conn = std::ptr::null_mut();
        stmt.read_done = 1;
        return STEP_RESULT_DONE;
    }

    let row_status = crate::libpq_helpers::rust_pq_result_status(row_res);
    if row_status == PGRES_SINGLE_TUPLE {
        stmt.result = row_res;
        stmt.current_row = 0;
        stmt.num_rows = 1;
        stmt.num_cols = crate::libpq_helpers::rust_pq_nfields(row_res);
        stmt.ensure_column_capacity(stmt.num_cols as usize);
        unsafe { trace_play_queue_result(stmt as *mut PgStmt, row_res, "STREAM NEXT") };
        drop(stmt_guard);
        return STEP_RESULT_ROW;
    }
    if row_status == PGRES_TUPLES_OK {
        crate::libpq_helpers::rust_pq_clear(row_res);
        let final_null =
            crate::libpq_helpers::rust_pq_get_result(unsafe { (*stmt.streaming_conn).conn });
        if !final_null.is_null() {
            crate::libpq_helpers::rust_pq_clear(final_null);
        }
        stmt.streaming_mode = 0;
        if !stmt.streaming_conn.is_null() {
            unsafe {
                (*stmt.streaming_conn)
                    .streaming_active
                    .store(0, Ordering::SeqCst);
            }
        }
        stmt.streaming_conn = std::ptr::null_mut();
        stmt.read_done = 1;
        return STEP_RESULT_DONE;
    }

    let err = crate::libpq_helpers::rust_pq_error_message(unsafe { (*stmt.streaming_conn).conn });
    log_error(&format!(
        "STREAM ERROR: {} (status={}) sql={:.100}",
        cstr_to_str(err),
        row_status,
        cstr_to_str(stmt.pg_sql)
    ));
    crate::libpq_helpers::rust_pq_clear(row_res);
    let mut drain =
        crate::libpq_helpers::rust_pq_get_result(unsafe { (*stmt.streaming_conn).conn });
    while !drain.is_null() {
        crate::libpq_helpers::rust_pq_clear(drain);
        drain = crate::libpq_helpers::rust_pq_get_result(unsafe { (*stmt.streaming_conn).conn });
    }
    stmt.streaming_mode = 0;
    if !stmt.streaming_conn.is_null() {
        unsafe {
            (*stmt.streaming_conn)
                .streaming_active
                .store(0, Ordering::SeqCst);
        }
    }
    stmt.streaming_conn = std::ptr::null_mut();
    stmt.read_done = 1;
    STEP_RESULT_DONE
}

pub(super) fn eager_next_impl(stmt: *mut PgStmt) -> c_int {
    if stmt.is_null() {
        return STEP_RESULT_ERROR;
    }
    let stmt_ptr = stmt;
    let stmt = unsafe { &mut *stmt };
    if stmt.result.is_null() {
        return STEP_RESULT_ERROR;
    }
    let _stmt_guard = unsafe { PgStmt::lock_mutex(stmt_ptr) };

    stmt.current_row += 1;
    if stmt.current_row >= stmt.num_rows {
        crate::libpq_helpers::rust_pq_clear(stmt.result);
        stmt.result = std::ptr::null_mut();
        stmt.result_conn = std::ptr::null_mut();
        stmt.read_done = 1;
        return STEP_RESULT_DONE;
    }
    STEP_RESULT_ROW
}

pub(super) fn log_debug_context_impl(stmt: *mut PgStmt, exec_conn: *mut PgConnection) {
    if stmt.is_null() {
        return;
    }
    let stmt_ref = unsafe { &*stmt };
    if stmt_ref.result.is_null() {
        log_debug_lazy!(
            "STEP READ: thread={:p} stmt={:p} exec_conn={:p}",
            unsafe { libc::pthread_self() as usize as *const c_void },
            stmt,
            exec_conn
        );
        let has_match = unsafe {
            !stmt_ref.sql.is_null()
                && (contains_icase_bytes(cstr_bytes(stmt_ref.sql), b"devices")
                    || contains_icase_bytes(cstr_bytes(stmt_ref.sql), b"library_sections"))
            || !stmt_ref.pg_sql.is_null()
                && (contains_icase_bytes(cstr_bytes(stmt_ref.pg_sql), b"devices")
                    || contains_icase_bytes(cstr_bytes(stmt_ref.pg_sql), b"library_sections"))
        };
        if has_match {
            log_debug_lazy!(
                "STEP READ devices/library_sections: param_count={} is_pg={}",
                stmt_ref.param_count,
                stmt_ref.is_pg
            );
            for i in 0..(stmt_ref.param_count as usize) {
                if i < stmt_ref.param_values.len() {
                    let p = stmt_ref.param_values[i];
                    log_debug_lazy!("  PARAM[{}]: {}", i, cstr_to_string_or(p, "NULL"));
                } else {
                    log_debug_lazy!("  PARAM[{}]: OUT_OF_BOUNDS", i);
                }
            }
            log_debug_lazy!("  SQL: {}", cstr_prefix(stmt_ref.sql, 1000, "NULL"));
            if !stmt_ref.pg_sql.is_null() {
                log_debug_lazy!("  PG_SQL: {}", cstr_prefix(stmt_ref.pg_sql, 1000, "NULL"));
            }
        }
    }
}
