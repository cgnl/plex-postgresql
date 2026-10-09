use super::*;
use crate::log_debug_lazy;

fn handle_state(db: *mut sqlite3) -> *mut crate::ffi_types::PgConnection {
    if crate::db_interpose_common::SHIM_PASSTHROUGH_ONLY.load(std::sync::atomic::Ordering::Acquire)
        != 0
        || std::env::var("PLEX_PG_FORCE_SQLITE_LIBRARY")
            .is_ok_and(|value| !value.is_empty() && value != "0")
    {
        return std::ptr::null_mut();
    }
    crate::pg_client::rust_pg_find_handle_connection(db)
}

pub(super) fn changes_impl(db: *mut sqlite3) -> c_int {
    let _guard = match InterposeGuard::try_enter() {
        Some(g) => g,
        None => return 0,
    };

    let pg_conn = handle_state(db);
    if pg_conn.is_null() {
        return crate::db_interpose_common::get_orig_sqlite3_changes()
            .map_or(0, |function| unsafe { function(db) });
    }
    let mut result = 0;
    if !pg_conn.is_null() {
        let conn = unsafe { &*pg_conn };
        if conn.is_pg_active != 0 {
            result = conn.last_changes;
        }
    }
    result
}

pub(super) fn changes64_impl(db: *mut sqlite3) -> i64 {
    let _guard = match InterposeGuard::try_enter() {
        Some(g) => g,
        None => return 0,
    };

    let pg_conn = handle_state(db);
    if pg_conn.is_null() {
        return crate::db_interpose_common::get_orig_sqlite3_changes64()
            .map_or(0, |function| unsafe { function(db) });
    }
    let mut result: i64 = 0;
    if !pg_conn.is_null() {
        let conn = unsafe { &*pg_conn };
        if conn.is_pg_active != 0 {
            result = conn.last_changes as i64;
        }
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

    let pg_conn = handle_state(db);
    if pg_conn.is_null() {
        return crate::db_interpose_common::get_orig_sqlite3_last_insert_rowid()
            .map_or(0, |function| unsafe { function(db) });
    }
    unsafe { (*pg_conn).last_insert_rowid }
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

    let pg_conn = handle_state(db);
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

    let pg_conn = handle_state(db);
    if !pg_conn.is_null() {
        let conn = unsafe { &*pg_conn };
        log_debug_lazy!(
            "ERRCODE: pg_conn found, returning code={}",
            conn.last_error_code
        );
        return conn.last_error_code & 255;
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
    let pg_conn = handle_state(db);
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

    let sql_str = unsafe { CStr::from_ptr(sql).to_str().unwrap_or("") };
    if crate::db_interpose_common::SHIM_PASSTHROUGH_ONLY.load(std::sync::atomic::Ordering::Acquire)
        != 0
        || std::env::var("PLEX_PG_FORCE_SQLITE_LIBRARY")
            .is_ok_and(|value| !value.is_empty() && value != "0")
        || crate::pg_config::is_sqlite_passthrough_str(sql_str)
    {
        return match get_orig_sqlite3_get_table() {
            Some(function) => unsafe {
                function(db, sql, paz_result, pn_row, pn_column, pz_err_msg)
            },
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
                            crate::db_interpose_exec::pg_path::clear_pg_last_error(pg_conn);
                            crate::db_interpose_exec::pg_path::copy_pg_outcome(
                                handle_state(db),
                                pg_conn,
                            );
                            crate::libpq_helpers::rust_pq_clear(res);
                            conn_guard.unlock();
                            sql_translation_free(&mut trans as *mut SqlTranslation);
                            return SQLITE_OK;
                        }
                        crate::db_interpose_exec::pg_path::set_pg_last_error(
                            pg_conn,
                            "Could not allocate PostgreSQL table result",
                        );
                    } else {
                        crate::db_interpose_exec::pg_path::record_pg_result_error(
                            pg_conn, pg_conn, res,
                        );
                    }
                    crate::libpq_helpers::rust_pq_clear(res);
                    conn_guard.unlock();
                } else {
                    crate::db_interpose_exec::pg_path::set_pg_last_error(
                        pg_conn,
                        &format!(
                            "PostgreSQL table query translation failed: {}",
                            cstr_to_string_or(trans.error.as_ptr(), "translation failed")
                        ),
                    );
                }
                sql_translation_free(&mut trans as *mut SqlTranslation);
                crate::db_interpose_exec::pg_path::copy_pg_outcome(handle_state(db), pg_conn);
                if !pz_err_msg.is_null() {
                    *pz_err_msg = crate::db_interpose_exec::exec_error_message(&cstr_to_string_or(
                        pg.last_error.as_ptr(),
                        "PostgreSQL table query failed",
                    ));
                }
                return SQLITE_ERROR;
            }
        } // if !pg_conn.is_null()
    }

    let filename = crate::db_interpose_open::lookup_db_handle_filename(db);
    if filename
        .as_ref()
        .is_some_and(|path| crate::pg_config::pg_config_should_redirect(path.as_ptr(), 0) != 0)
    {
        let message = "PostgreSQL get_table connection unavailable or operation unsupported; refusing shadow SQLite execution";
        unsafe {
            crate::db_interpose_exec::pg_path::set_pg_last_error(handle_state(db), message);
            if !pz_err_msg.is_null() {
                *pz_err_msg = crate::db_interpose_exec::exec_error_message(message);
            }
        }
        return SQLITE_ERROR;
    }

    match get_orig_sqlite3_get_table() {
        Some(f) => unsafe { f(db, sql, paz_result, pn_row, pn_column, pz_err_msg) },
        None => SQLITE_ERROR,
    }
}
