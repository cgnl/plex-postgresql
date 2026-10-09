use super::*;
use crate::log_info_lazy;

pub(super) unsafe fn acquire_exec_connection(
    pg_stmt: *mut PgStmt,
    exec_conn_io: *mut *mut PgConnection,
    stmt_guard: &mut Option<StmtGuard>,
    pg_conn_error_out: *mut c_int,
) -> Result<*mut PgConnection, c_int> {
    let mut exec_conn = *exec_conn_io;
    let handle_conn = (*pg_stmt).conn;
    if !handle_conn.is_null() && !(*handle_conn).shadow_db.is_null() {
        crate::pg_client::transaction::ensure_handle_session(handle_conn);
        if !crate::pg_client::transaction::session_ready(handle_conn) {
            *stmt_guard = None;
            if !crate::pg_client::transaction::transaction_active(handle_conn) {
                set_pg_conn_error(pg_conn_error_out);
            }
            return Err(STEP_RESULT_ERROR);
        }
        *exec_conn_io = handle_conn;
        return Ok(handle_conn);
    }
    if exec_conn.is_null() || (&*exec_conn).conn.is_null() {
        log_error(&format!(
            "STEP SELECT: NULL connection, retrying in 500ms (exec_conn={:p})",
            exec_conn
        ));
        *stmt_guard = None; // unlock
        libc::usleep(500_000);
        *stmt_guard = Some(PgStmt::lock_mutex(pg_stmt)); // relock

        let ps = &*pg_stmt;
        let retry_db = sqlite3_db_handle(ps.shadow_stmt);
        let retry_handle = crate::pg_client::rust_pg_find_connection(retry_db);
        if !retry_handle.is_null() {
            let rh = &*retry_handle;
            if rh.db_path[0] != 0 {
                exec_conn = crate::pg_client::rust_pool_get_connection(rh.db_path.as_ptr())
                    as *mut PgConnection;
            }
        }
        if exec_conn.is_null() || (&*exec_conn).conn.is_null() {
            log_error("STEP SELECT: NULL connection after retry - giving up");
            *stmt_guard = None; // unlock
            set_pg_conn_error(pg_conn_error_out);
            return Err(STEP_RESULT_ERROR);
        }
        log_error(&format!(
            "STEP SELECT: reconnect retry succeeded (exec_conn={:p})",
            exec_conn
        ));
    }

    Ok(exec_conn)
}

pub(super) unsafe fn lock_exec_connection(
    exec_conn: &mut *mut PgConnection,
    stmt_guard: &mut Option<StmtGuard>,
    pg_conn_error_out: *mut c_int,
) -> Result<PthreadMutexGuard, c_int> {
    crate::pg_client::rust_pool_touch_connection(*exec_conn as *const c_void);
    let ec = &mut **exec_conn;
    let mut conn_guard = PthreadMutexGuard::lock(&mut ec.mutex as *mut _);

    if !ec.shadow_db.is_null() {
        if !crate::pg_client::transaction::session_ready_locked(*exec_conn) {
            *stmt_guard = None;
            if !crate::pg_client::transaction::transaction_active(*exec_conn) {
                set_pg_conn_error(pg_conn_error_out);
            }
            return Err(STEP_RESULT_ERROR);
        }
        if crate::pg_client::transaction::result_stream_live_locked(*exec_conn) {
            crate::pg_client::transaction::set_error(
                *exec_conn,
                "Read cannot switch an active handle session while a result is streaming",
            );
            *stmt_guard = None;
            return Err(STEP_RESULT_ERROR);
        }
        return Ok(conn_guard);
    }

    if ec.conn.is_null() {
        log_error("STEP SELECT: conn became NULL after lock (TOCTOU race)");
        conn_guard.unlock();
        *stmt_guard = None; // unlock
        set_pg_conn_error(pg_conn_error_out);
        return Err(STEP_RESULT_ERROR);
    }

    if ec.streaming_active.load(Ordering::SeqCst) != 0 {
        let alt_db_path = owned_db_path(*exec_conn);
        log_info_lazy!(
            "STEP SELECT: conn {:p} became streaming_active after lock, getting new connection",
            *exec_conn
        );
        conn_guard.unlock();
        let Some(alt_db_path) = alt_db_path else {
            log_error("STEP SELECT: live db_path unavailable for alternate connection");
            *stmt_guard = None; // unlock
            set_pg_conn_error(pg_conn_error_out);
            return Err(STEP_RESULT_ERROR);
        };
        let alt_conn = crate::pg_client::rust_pool_get_connection_excluding(
            alt_db_path.as_ptr(),
            *exec_conn as *const c_void,
        ) as *mut PgConnection;
        if !alt_conn.is_null()
            && !(&*alt_conn).conn.is_null()
            && alt_conn != *exec_conn
            && (&*alt_conn).streaming_active.load(Ordering::SeqCst) == 0
        {
            *exec_conn = alt_conn;
            crate::pg_client::rust_pool_touch_connection(*exec_conn as *const c_void);
            let ec2 = &mut **exec_conn;
            conn_guard = PthreadMutexGuard::lock(&mut ec2.mutex as *mut _);
            if ec2.conn.is_null() || ec2.streaming_active.load(Ordering::SeqCst) != 0 {
                log_error("STEP SELECT: alt conn also unavailable");
                conn_guard.unlock();
                *stmt_guard = None; // unlock
                set_pg_conn_error(pg_conn_error_out);
                return Err(STEP_RESULT_ERROR);
            }
        } else {
            log_error(&format!(
                "STEP SELECT: no non-streaming connection available (db_path={} exec_conn={:p} alt_conn={:p})",
                alt_db_path.to_string_lossy(),
                *exec_conn,
                alt_conn,
            ));
            *stmt_guard = None; // unlock
            set_pg_conn_error(pg_conn_error_out);
            return Err(STEP_RESULT_ERROR);
        }
    }

    Ok(conn_guard)
}

