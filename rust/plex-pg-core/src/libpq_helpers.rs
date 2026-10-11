use crate::log_info_lazy;
use std::os::raw::{c_char, c_int};

#[repr(C)]
pub struct PGconn {
    _private: [u8; 0],
}

#[repr(C)]
pub struct PGresult {
    _private: [u8; 0],
}

#[repr(C)]
pub struct PGcancel {
    _private: [u8; 0],
}

pub type Oid = u32;

pub const PGRES_TUPLES_OK: c_int = 2;

extern "C" {
    fn PQsetnonblocking(conn: *mut PGconn, arg: c_int) -> c_int;
    fn PQisBusy(conn: *mut PGconn) -> c_int;
    fn PQconsumeInput(conn: *mut PGconn) -> c_int;
    fn PQgetCancel(conn: *mut PGconn) -> *mut PGcancel;
    fn PQcancel(cancel: *mut PGcancel, errbuf: *mut c_char, errbuf_len: c_int) -> c_int;
    fn PQfreeCancel(cancel: *mut PGcancel);
    fn PQgetResult(conn: *mut PGconn) -> *mut PGresult;
    fn PQresultStatus(res: *const PGresult) -> c_int;
    fn PQresStatus(status: c_int) -> *const c_char;
    fn PQclear(res: *mut PGresult);
    fn PQerrorMessage(conn: *mut PGconn) -> *const c_char;
    fn PQstatus(conn: *mut PGconn) -> c_int;
    fn PQreset(conn: *mut PGconn);
    fn PQconnectdb(conninfo: *const c_char) -> *mut PGconn;
    fn PQfinish(conn: *mut PGconn);
    fn PQexec(conn: *mut PGconn, command: *const c_char) -> *mut PGresult;
    fn PQprepare(
        conn: *mut PGconn,
        stmt: *const c_char,
        query: *const c_char,
        n_params: c_int,
        param_types: *const Oid,
    ) -> *mut PGresult;
    fn PQexecPrepared(
        conn: *mut PGconn,
        stmt: *const c_char,
        n_params: c_int,
        param_values: *const *const c_char,
        param_lengths: *const c_int,
        param_formats: *const c_int,
        result_format: c_int,
    ) -> *mut PGresult;
    fn PQexecParams(
        conn: *mut PGconn,
        command: *const c_char,
        n_params: c_int,
        param_types: *const Oid,
        param_values: *const *const c_char,
        param_lengths: *const c_int,
        param_formats: *const c_int,
        result_format: c_int,
    ) -> *mut PGresult;
    fn PQsendQueryPrepared(
        conn: *mut PGconn,
        stmt: *const c_char,
        n_params: c_int,
        param_values: *const *const c_char,
        param_lengths: *const c_int,
        param_formats: *const c_int,
        result_format: c_int,
    ) -> c_int;
    fn PQsendQueryParams(
        conn: *mut PGconn,
        command: *const c_char,
        n_params: c_int,
        param_types: *const Oid,
        param_values: *const *const c_char,
        param_lengths: *const c_int,
        param_formats: *const c_int,
        result_format: c_int,
    ) -> c_int;
    fn PQsetSingleRowMode(conn: *mut PGconn) -> c_int;
    fn PQnfields(res: *const PGresult) -> c_int;
    fn PQntuples(res: *const PGresult) -> c_int;
    fn PQgetisnull(res: *const PGresult, row: c_int, col: c_int) -> c_int;
    fn PQgetvalue(res: *const PGresult, row: c_int, col: c_int) -> *const c_char;
    fn PQgetlength(res: *const PGresult, row: c_int, col: c_int) -> c_int;
    fn PQcmdTuples(res: *const PGresult) -> *const c_char;
    fn PQresultErrorMessage(res: *const PGresult) -> *const c_char;
    fn PQresultErrorField(res: *const PGresult, fieldcode: c_int) -> *const c_char;
    fn PQtransactionStatus(conn: *mut PGconn) -> c_int;
    fn PQsocket(conn: *mut PGconn) -> c_int;
    fn PQdescribePrepared(conn: *mut PGconn, stmt: *const c_char) -> *mut PGresult;
}

