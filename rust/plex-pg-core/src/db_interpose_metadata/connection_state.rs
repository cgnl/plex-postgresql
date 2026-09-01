use super::*;
use crate::log_debug_lazy;

pub(super) fn changes_impl(db: *mut sqlite3) -> c_int {
    let _guard = match InterposeGuard::try_enter() {
        Some(g) => g,
        None => return 0,
    };

    let pg_conn = crate::pg_client::rust_pg_find_connection(db);
    if pg_conn.is_null() {
        if let Some(f) = get_orig_sqlite3_changes() {
            return unsafe { f(db) };
        }
        return 0;
    }

    let mut result = 0;
    let conn = unsafe { &*pg_conn };
    if conn.is_pg_active != 0 {
        result = conn.last_changes;
    }
    result
}

pub(super) fn changes64_impl(db: *mut sqlite3) -> i64 {
    let _guard = match InterposeGuard::try_enter() {
        Some(g) => g,
        None => return 0,
    };

    let pg_conn = crate::pg_client::rust_pg_find_connection(db);
    if pg_conn.is_null() {
        if let Some(f) = get_orig_sqlite3_changes64() {
            return unsafe { f(db) };
        }
        return 0;
    }

    let mut result: i64 = 0;
    let conn = unsafe { &*pg_conn };
    if conn.is_pg_active != 0 {
        result = conn.last_changes as i64;
    }
    result
}

pub(super) fn last_insert_rowid_impl(db: *mut sqlite3) -> i64 {
    if unsafe { *tls_in_interpose_call_ptr() } != 0 {
        log_debug("last_insert_rowid: RECURSION DETECTED, returning 0");
        return 0;
    }
    let _guard = match InterposeGuard::try_enter() {
        Some(g) => g,
        None => return 0,
    };

    let pg_conn = crate::pg_client::rust_pg_find_connection(db);
    if pg_conn.is_null() {
        if let Some(f) = get_orig_sqlite3_last_insert_rowid() {
            let rowid = unsafe { f(db) };
            log_debug_lazy!(
                "last_insert_rowid: CALLED db={:p} pg_conn=NULL (no exact match, SQLite returned {})",
                db,
                rowid
            );
            return rowid;
        }
        let global_rowid = crate::pg_client::rust_get_global_last_insert_rowid();
        log_debug_lazy!(
            "last_insert_rowid: CALLED db={:p} pg_conn=NULL (no exact match, no SQLite fn, global={})",
            db,
            global_rowid
        );
        return if global_rowid > 0 { global_rowid } else { 0 };
    }

    log_debug_lazy!(
        "last_insert_rowid: CALLED db={:p} pg_conn={:p} (exact match)",
        db,
        pg_conn
    );

    {
        let pg = unsafe { &*pg_conn };
        if pg.last_insert_rowid > 0 {
            let rowid = pg.last_insert_rowid;
            log_debug_lazy!("last_insert_rowid: using cached connection rowid={}", rowid);
            return rowid;
        }
    }

    let global_rowid = crate::pg_client::rust_get_global_last_insert_rowid();
    if global_rowid > 0 {
        log_debug_lazy!(
            "last_insert_rowid: using cached global rowid={}",
            global_rowid
        );
        return global_rowid;
    }

    let mut result: i64 = 0;
    let pg = unsafe { &mut *pg_conn };
    unsafe {
        if !pg_conn.is_null() && pg.is_pg_active != 0 && !pg.conn.is_null() {
            let mut conn_guard = PthreadMutexGuard::lock(&mut pg.mutex as *mut _);
            log_debug_lazy!(
                "last_insert_rowid: EXECUTING lastval() on conn {:p}",
                pg.conn
            );
            let res = crate::libpq_helpers::rust_pq_exec(
                pg.conn,
                b"SELECT lastval()\0".as_ptr() as *const c_char,
            );
            if res.is_null() {
                conn_guard.unlock();
                log_debug("last_insert_rowid: NULL result, RETURNING 0");
                return 0;
            }

            let status = crate::libpq_helpers::rust_pq_result_status(res);
            log_debug_lazy!(
                "last_insert_rowid: STATUS={} TUPLES={}",
                status,
                crate::libpq_helpers::rust_pq_ntuples(res)
            );
            if status == PGRES_TUPLES_OK && crate::libpq_helpers::rust_pq_ntuples(res) > 0 {
                let mut val_buf = [0 as c_char; 64];
                let mut val_str: *const c_char = b"0\0".as_ptr() as *const c_char;
                if crate::db_interpose_helpers::rust_pg_result_text_copy(
                    res as *const crate::db_interpose_helpers::PGresult,
                    0,
                    0,
                    val_buf.as_mut_ptr(),
                    val_buf.len(),
                ) >= 0
                {
                    val_str = val_buf.as_ptr();
                }
                let rowid = crate::db_interpose_helpers::rust_pg_text_to_int64(val_str);
                log_debug_lazy!(
                    "last_insert_rowid: GOT VALUE={} rowid={}",
                    cstr_to_string_or(val_str, "0"),
                    rowid
                );
                crate::libpq_helpers::rust_pq_clear(res);
                conn_guard.unlock();
                if rowid > 0 {
                    log_debug_lazy!("last_insert_rowid: RETURNING rowid={}", rowid);
                    result = rowid;
                } else {
                    log_debug("last_insert_rowid: rowid <= 0, RETURNING 0");
                }
            } else {
                if status == PGRES_FATAL_ERROR {
                    let err = crate::libpq_helpers::rust_pq_error_message(pg.conn);
                    log_debug_lazy!(
                        "last_insert_rowid: FATAL_ERROR: {}",
                        cstr_to_string_or(err, "(null)")
                    );
                } else {
                    log_debug_lazy!("last_insert_rowid: NON-TUPLES status={}", status);
                }
                crate::libpq_helpers::rust_pq_clear(res);
                conn_guard.unlock();
                log_debug("last_insert_rowid: RETURNING 0 due to error");
            }
        } else {
            log_debug("last_insert_rowid: NO PG_CONN or not active, RETURNING 0");
        }
    }

    log_debug_lazy!("last_insert_rowid: FINAL result={}", result);
    result
}

