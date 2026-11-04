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

/// Prepare, describe and deallocate the private descriptor statement with three
/// separate round trips. Fallback for when pipeline mode is unavailable.
unsafe fn describe_round_trips(
    conn: *mut PgConnection,
    exec_conn: *mut PgConnection,
    name: &CStr,
    sql: &CStr,
    param_count: c_int,
) -> (*mut PgResultLibpq, bool) {
    let ec = &mut *exec_conn;
    let prep = crate::libpq_helpers::rust_pq_prepare(
        ec.conn,
        name.as_ptr(),
        sql.as_ptr(),
        param_count,
        ptr::null(),
    );
    let mut round_ok = crate::libpq_helpers::rust_pq_result_status(prep) == PGRES_COMMAND_OK;
    if !round_ok {
        crate::db_interpose_exec::pg_path::record_pg_result_error(conn, exec_conn, prep);
    }
    crate::libpq_helpers::rust_pq_clear(prep);
    let mut desc = ptr::null_mut();
    if round_ok {
        desc = crate::libpq_helpers::rust_pq_describe_prepared(ec.conn, name.as_ptr());
        round_ok = crate::libpq_helpers::rust_pq_result_status(desc) == PGRES_COMMAND_OK;
        if !round_ok {
            crate::db_interpose_exec::pg_path::record_pg_result_error(conn, exec_conn, desc);
        }
        let deallocated = deallocate_descriptor(conn, exec_conn, name);
        round_ok = round_ok && deallocated;
    }
    (desc, round_ok)
}