#[no_mangle]
pub extern "C" fn rust_pq_set_nonblocking(conn: *mut PGconn, arg: c_int) -> c_int {
    if conn.is_null() {
        return -1;
    }
    unsafe { PQsetnonblocking(conn, arg) }
}

#[no_mangle]
pub extern "C" fn rust_pq_is_busy(conn: *mut PGconn) -> c_int {
    if conn.is_null() {
        return 0;
    }
    unsafe { PQisBusy(conn) }
}

#[no_mangle]
pub extern "C" fn rust_pq_consume_input(conn: *mut PGconn) -> c_int {
    if conn.is_null() {
        return 0;
    }
    unsafe { PQconsumeInput(conn) }
}

#[no_mangle]
pub extern "C" fn rust_pq_get_cancel(conn: *mut PGconn) -> *mut PGcancel {
    if conn.is_null() {
        return std::ptr::null_mut();
    }
    unsafe { PQgetCancel(conn) }
}

#[no_mangle]
pub extern "C" fn rust_pq_cancel(
    cancel: *mut PGcancel,
    errbuf: *mut c_char,
    errbuf_len: c_int,
) -> c_int {
    if cancel.is_null() {
        return 0;
    }
    unsafe { PQcancel(cancel, errbuf, errbuf_len) }
}

#[no_mangle]
pub extern "C" fn rust_pq_free_cancel(cancel: *mut PGcancel) {
    if cancel.is_null() {
        return;
    }
    unsafe { PQfreeCancel(cancel) }
}

#[no_mangle]
pub extern "C" fn rust_pq_get_result(conn: *mut PGconn) -> *mut PGresult {
    if conn.is_null() {
        return std::ptr::null_mut();
    }
    unsafe { PQgetResult(conn) }
}

#[no_mangle]
pub extern "C" fn rust_pq_result_status(res: *const PGresult) -> c_int {
    if res.is_null() {
        return -1;
    }
    unsafe { PQresultStatus(res) }
}

#[no_mangle]
pub extern "C" fn rust_pq_res_status(status: c_int) -> *const c_char {
    unsafe { PQresStatus(status) }
}

#[no_mangle]
pub extern "C" fn rust_pq_clear(res: *mut PGresult) {
    if res.is_null() {
        return;
    }
    unsafe { PQclear(res) }
}

#[no_mangle]
pub extern "C" fn rust_pq_error_message(conn: *mut PGconn) -> *const c_char {
    if conn.is_null() {
        return std::ptr::null();
    }
    unsafe { PQerrorMessage(conn) }
}

#[no_mangle]
pub extern "C" fn rust_pq_status(conn: *mut PGconn) -> c_int {
    if conn.is_null() {
        return -1;
    }
    unsafe { PQstatus(conn) }
}

#[no_mangle]
pub extern "C" fn rust_pq_reset(conn: *mut PGconn) {
    if conn.is_null() {
        return;
    }
    unsafe { PQreset(conn) }
}

#[no_mangle]
pub extern "C" fn rust_pq_connectdb(conninfo: *const c_char) -> *mut PGconn {
    if conninfo.is_null() {
        return std::ptr::null_mut();
    }
    unsafe { PQconnectdb(conninfo) }
}

#[no_mangle]
pub extern "C" fn rust_pq_finish(conn: *mut PGconn) {
    if conn.is_null() {
        return;
    }
    unsafe { PQfinish(conn) }
}

#[no_mangle]
pub extern "C" fn rust_pq_exec(conn: *mut PGconn, command: *const c_char) -> *mut PGresult {
    if conn.is_null() || command.is_null() {
        return std::ptr::null_mut();
    }
    unsafe { PQexec(conn, command) }
}

#[no_mangle]
pub extern "C" fn rust_pq_prepare(
    conn: *mut PGconn,
    stmt: *const c_char,
    query: *const c_char,
    n_params: c_int,
    param_types: *const Oid,
) -> *mut PGresult {
    if conn.is_null() || stmt.is_null() || query.is_null() {
        return std::ptr::null_mut();
    }
    unsafe { PQprepare(conn, stmt, query, n_params, param_types) }
}

