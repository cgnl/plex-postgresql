use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};

use crate::db_interpose_conn_utils::log_error;
use crate::db_interpose_helpers::cstr_to_str_or_empty;
use crate::ffi_types::PgConnection;
use crate::libpq_helpers::{
    rust_pq_clear, rust_pq_exec, rust_pq_result_error_message, rust_pq_result_status,
    rust_pq_socket, rust_pq_transaction_status, PGconn,
};
use crate::log_debug_lazy;

const PG_SOCKET_TIMEOUT_SEC: i64 = 60;
pub(super) const CONNECTION_OK: i32 = 0;
const PGRES_COMMAND_OK: i32 = 1;
const PGRES_TUPLES_OK: i32 = 2;
pub(super) const PQTRANS_INTRANS: i32 = 2;
pub(super) const PQTRANS_INERROR: i32 = 3;

pub(super) fn pg_set_socket_timeout(pg_conn: *mut PGconn) {
    if pg_conn.is_null() {
        return;
    }
    let sock = rust_pq_socket(pg_conn);
    if sock < 0 {
        log_error("pg_set_socket_timeout: invalid socket");
        return;
    }

    let tv = libc::timeval {
        tv_sec: PG_SOCKET_TIMEOUT_SEC,
        tv_usec: 0,
    };
    unsafe {
        if libc::setsockopt(
            sock,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &tv as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        ) < 0
        {
            log_error("pg_set_socket_timeout: failed to set SO_RCVTIMEO");
        }
        if libc::setsockopt(
            sock,
            libc::SOL_SOCKET,
            libc::SO_SNDTIMEO,
            &tv as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        ) < 0
        {
            log_error("pg_set_socket_timeout: failed to set SO_SNDTIMEO");
        }
    }

    log_debug_lazy!(
        "Socket timeout set to {} seconds for socket {}",
        PG_SOCKET_TIMEOUT_SEC,
        sock
    );
}

pub(super) fn exec_command(pg_conn: *mut PGconn, sql: &str) -> bool {
    if pg_conn.is_null() {
        return false;
    }
    let cs = match CString::new(sql) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let res = rust_pq_exec(pg_conn, cs.as_ptr());
    let ok = !res.is_null() && rust_pq_result_status(res) == PGRES_COMMAND_OK;
    if !res.is_null() {
        rust_pq_clear(res);
    }
    ok
}

pub(super) fn exec_tuples(pg_conn: *mut PGconn, sql: &str) -> bool {
    if pg_conn.is_null() {
        return false;
    }
    let cs = match CString::new(sql) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let res = rust_pq_exec(pg_conn, cs.as_ptr());
    let ok = !res.is_null() && rust_pq_result_status(res) == PGRES_TUPLES_OK;
    if !res.is_null() {
        rust_pq_clear(res);
    }
    ok
}

pub(super) fn apply_session_settings(pg_conn: *mut PGconn, schema: &str, deallocate_all: bool) {
    if pg_conn.is_null() {
        return;
    }

    let schema_cmd = format!("SET search_path TO {}, public", schema);
    let res = match CString::new(schema_cmd) {
        Ok(s) => rust_pq_exec(pg_conn, s.as_ptr()),
        Err(_) => std::ptr::null_mut(),
    };
    if res.is_null() || rust_pq_result_status(res) != PGRES_COMMAND_OK {
        let err = if res.is_null() {
            "<null result>".to_string()
        } else {
            unsafe {
                let msg = rust_pq_result_error_message(res);
                if msg.is_null() {
                    "<null>".to_string()
                } else {
                    CStr::from_ptr(msg).to_string_lossy().into_owned()
                }
            }
        };
        log_error(&format!("Failed to set search_path: {}", err));
    }
    if !res.is_null() {
        rust_pq_clear(res);
    }

    if deallocate_all {
        let _ = exec_command(pg_conn, "DEALLOCATE ALL");
    }

    // One session-wide timeout, set once per connection. Reads used to re-send
    // this before every query (an extra round trip each, issue #59), which in
    // effect left every connection at this value after its first read anyway.
    if !exec_command(pg_conn, crate::db_interpose_conn_utils::STATEMENT_TIMEOUT_SQL) {
        log_error("Failed to set statement_timeout");
    }
}

pub(super) fn exec_simple(raw_conn: *mut c_void, sql: *const c_char) -> bool {
    let conn_ptr = raw_conn as *mut PgConnection;
    if conn_ptr.is_null() || sql.is_null() {
        return false;
    }
    // SAFETY: conn_ptr is non-null after the check above.
    let conn = unsafe { &*conn_ptr };
    let s = unsafe { cstr_to_str_or_empty(sql) };
    let trimmed = s.trim_start();
    let lower = trimmed.to_ascii_lowercase();

    if lower.starts_with("commit") || lower.starts_with("rollback") || lower.starts_with("end") {
        let txn = if conn.conn.is_null() {
            0
        } else {
            rust_pq_transaction_status(conn.conn)
        };
        if txn != PQTRANS_INTRANS && txn != PQTRANS_INERROR {
            log_debug_lazy!(
                "exec_simple: skipped {} in non-transaction state={}",
                trimmed,
                txn
            );
            return true;
        }
    }

    let cmd = match CString::new(trimmed) {
        Ok(s) => s,
        Err(_) => return false,
    };
    if conn.conn.is_null() {
        return false;
    }
    let res = rust_pq_exec(conn.conn, cmd.as_ptr());
    let ok = !res.is_null() && rust_pq_result_status(res) == PGRES_COMMAND_OK;
    if !res.is_null() {
        rust_pq_clear(res);
    }
    ok
}