/// The same prepare/describe/deallocate sequence in a single round trip using
/// libpq pipeline mode. Every prepared statement used to pay three network
/// round trips here before it ever executed, which dominated latency against a
/// PostgreSQL server on another host (issue #59).
///
/// Returns None when pipeline mode cannot be entered (nothing was sent), so the
/// caller can fall back to separate round trips. The flag reports that the
/// session had to be reset (a lost connection), so the caller retries once.
unsafe fn describe_pipelined(
    conn: *mut PgConnection,
    exec_conn: *mut PgConnection,
    name: &CStr,
    sql: &CStr,
    param_count: c_int,
) -> Option<(*mut PgResultLibpq, bool, bool)> {
    let pq = (*exec_conn).conn;
    if !crate::libpq_helpers::pq_enter_pipeline_mode(pq) {
        return None;
    }
    let deallocate = CString::new(format!("DEALLOCATE {}", name.to_string_lossy()))
        .expect("generated DEALLOCATE cannot contain NUL");
    let queued = crate::libpq_helpers::pq_send_prepare(pq, name.as_ptr(), sql.as_ptr(), param_count)
        && crate::libpq_helpers::pq_send_describe_prepared(pq, name.as_ptr())
        && crate::libpq_helpers::pq_send_command(pq, deallocate.as_ptr());
    let synced = crate::libpq_helpers::pq_pipeline_sync(pq);

    // Collect exactly one result per queued command (each followed by NULL),
    // then the sync marker. After an error PostgreSQL skips to the sync point
    // and libpq reports the remaining commands as PIPELINE_ABORTED.
    let mut round_ok = queued && synced;
    let mut error_recorded = false;
    let mut desc = ptr::null_mut();
    if !round_ok {
        // A send failed: libpq holds the reason; the results that were
        // queued cannot be matched to commands, so just drain to the sync.
        crate::db_interpose_exec::pg_path::record_pg_result_error(conn, exec_conn, ptr::null_mut());
        error_recorded = true;
    }
    for command in 0..if round_ok { 3 } else { 0 } {
        let res = crate::libpq_helpers::rust_pq_get_result(pq);
        if res.is_null() {
            round_ok = false;
            if !error_recorded {
                crate::db_interpose_exec::pg_path::record_pg_result_error(conn, exec_conn, res);
                error_recorded = true;
            }
            break;
        }
        let status = crate::libpq_helpers::rust_pq_result_status(res);
        if status != PGRES_COMMAND_OK {
            round_ok = false;
            if !error_recorded {
                crate::db_interpose_exec::pg_path::record_pg_result_error(conn, exec_conn, res);
                error_recorded = true;
            }
        }
        if command == 1 && status == PGRES_COMMAND_OK {
            desc = res;
        } else {
            crate::libpq_helpers::rust_pq_clear(res);
        }
        let mut trailing = crate::libpq_helpers::rust_pq_get_result(pq);
        while !trailing.is_null() {
            crate::libpq_helpers::rust_pq_clear(trailing);
            trailing = crate::libpq_helpers::rust_pq_get_result(pq);
        }
    }
    // Up to the sync marker (directly after the three commands; after a
    // failed send, whatever was queued before it).
    for _ in 0..8 {
        let res = crate::libpq_helpers::rust_pq_get_result(pq);
        if res.is_null() {
            continue;
        }
        let status = crate::libpq_helpers::rust_pq_result_status(res);
        crate::libpq_helpers::rust_pq_clear(res);
        if status == crate::libpq_helpers::PGRES_PIPELINE_SYNC {
            break;
        }
        round_ok = false;
    }
    let mut reset = false;
    if !crate::libpq_helpers::pq_exit_pipeline_mode(pq) {
        // Results are still pending (lost connection or a protocol surprise).
        // Never leave the session in pipeline mode: start a fresh one.
        round_ok = false;
        if !error_recorded {
            crate::db_interpose_exec::pg_path::record_pg_result_error(conn, exec_conn, ptr::null_mut());
        }
        crate::libpq_helpers::rust_pq_reset(pq);
        reset = true;
    }
    if !round_ok && !desc.is_null() {
        crate::libpq_helpers::rust_pq_clear(desc);
        desc = ptr::null_mut();
    }
    Some((desc, round_ok, reset))
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
/// Hand `desc` to the statement unless another thread described it first.
unsafe fn install_descriptor(pg_stmt: *mut PgStmt, desc: *mut PgResultLibpq) {
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

/// Replace a description that an execution result contradicts (the schema
/// changed underneath, e.g. a migration in another Plex process): the
/// statement's own copy, which column metadata prefers, and the cached one.
pub(crate) fn refresh_descriptor_if_stale(stmt: &mut PgStmt, result: *const PgResultLibpq) {
    if stmt.descriptor.is_null() || result.is_null() {
        return;
    }
    let columns = crate::libpq_helpers::rust_pq_nfields(result);
    let same = crate::libpq_helpers::rust_pq_nfields(stmt.descriptor) == columns
        && (0..columns)
            .all(|col| unsafe { crate::libpq_helpers::pq_field_matches(stmt.descriptor, result, col) });
    if same {
        return;
    }
    if !stmt.pg_sql.is_null() {
        let sql = unsafe { CStr::from_ptr(stmt.pg_sql) };
        crate::descriptor_cache::forget(sql, stmt.param_count);
    }
    let fresh = unsafe {
        crate::libpq_helpers::pq_copy_result(result, crate::libpq_helpers::PG_COPYRES_ATTRS)
    };
    crate::libpq_helpers::rust_pq_clear(stmt.descriptor);
    stmt.descriptor = fresh;
}

fn session_looks_healthy(ec: &mut PgConnection) -> bool {
    let _guard = unsafe { PthreadMutexGuard::lock(&mut ec.mutex as *mut _) };
    if ec.conn.is_null() || ec.streaming_active.load(Ordering::SeqCst) != 0 {
        return !ec.conn.is_null() && crate::libpq_helpers::rust_pq_status(ec.conn) == 0;
    }
    crate::libpq_helpers::rust_pq_consume_input(ec.conn);
    crate::libpq_helpers::rust_pq_status(ec.conn) == 0
}

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
    // Repeated statements reuse a cached description: no round trip. Only
    // while the session is healthy: during an outage prepare must fail as
    // before instead of publishing a statement that cannot run. Reading
    // whatever already arrived is local and notices a server that closed the
    // connection (restart, outage) without a round trip.
    let described_at = crate::descriptor_cache::generation();
    if session_looks_healthy(ec) {
        let cached = crate::descriptor_cache::lookup(&sql, param_count);
        if !cached.is_null() {
            unsafe { install_descriptor(pg_stmt, cached) };
            return true;
        }
    }
    // A backend lost since the pool handed out this connection only surfaces
    // once the first round trips fail. Mirror the step retry contract: let the
    // pool reset the session and run the pure-metadata round once more, so a
    // recovered PostgreSQL keeps prepare succeeding instead of failing it.
    for attempt in 0..2 {
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
        let (desc, round_ok, reset) = unsafe {
            match describe_pipelined(conn, exec_conn, &descriptor_name, &sql, param_count) {
                Some(outcome) => outcome,
                None => {
                    let (desc, ok) =
                        describe_round_trips(conn, exec_conn, &descriptor_name, &sql, param_count);
                    (desc, ok, false)
                }
            }
        };
        if round_ok {
            drop(conn_guard);
            crate::descriptor_cache::store(&sql, param_count, desc, described_at);
            unsafe { install_descriptor(pg_stmt, desc) };
            return true;
        }
        crate::libpq_helpers::rust_pq_clear(desc);
        drop(conn_guard);
        // No-op while the session is healthy; resets a broken one and reports
        // whether recovery was attempted, gating the single retry.
        let recovered = crate::pg_client::rust_pool_check_health(exec_conn.cast());
        if attempt == 0 && (recovered != 0 || reset) {
            continue;
        }
        return false;
    }
    false
}