#[no_mangle]
pub extern "C" fn rust_pq_exec_prepared(
    conn: *mut PGconn,
    stmt: *const c_char,
    n_params: c_int,
    param_values: *const *const c_char,
    param_lengths: *const c_int,
    param_formats: *const c_int,
    result_format: c_int,
) -> *mut PGresult {
    if conn.is_null() || stmt.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        PQexecPrepared(
            conn,
            stmt,
            n_params,
            param_values,
            param_lengths,
            param_formats,
            result_format,
        )
    }
}

#[no_mangle]
pub extern "C" fn rust_pq_exec_params(
    conn: *mut PGconn,
    command: *const c_char,
    n_params: c_int,
    param_types: *const Oid,
    param_values: *const *const c_char,
    param_lengths: *const c_int,
    param_formats: *const c_int,
    result_format: c_int,
) -> *mut PGresult {
    if conn.is_null() || command.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        PQexecParams(
            conn,
            command,
            n_params,
            param_types,
            param_values,
            param_lengths,
            param_formats,
            result_format,
        )
    }
}

#[no_mangle]
pub extern "C" fn rust_pq_send_query_prepared(
    conn: *mut PGconn,
    stmt: *const c_char,
    n_params: c_int,
    param_values: *const *const c_char,
    param_lengths: *const c_int,
    param_formats: *const c_int,
    result_format: c_int,
) -> c_int {
    if conn.is_null() || stmt.is_null() {
        return 0;
    }
    unsafe {
        PQsendQueryPrepared(
            conn,
            stmt,
            n_params,
            param_values,
            param_lengths,
            param_formats,
            result_format,
        )
    }
}

#[no_mangle]
pub extern "C" fn rust_pq_send_query_params(
    conn: *mut PGconn,
    command: *const c_char,
    n_params: c_int,
    param_types: *const Oid,
    param_values: *const *const c_char,
    param_lengths: *const c_int,
    param_formats: *const c_int,
    result_format: c_int,
) -> c_int {
    if conn.is_null() || command.is_null() {
        return 0;
    }
    unsafe {
        PQsendQueryParams(
            conn,
            command,
            n_params,
            param_types,
            param_values,
            param_lengths,
            param_formats,
            result_format,
        )
    }
}

#[no_mangle]
pub extern "C" fn rust_pq_set_single_row_mode(conn: *mut PGconn) -> c_int {
    if conn.is_null() {
        return 0;
    }
    unsafe { PQsetSingleRowMode(conn) }
}

#[no_mangle]
pub extern "C" fn rust_pq_nfields(res: *const PGresult) -> c_int {
    if res.is_null() {
        return 0;
    }
    unsafe { PQnfields(res) }
}

#[no_mangle]
pub extern "C" fn rust_pq_ntuples(res: *const PGresult) -> c_int {
    if res.is_null() {
        return 0;
    }
    unsafe { PQntuples(res) }
}

#[no_mangle]
pub extern "C" fn rust_pq_cmd_tuples(res: *const PGresult) -> *const c_char {
    if res.is_null() {
        return std::ptr::null();
    }
    unsafe { PQcmdTuples(res) }
}

#[no_mangle]
pub extern "C" fn rust_pq_result_error_message(res: *const PGresult) -> *const c_char {
    if res.is_null() {
        return std::ptr::null();
    }
    unsafe { PQresultErrorMessage(res) }
}

#[no_mangle]
pub extern "C" fn rust_pq_result_error_field(
    res: *const PGresult,
    fieldcode: c_int,
) -> *const c_char {
    if res.is_null() {
        return std::ptr::null();
    }
    unsafe { PQresultErrorField(res, fieldcode) }
}

#[no_mangle]
pub extern "C" fn rust_pq_transaction_status(conn: *mut PGconn) -> c_int {
    if conn.is_null() {
        return -1;
    }
    unsafe { PQtransactionStatus(conn) }
}

#[no_mangle]
pub extern "C" fn rust_pq_socket(conn: *mut PGconn) -> c_int {
    if conn.is_null() {
        return -1;
    }
    unsafe { PQsocket(conn) }
}

