use super::*;
use crate::log_debug_lazy;

static DESCRIPTOR_NAME_SEQUENCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

unsafe fn deallocate_descriptor(
    conn: *mut PgConnection,
    exec_conn: *mut PgConnection,
    name: &CStr,
) -> bool {
    let ec = &mut *exec_conn;
    let command = CString::new(format!("DEALLOCATE {}", name.to_string_lossy()))
        .expect("generated DEALLOCATE cannot contain NUL");
    let result = crate::libpq_helpers::rust_pq_exec(ec.conn, command.as_ptr());
    let ok = crate::libpq_helpers::rust_pq_result_status(result) == PGRES_COMMAND_OK;
    if !ok {
        crate::db_interpose_exec::pg_path::record_pg_result_error(conn, exec_conn, result);
    }
    crate::libpq_helpers::rust_pq_clear(result);
    ok
}

pub(crate) fn mask_collection_metadata_type(
    pg_stmt: &PgStmt,
    col_name: *const c_char,
    raw_val: i64,
    out: &mut i64,
) -> bool {
    if col_name.is_null() {
        return false;
    }
    let sql_ptr = pg_stmt.pg_sql;
    if sql_ptr.is_null() {
        return false;
    }
    let rc = crate::db_interpose_helpers::rust_should_mask_collection_metadata_type(
        sql_ptr, col_name, raw_val,
    );
    if rc == 0 {
        return false;
    }
    let row = pg_stmt.current_row;
    log_debug_lazy!(
        "COMPAT_TYPE18: masking metadata_type 18 -> 0 for related-items query, row {}",
        row
    );
    *out = 0;
    true
}

#[cfg(test)]
pub(crate) unsafe fn set_metadata_result_state(
    pg_stmt: &mut PgStmt,
    result: *mut PgResultLibpq,
    exec_conn: *mut PgConnection,
    num_rows: c_int,
    current_row: c_int,
) {
    pg_stmt.result = result;
    pg_stmt.num_rows = num_rows;
    pg_stmt.current_row = current_row;
    pg_stmt.result_conn = exec_conn;
    pg_stmt.metadata_only_result = 1;
}

/// Obtain result descriptors without executing the statement or consuming its stream.
/// Connection and statement locks are deliberately never held together.
pub(crate) fn ensure_pg_result_for_metadata(pg_stmt: *mut PgStmt) -> bool {
    if pg_stmt.is_null() {
        return false;
    }
    let (conn, sql, param_count) = unsafe {
        let _guard = PgStmt::lock_mutex(pg_stmt);
        let s = &*pg_stmt;
        if !s.descriptor.is_null() {
            return true;
        }
        if s.pg_sql.is_null() || s.conn.is_null() {
            return false;
        }
        (s.conn, CStr::from_ptr(s.pg_sql).to_owned(), s.param_count)
    };
    let conn_ref = unsafe { &*conn };
    let thread_conn = unsafe {
        if conn_ref.streaming_active.load(Ordering::SeqCst) != 0 {
            pg_get_thread_connection_excluding(conn_ref.db_path.as_ptr(), conn.cast())
        } else {
            pg_get_thread_connection(conn_ref.db_path.as_ptr())
        }
    };
    let exec_conn = if thread_conn.is_null() {
        conn
    } else {
        thread_conn
    };
    let ec = unsafe { &mut *exec_conn };
    let conn_guard = unsafe { PthreadMutexGuard::lock(&mut ec.mutex as *mut _) };
    if ec.conn.is_null() || ec.streaming_active.load(Ordering::SeqCst) != 0 {
        return false;
    }
    // Use a private, unique name so metadata discovery cannot replace libpq's
    // connection-scoped unnamed statement or collide with the execution cache.
    // Preparing/describing evaluates no expressions and performs no writes.
    let descriptor_name = CString::new(format!(
        "plex_descriptor_{}_{}",
        std::process::id(),
        DESCRIPTOR_NAME_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
    .expect("generated descriptor name cannot contain NUL");
    crate::libpq_helpers::rust_pq_set_nonblocking(ec.conn, 0);
    let prep = crate::libpq_helpers::rust_pq_prepare(
        ec.conn,
        descriptor_name.as_ptr(),
        sql.as_ptr(),
        param_count,
        ptr::null(),
    );
    let prepared = crate::libpq_helpers::rust_pq_result_status(prep) == PGRES_COMMAND_OK;
    if !prepared {
        unsafe {
            crate::db_interpose_exec::pg_path::record_pg_result_error(conn, exec_conn, prep);
        }
    }
    crate::libpq_helpers::rust_pq_clear(prep);
    if !prepared {
        return false;
    }
    let desc = crate::libpq_helpers::rust_pq_describe_prepared(ec.conn, descriptor_name.as_ptr());
    let described = crate::libpq_helpers::rust_pq_result_status(desc) == PGRES_COMMAND_OK;
    if !described {
        unsafe {
            crate::db_interpose_exec::pg_path::record_pg_result_error(conn, exec_conn, desc);
        }
    }
    let deallocated =
        unsafe { deallocate_descriptor(conn, exec_conn, CStr::from_ptr(descriptor_name.as_ptr())) };
    if !described || !deallocated {
        crate::libpq_helpers::rust_pq_clear(desc);
        drop(conn_guard);
        if !deallocated {
            crate::pg_client::rust_pool_check_health(exec_conn.cast());
        }
        return false;
    }
    drop(conn_guard);
    unsafe {
        let _guard = PgStmt::lock_mutex(pg_stmt);
        let s = &mut *pg_stmt;
        if s.descriptor.is_null() {
            s.descriptor = desc;
            s.num_cols = crate::libpq_helpers::rust_pq_nfields(desc);
            s.ensure_column_capacity(s.num_cols as usize);
        } else {
            crate::libpq_helpers::rust_pq_clear(desc);
        }
    }
    true
}