pub(super) fn errmsg_impl(db: *mut sqlite3) -> *const c_char {
    log_debug_lazy!("ERRMSG: db={:p}", db);
    unsafe {
        if *tls_in_interpose_call_ptr() != 0 {
            if let Some(f) = get_shim_sqlite3_errmsg() {
                return f(db);
            }
        }
    }

    let pg_conn = crate::pg_client::rust_pg_find_connection(db);
    if !pg_conn.is_null() {
        let conn = unsafe { &*pg_conn };
        if conn.last_error_code != SQLITE_OK && conn.last_error[0] != 0 {
            log_debug_lazy!(
                "ERRMSG: returning tracked error='{}'",
                cstr_to_string_or(conn.last_error.as_ptr(), "")
            );
            return conn.last_error.as_ptr();
        }
        log_debug("ERRMSG: returning 'not an error'");
        return NOT_AN_ERROR.as_ptr() as *const c_char;
    }

    if let Some(f) = get_shim_sqlite3_errmsg() {
        return unsafe { f(db) };
    }
    if let Some(f) = get_orig_sqlite3_errmsg() {
        return unsafe { f(db) };
    }
    b"unknown error\0".as_ptr() as *const c_char
}

pub(super) fn errcode_impl(db: *mut sqlite3) -> c_int {
    log_debug_lazy!("ERRCODE: db={:p}", db);
    unsafe {
        if *tls_in_interpose_call_ptr() != 0 {
            if let Some(f) = get_shim_sqlite3_errcode() {
                return f(db);
            }
        }
    }

    let pg_conn = crate::pg_client::rust_pg_find_connection(db);
    if !pg_conn.is_null() {
        let conn = unsafe { &*pg_conn };
        log_debug_lazy!(
            "ERRCODE: pg_conn found, returning code={}",
            conn.last_error_code
        );
        return conn.last_error_code;
    }

    if let Some(f) = get_shim_sqlite3_errcode() {
        return unsafe { f(db) };
    }
    if let Some(f) = get_orig_sqlite3_errcode() {
        return unsafe { f(db) };
    }
    SQLITE_ERROR
}

pub(super) fn extended_errcode_impl(db: *mut sqlite3) -> c_int {
    let pg_conn = crate::pg_client::rust_pg_find_connection(db);
    if !pg_conn.is_null() {
        let conn = unsafe { &*pg_conn };
        return conn.last_error_code;
    }
    if let Some(f) = get_orig_sqlite3_extended_errcode() {
        return unsafe { f(db) };
    }
    SQLITE_ERROR
}

pub(super) fn get_table_impl(
    db: *mut sqlite3,
    sql: *const c_char,
    paz_result: *mut *mut *mut c_char,
    pn_row: *mut c_int,
    pn_column: *mut c_int,
    pz_err_msg: *mut *mut c_char,
) -> c_int {
    if sql.is_null() {
        return match get_orig_sqlite3_get_table() {
            Some(f) => unsafe { f(db, sql, paz_result, pn_row, pn_column, pz_err_msg) },
            None => SQLITE_ERROR,
        };
    }

    let pg_conn = crate::pg_client::rust_pg_find_connection(db);
    unsafe {
        if !pg_conn.is_null() {
            let pg = &mut *pg_conn;
            if pg.is_pg_active != 0
                && !pg.conn.is_null()
                && crate::pg_config::pg_config_is_read_operation(sql) != 0
            {
                let mut trans = sql_translate(sql);
                if trans.success != 0 && !trans.sql.is_null() {
                    let mut conn_guard = PthreadMutexGuard::lock(&mut pg.mutex as *mut _);
                    let res = crate::libpq_helpers::rust_pq_exec(pg.conn, trans.sql);
                    if crate::libpq_helpers::rust_pq_result_status(res) == PGRES_TUPLES_OK {
                        let mut result: *mut *mut c_char = std::ptr::null_mut();
                        let mut nrows = 0;
                        let mut ncols = 0;
                        if crate::db_interpose_helpers::rust_get_table_from_pgresult(
                            res as *const crate::db_interpose_helpers::PGresult,
                            &mut result,
                            &mut nrows,
                            &mut ncols,
                        ) != 0
                        {
                            if !paz_result.is_null() {
                                *paz_result = result;
                            }
                            if !pn_row.is_null() {
                                *pn_row = nrows;
                            }
                            if !pn_column.is_null() {
                                *pn_column = ncols;
                            }
                            if !pz_err_msg.is_null() {
                                *pz_err_msg = std::ptr::null_mut();
                            }
                            crate::libpq_helpers::rust_pq_clear(res);
                            conn_guard.unlock();
                            sql_translation_free(&mut trans as *mut SqlTranslation);
                            return SQLITE_OK;
                        }
                    }
                    crate::libpq_helpers::rust_pq_clear(res);
                    conn_guard.unlock();
                }
                sql_translation_free(&mut trans as *mut SqlTranslation);
            }
        } // if !pg_conn.is_null()
    }

    match get_orig_sqlite3_get_table() {
        Some(f) => unsafe { f(db, sql, paz_result, pn_row, pn_column, pz_err_msg) },
        None => SQLITE_ERROR,
    }
}