#[no_mangle]
pub extern "C" fn rust_pq_describe_prepared(
    conn: *mut PGconn,
    stmt: *const c_char,
) -> *mut PGresult {
    if conn.is_null() || stmt.is_null() {
        return std::ptr::null_mut();
    }
    unsafe { PQdescribePrepared(conn, stmt) }
}

#[inline]
unsafe fn cstr_to_str<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    std::ffi::CStr::from_ptr(ptr).to_str().ok()
}

#[inline]
unsafe fn read_pg_int(res: *const PGresult, row: c_int, col: c_int) -> Option<i32> {
    if res.is_null() {
        return None;
    }
    if PQgetisnull(res, row, col) != 0 {
        return None;
    }
    let ptr = PQgetvalue(res, row, col);
    let len = PQgetlength(res, row, col);
    if ptr.is_null() || len <= 0 {
        return None;
    }
    let s = std::slice::from_raw_parts(ptr.cast::<u8>(), len as usize);
    std::str::from_utf8(s).ok()?.trim().parse::<i32>().ok()
}

#[no_mangle]
pub extern "C" fn rust_parse_positive_env_or_default(
    name: *const c_char,
    default_value: c_int,
) -> c_int {
    if name.is_null() {
        return default_value;
    }
    let key = unsafe { std::ffi::CStr::from_ptr(name) };
    if key.to_str().is_ok() {
        unsafe {
            let val = libc::getenv(key.as_ptr());
            if val.is_null() {
                return default_value;
            }
            if let Some(s) = cstr_to_str(val) {
                if let Ok(v) = s.trim().parse::<i32>() {
                    return if v > 0 { v } else { default_value };
                }
            }
        }
    }
    default_value
}

#[no_mangle]
pub extern "C" fn rust_pg_probe_max_connections(
    host: *const c_char,
    port: c_int,
    db: *const c_char,
    user: *const c_char,
    password: *const c_char,
) -> c_int {
    if host.is_null() || db.is_null() || user.is_null() || port <= 0 {
        return 0;
    }
    let host_s = unsafe { cstr_to_str(host) }.unwrap_or("");
    let db_s = unsafe { cstr_to_str(db) }.unwrap_or("");
    let user_s = unsafe { cstr_to_str(user) }.unwrap_or("");
    let pass_s = unsafe { cstr_to_str(password) }.unwrap_or("");
    if host_s.is_empty() || db_s.is_empty() || user_s.is_empty() {
        return 0;
    }

    let conninfo = format!(
        "host={} port={} dbname={} user={} password={} connect_timeout=3",
        host_s, port, db_s, user_s, pass_s
    );
    let c_conninfo = match std::ffi::CString::new(conninfo) {
        Ok(s) => s,
        Err(_) => return 0,
    };

    const CONNECTION_OK: c_int = 0;
    let probe = unsafe { PQconnectdb(c_conninfo.as_ptr()) };
    if probe.is_null() || unsafe { PQstatus(probe) } != CONNECTION_OK {
        if !probe.is_null() {
            unsafe { PQfinish(probe) };
        }
        return 0;
    }

    let res = unsafe { PQexec(probe, b"SHOW max_connections\0".as_ptr() as *const c_char) };
    let mut max_connections = 0;
    if !res.is_null() {
        let status = unsafe { PQresultStatus(res) };
        const PGRES_TUPLES_OK: c_int = 2;
        if status == PGRES_TUPLES_OK && unsafe { PQntuples(res) } > 0 {
            if let Some(v) = unsafe { read_pg_int(res, 0, 0) } {
                max_connections = v;
            }
        }
        unsafe { PQclear(res) };
    }
    unsafe { PQfinish(probe) };
    if max_connections > 0 {
        max_connections
    } else {
        0
    }
}