pub(super) unsafe fn ensure_connection_ready(
    pg_stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
    stmt_guard: &mut Option<StmtGuard>,
    conn_guard: &mut PthreadMutexGuard,
    pg_conn_error_out: *mut c_int,
) -> Result<(), c_int> {
    let ec = &mut *exec_conn;
    if !ec.shadow_db.is_null() {
        if crate::pg_client::transaction::session_ready_locked(exec_conn) {
            return Ok(());
        }
        conn_guard.unlock();
        *stmt_guard = None;
        return Err(STEP_RESULT_ERROR);
    }
    let conn_status = crate::libpq_helpers::rust_pq_status(ec.conn);
    if conn_status == CONNECTION_OK {
        return Ok(());
    }

    let pg_err = crate::libpq_helpers::rust_pq_error_message(ec.conn);
    log_error("=== CONNECTION_BAD DIAGNOSTIC (READ) ===");
    log_error(&format!(
        "  Status: {}, Thread: {:p}",
        conn_status,
        libc::pthread_self() as usize as *const c_void
    ));
    log_error(&format!(
        "  Connection: {:p}, PGconn: {:p}",
        exec_conn, ec.conn
    ));
    log_error(&format!("  PG Error: {}", cstr_to_str(pg_err)));
    log_error(&format!("  SQL: {:.100}", cstr_to_str((&*pg_stmt).sql)));
    if let Ok(reason) = CString::new("CONNECTION_BAD in STEP READ") {
        platform_print_backtrace(reason.as_ptr(), 1);
    }
    log_error("=== END DIAGNOSTIC ===");
    log_error("STEP READ: Attempting PQreset...");
    crate::libpq_helpers::rust_pq_reset(ec.conn);

    if crate::libpq_helpers::rust_pq_status(ec.conn) != CONNECTION_OK {
        log_error("STEP READ: PQreset failed, trying fresh PQconnectdb...");
        crate::pg_client::rust_stmt_cache_clear(exec_conn as *mut c_void);
        crate::libpq_helpers::rust_pq_finish(ec.conn);
        ec.conn = std::ptr::null_mut();

        let rcfg = pg_config_get();
        if rcfg.is_null() {
            log_error("STEP READ: pg_config_get returned NULL");
            ec.is_pg_active = 0;
            conn_guard.unlock();
            *stmt_guard = None; // unlock
            set_pg_conn_error(pg_conn_error_out);
            return Err(STEP_RESULT_ERROR);
        }
        let new_read_conn = connect_new(&*rcfg);
        if crate::libpq_helpers::rust_pq_status(new_read_conn) == CONNECTION_OK {
            ec.conn = new_read_conn;
            ec.is_pg_active = 1;
            log_info("STEP READ: fresh connection succeeded (reconnected)");
        } else {
            let reset_err = crate::libpq_helpers::rust_pq_error_message(new_read_conn);
            log_error(&format!(
                "STEP READ: fresh connection also failed: {}",
                cstr_to_str(reset_err)
            ));
            crate::libpq_helpers::rust_pq_finish(new_read_conn);
            ec.is_pg_active = 0;
            conn_guard.unlock();
            *stmt_guard = None; // unlock
            set_pg_conn_error(pg_conn_error_out);
            return Err(STEP_RESULT_ERROR);
        }
    } else {
        log_error("STEP READ: PQreset succeeded, connection recovered");
    }

    let cfg = pg_config_get();
    if !cfg.is_null() {
        apply_pg_session_settings(ec.conn, &*cfg);
    }
    Ok(())
}
