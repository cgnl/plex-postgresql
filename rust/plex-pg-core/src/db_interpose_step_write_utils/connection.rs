use super::*;
use crate::log_info_lazy;

#[no_mangle]
pub extern "C" fn rust_step_pick_thread_connection(
    base_conn: *mut PgConnection,
) -> *mut PgConnection {
    if base_conn.is_null() {
        return std::ptr::null_mut();
    }
    let bc = unsafe { &*base_conn };
    if !bc.shadow_db.is_null() {
        return base_conn;
    }
    if crate::db_interpose_helpers::rust_is_library_or_blobs_db_path(bc.db_path.as_ptr()) == 0 {
        return base_conn;
    }

    let thread_conn = unsafe { crate::pg_client::rust_pool_get_connection(bc.db_path.as_ptr()) }
        as *mut PgConnection;
    if !thread_conn.is_null() {
        let tc = unsafe { &*thread_conn };
        if tc.is_pg_active != 0 && !tc.conn.is_null() {
            return thread_conn;
        }
    }
    base_conn
}

#[no_mangle]
pub extern "C" fn rust_step_write_prepare_connection(
    pg_stmt: *mut PgStmt,
    exec_conn_io: *mut *mut PgConnection,
    pg_conn_error_out: *mut c_int,
) -> c_int {
    unsafe {
        if !pg_conn_error_out.is_null() {
            *pg_conn_error_out = 0;
        }
        if pg_stmt.is_null() || exec_conn_io.is_null() {
            return STEP_RESULT_ERROR;
        }
        let stmt = &mut *pg_stmt;
        let mut _stmt_guard: Option<StmtGuard> = Some(PgStmt::lock_mutex(pg_stmt));

        let mut exec_conn = *exec_conn_io;
        if !stmt.conn.is_null() && !(*stmt.conn).shadow_db.is_null() {
            exec_conn = stmt.conn;
            *exec_conn_io = exec_conn;
            let _conn_guard = PthreadMutexGuard::lock(&mut (*exec_conn).mutex);
            if !crate::pg_client::transaction::session_ready_locked(exec_conn) {
                return STEP_RESULT_ERROR;
            }
            if crate::pg_client::transaction::result_stream_live_locked(exec_conn) {
                crate::pg_client::transaction::set_error(
                    exec_conn,
                    "Write cannot switch an active handle session while a result is streaming",
                );
                return STEP_RESULT_ERROR;
            }
            return STEP_RESULT_DONE;
        }
        if exec_conn.is_null() || (&*exec_conn).conn.is_null() {
            log_error(&format!(
                "STEP WRITE: NULL connection, retrying in 500ms (exec_conn={:p})",
                exec_conn
            ));
            _stmt_guard = None; // unlock
            libc::usleep(500_000);
            _stmt_guard = Some(PgStmt::lock_mutex(pg_stmt)); // relock

            let retry_db = sqlite3_db_handle(stmt.shadow_stmt);
            let retry_handle = crate::pg_client::rust_pg_find_connection(retry_db);
            if !retry_handle.is_null() {
                let rh = &*retry_handle;
                if rh.db_path[0] != 0 {
                    exec_conn = crate::pg_client::rust_pool_get_connection(rh.db_path.as_ptr())
                        as *mut PgConnection;
                }
            }
            if exec_conn.is_null() || (&*exec_conn).conn.is_null() {
                log_error("STEP WRITE: NULL connection after retry - giving up");
                stmt.write_executed = 1;
                _stmt_guard = None; // unlock
                if !pg_conn_error_out.is_null() {
                    *pg_conn_error_out = 1;
                }
                return STEP_RESULT_ERROR;
            }
            log_error(&format!(
                "STEP WRITE: reconnect retry succeeded (exec_conn={:p})",
                exec_conn
            ));
        }

        crate::pg_client::rust_pool_touch_connection(exec_conn as *const c_void);
        let ec = &mut *exec_conn;
        let mut conn_guard = PthreadMutexGuard::lock(&mut ec.mutex as *mut _);

        if ec.conn.is_null() {
            log_error("STEP WRITE: conn became NULL after lock (TOCTOU race)");
            conn_guard.unlock();
            stmt.write_executed = 1;
            _stmt_guard = None; // unlock
            if !pg_conn_error_out.is_null() {
                *pg_conn_error_out = 1;
            }
            return STEP_RESULT_ERROR;
        }

        if ec.streaming_active.load(Ordering::SeqCst) != 0 {
            let alt_db_path = owned_db_path(exec_conn);
            log_info_lazy!(
                "STEP WRITE: conn {:p} became streaming_active after lock, getting new connection",
                exec_conn
            );
            conn_guard.unlock();
            let Some(alt_db_path) = alt_db_path else {
                log_error("STEP WRITE: live db_path unavailable for alternate connection");
                stmt.write_executed = 1;
                _stmt_guard = None; // unlock
                if !pg_conn_error_out.is_null() {
                    *pg_conn_error_out = 1;
                }
                return STEP_RESULT_ERROR;
            };
            let alt_conn = crate::pg_client::rust_pool_get_connection_excluding(
                alt_db_path.as_ptr(),
                exec_conn as *const c_void,
            ) as *mut PgConnection;
            if !alt_conn.is_null()
                && !(&*alt_conn).conn.is_null()
                && alt_conn != exec_conn
                && (&*alt_conn).streaming_active.load(Ordering::SeqCst) == 0
            {
                exec_conn = alt_conn;
                crate::pg_client::rust_pool_touch_connection(exec_conn as *const c_void);
                let ec2 = &mut *exec_conn;
                conn_guard = PthreadMutexGuard::lock(&mut ec2.mutex as *mut _);
                if ec2.conn.is_null() || ec2.streaming_active.load(Ordering::SeqCst) != 0 {
                    log_error("STEP WRITE: alt conn also unavailable");
                    conn_guard.unlock();
                    stmt.write_executed = 1;
                    _stmt_guard = None; // unlock
                    if !pg_conn_error_out.is_null() {
                        *pg_conn_error_out = 1;
                    }
                    return STEP_RESULT_ERROR;
                }
            } else {
                log_error(&format!(
                    "STEP WRITE: no non-streaming connection available (db_path={} exec_conn={:p} alt_conn={:p})",
                    alt_db_path.to_string_lossy(),
                    exec_conn,
                    alt_conn,
                ));
                stmt.write_executed = 1;
                _stmt_guard = None; // unlock
                if !pg_conn_error_out.is_null() {
                    *pg_conn_error_out = 1;
                }
                return STEP_RESULT_ERROR;
            }
        }

        stmt.conn = exec_conn;

        // Rebind after possible reassignment of exec_conn
        let ec = &mut *exec_conn;
        let write_conn_status = crate::libpq_helpers::rust_pq_status(ec.conn);
        if write_conn_status != CONNECTION_OK {
            let pg_err = crate::libpq_helpers::rust_pq_error_message(ec.conn);
            log_error("=== CONNECTION_BAD DIAGNOSTIC (WRITE) ===");
            log_error(&format!(
                "  Status: {}, Thread: {:p}",
                write_conn_status,
                libc::pthread_self() as *mut c_void
            ));
            log_error(&format!(
                "  Connection: {:p}, PGconn: {:p}",
                exec_conn, ec.conn
            ));
            log_error(&format!(
                "  PG Error: {}",
                cstr_to_string_or(pg_err, "(null)")
            ));
            log_error(&format!("  SQL: {}", cstr_prefix(stmt.sql, 100, "(null)")));
            platform_print_backtrace(
                b"CONNECTION_BAD in STEP WRITE\0".as_ptr() as *const c_char,
                1,
            );
            log_error("=== END DIAGNOSTIC ===");
            log_error("STEP WRITE: Attempting PQreset...");
            crate::libpq_helpers::rust_pq_reset(ec.conn);
            if crate::libpq_helpers::rust_pq_status(ec.conn) != CONNECTION_OK {
                log_error("STEP WRITE: PQreset failed, trying fresh PQconnectdb...");
                crate::pg_client::rust_stmt_cache_clear(exec_conn as *mut c_void);
                crate::libpq_helpers::rust_pq_finish(ec.conn);
                ec.conn = std::ptr::null_mut();

                let cfg = pg_config_get();
                if cfg.is_null() {
                    log_error("STEP WRITE: pg_config_get returned NULL");
                    ec.is_pg_active = 0;
                    conn_guard.unlock();
                    stmt.write_executed = 1;
                    _stmt_guard = None; // unlock
                    if !pg_conn_error_out.is_null() {
                        *pg_conn_error_out = 1;
                    }
                    return STEP_RESULT_ERROR;
                }

                let new_write_conn = connect_new(&*cfg);
                if crate::libpq_helpers::rust_pq_status(new_write_conn) == CONNECTION_OK {
                    ec.conn = new_write_conn;
                    ec.is_pg_active = 1;
                    log_info("STEP WRITE: fresh connection succeeded (reconnected)");
                    apply_pg_session_settings(ec.conn, &*cfg);
                } else {
                    let reset_err = crate::libpq_helpers::rust_pq_error_message(new_write_conn);
                    log_error(&format!(
                        "STEP WRITE: fresh connection also failed: {}",
                        cstr_to_string_or(reset_err, "(null)")
                    ));
                    crate::libpq_helpers::rust_pq_finish(new_write_conn);
                    ec.is_pg_active = 0;
                    conn_guard.unlock();
                    stmt.write_executed = 1;
                    _stmt_guard = None; // unlock
                    if !pg_conn_error_out.is_null() {
                        *pg_conn_error_out = 1;
                    }
                    return STEP_RESULT_ERROR;
                }
            } else {
                log_error("STEP WRITE: PQreset succeeded, connection recovered");
            }
        }

        let scope = CString::new("STEP WRITE").unwrap();
        crate::db_interpose_conn_utils::rust_step_conn_cancel_and_drain(exec_conn, scope.as_ptr());
        *exec_conn_io = exec_conn;
        STEP_RESULT_DONE
    }
}