#[no_mangle]
pub extern "C" fn rust_pg_probe_idle_timeouts(
    host: *const c_char,
    port: c_int,
    db: *const c_char,
    user: *const c_char,
    password: *const c_char,
    idle_session_out: *mut c_int,
    idle_in_tx_out: *mut c_int,
) -> c_int {
    if !idle_session_out.is_null() {
        unsafe { *idle_session_out = 0 };
    }
    if !idle_in_tx_out.is_null() {
        unsafe { *idle_in_tx_out = 0 };
    }
    if host.is_null() || db.is_null() || user.is_null() || port <= 0 {
        return 0;
    }
    let host_s = unsafe { cstr_to_str(host) }.unwrap_or("");
    let db_s = unsafe { cstr_to_str(db) }.unwrap_or("");
    let user_s = unsafe { cstr_to_str(user) }.unwrap_or("");
    let pass_s = unsafe { cstr_to_str(password) }.unwrap_or("");
    if host_s.is_empty() || db_s.is_empty() || user_s.is_empty() {
        return 0;
    }

    let conninfo = format!(
        "host={} port={} dbname={} user={} password={} connect_timeout=3",
        host_s, port, db_s, user_s, pass_s
    );
    let c_conninfo = match std::ffi::CString::new(conninfo) {
        Ok(s) => s,
        Err(_) => return 0,
    };

    const CONNECTION_OK: c_int = 0;
    let probe = unsafe { PQconnectdb(c_conninfo.as_ptr()) };
    if probe.is_null() || unsafe { PQstatus(probe) } != CONNECTION_OK {
        if !probe.is_null() {
            unsafe { PQfinish(probe) };
        }
        return 0;
    }

    let query = b"SELECT CASE WHEN current_setting('idle_session_timeout') = '0' THEN 0 \
        ELSE GREATEST(1, CEIL(EXTRACT(EPOCH FROM current_setting('idle_session_timeout')::interval))::int) END, \
        CASE WHEN current_setting('idle_in_transaction_session_timeout') = '0' THEN 0 \
        ELSE GREATEST(1, CEIL(EXTRACT(EPOCH FROM current_setting('idle_in_transaction_session_timeout')::interval))::int) END\0";

    let res = unsafe { PQexec(probe, query.as_ptr() as *const c_char) };
    let mut got_session = false;
    let mut got_in_tx = false;
    if !res.is_null() {
        const PGRES_TUPLES_OK: c_int = 2;
        if unsafe { PQresultStatus(res) } == PGRES_TUPLES_OK
            && unsafe { PQntuples(res) } > 0
            && unsafe { PQnfields(res) } >= 2
        {
            if let Some(v) = unsafe { read_pg_int(res, 0, 0) } {
                if !idle_session_out.is_null() {
                    unsafe { *idle_session_out = v };
                }
                got_session = true;
            }
            if let Some(v) = unsafe { read_pg_int(res, 0, 1) } {
                if !idle_in_tx_out.is_null() {
                    unsafe { *idle_in_tx_out = v };
                }
                got_in_tx = true;
            }
        }
        unsafe { PQclear(res) };
    }
    unsafe { PQfinish(probe) };
    i32::from(got_session && got_in_tx)
}

#[no_mangle]
pub extern "C" fn rust_pg_align_idle_timeout_with_server(
    idle_timeout: c_int,
    host: *const c_char,
    port: c_int,
    db: *const c_char,
    user: *const c_char,
    password: *const c_char,
) -> c_int {
    let mut session_s: c_int = 0;
    let mut in_tx_s: c_int = 0;
    let ok = rust_pg_probe_idle_timeouts(
        host,
        port,
        db,
        user,
        password,
        &mut session_s as *mut c_int,
        &mut in_tx_s as *mut c_int,
    );
    if ok == 0 {
        log_info_lazy!(
            "Pool init: could not read PostgreSQL idle timeout settings; keeping pool idle_timeout={}s",
            idle_timeout
        );
        return idle_timeout;
    }

    let mut cutoff = 0;
    if session_s > 0 {
        cutoff = session_s;
    }
    if in_tx_s > 0 && (cutoff == 0 || in_tx_s < cutoff) {
        cutoff = in_tx_s;
    }
    if cutoff <= 0 {
        return idle_timeout;
    }

    let safety_margin = 10;
    let mut target = cutoff - safety_margin;
    if target < 10 {
        target = 10;
    }

    if idle_timeout >= cutoff {
        log_info_lazy!(
            "Pool idle timeout ({}s) >= PostgreSQL idle cutoff ({}s, session={}s, in_tx={}s); adjusting to {}s",
            idle_timeout, cutoff, session_s, in_tx_s, target
        );
        return target;
    }
    idle_timeout
}
