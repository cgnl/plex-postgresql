use crate::byte_utils::{contains_bytes, contains_icase_bytes, starts_with_icase_bytes};
pub(crate) mod pg_path;
pub(crate) mod replace_guard;
pub(crate) mod support;

use crate::db_interpose_common::stderr_ptr;
use crate::db_interpose_conn_utils::{
    cstr_prefix, cstr_to_string_or, log_error, PthreadMutexGuard,
};
use crate::ffi_types::sqlite3;
use crate::libpq_helpers::PGresult;
use pg_path::exec_via_postgres;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use support::orig_exec;

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;

const CONNECTION_OK: c_int = 0;
const PGRES_COMMAND_OK: c_int = 1;
const PGRES_TUPLES_OK: c_int = 2;
const PG_DIAG_SQLSTATE: c_int = b'C' as c_int;

type ExecCallback =
    Option<unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int>;

#[repr(C)]
struct SqlTranslation {
    sql: *mut c_char,
    param_names: *mut *mut c_char,
    param_count: c_int,
    success: c_int,
    error: [c_char; 256],
}

extern "C" {
    static mut orig_sqlite3_exec: Option<
        unsafe extern "C" fn(
            *mut sqlite3,
            *const c_char,
            ExecCallback,
            *mut c_void,
            *mut *mut c_char,
        ) -> c_int,
    >;

    fn rewrite_blobs_schema_migrations(sql: *const c_char, db_path: *const c_char) -> *mut c_char;
    fn sql_translate(sql: *const c_char) -> SqlTranslation;
    fn sql_translation_free(result: *mut SqlTranslation);
}

use crate::env_utils::loadone_trace_enabled;

pub(crate) unsafe fn exec_error_message(message: &str) -> *mut c_char {
    let Some(allocate) = crate::db_interpose_common::get_orig_sqlite3_malloc() else {
        return std::ptr::null_mut();
    };
    let bytes = message.as_bytes();
    let Ok(size) = c_int::try_from(bytes.len() + 1) else {
        return std::ptr::null_mut();
    };
    let buffer = allocate(size) as *mut c_char;
    if !buffer.is_null() {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.cast::<u8>(), bytes.len());
        *buffer.add(bytes.len()) = 0;
    }
    buffer
}

fn trim_ascii_sql(bytes: &[u8]) -> &[u8] {
    let mut i = 0usize;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    &bytes[i..]
}

unsafe fn trace_exec_skipped_select(
    route: *const c_char,
    db: *mut sqlite3,
    pg_conn: *mut crate::ffi_types::PgConnection,
    sql: *const c_char,
) {
    if !loadone_trace_enabled() || sql.is_null() {
        return;
    }

    let sql_bytes = trim_ascii_sql(CStr::from_ptr(sql).to_bytes());
    if !starts_with_icase_bytes(sql_bytes, b"select") {
        return;
    }

    let file = crate::db_interpose_open::lookup_db_handle_filename(db);
    let file_ptr = file
        .as_ref()
        .map(|s| s.as_ptr())
        .unwrap_or(b"<untracked>\0".as_ptr() as *const c_char);
    let handle_conn = crate::pg_client::rust_pg_find_handle_connection(db);
    let _ = libc::fprintf(
        stderr_ptr(),
        b"[LOADONE_TRACE][exec] route=%s db=%p file=%.900s handle_conn=%p pg_conn=%p sql=%.900s\n\0"
            .as_ptr() as *const c_char,
        route,
        db as *mut c_void,
        file_ptr,
        handle_conn as *mut c_void,
        pg_conn as *mut c_void,
        sql,
    );
    let _ = libc::fflush(stderr_ptr());
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_exec(
    db: *mut sqlite3,
    sql: *const c_char,
    callback: ExecCallback,
    arg: *mut c_void,
    errmsg: *mut *mut c_char,
) -> c_int {
    let result = rust_my_sqlite3_exec_impl(db, sql, callback, arg, errmsg);
    if result == SQLITE_ERROR {
        pg_path::primary_error_code(crate::pg_client::rust_pg_find_handle_connection(db))
    } else {
        result
    }
}

fn rust_my_sqlite3_exec_impl(
    db: *mut sqlite3,
    sql: *const c_char,
    callback: ExecCallback,
    arg: *mut c_void,
    errmsg: *mut *mut c_char,
) -> c_int {
    if sql.is_null() {
        log_error("exec called with NULL SQL");
        return orig_exec(db, sql, callback, arg, errmsg);
    }

    if crate::db_interpose_common::SHIM_PASSTHROUGH_ONLY.load(std::sync::atomic::Ordering::Acquire)
        != 0
    {
        return orig_exec(db, sql, callback, arg, errmsg);
    }

    let pg_conn = crate::pg_client::rust_pg_find_connection(db);

    if !pg_conn.is_null() && unsafe { (&*pg_conn).is_pg_active } != 0 {
        // SQLite engine config (fts3_tokenizer, icu_load_collation, load_extension)
        // must execute on real SQLite, not PG.
        let sql_str = unsafe { CStr::from_ptr(sql).to_str().unwrap_or("") };
        if crate::pg_config::is_sqlite_passthrough_str(sql_str) {
            unsafe {
                trace_exec_skipped_select(
                    b"sqlite_passthrough\0".as_ptr() as *const c_char,
                    db,
                    pg_conn,
                    sql,
                );
            }
            return orig_exec(db, sql, callback, arg, errmsg);
        }

        unsafe {
            trace_exec_skipped_select(b"pg_route\0".as_ptr() as *const c_char, db, pg_conn, sql);
        }
        let handle = crate::pg_client::rust_pg_find_handle_connection(db);
        let rc = if crate::pg_client::transaction::is_transaction_sql(sql_str) {
            crate::pg_client::transaction::execute_transaction(pg_conn, sql_str)
                .unwrap_or(SQLITE_ERROR)
        } else {
            exec_via_postgres(pg_conn, sql, handle)
        };
        unsafe {
            pg_path::copy_pg_outcome(handle, pg_conn);
            if !errmsg.is_null() {
                *errmsg = if rc == SQLITE_OK {
                    std::ptr::null_mut()
                } else {
                    exec_error_message(&cstr_to_string_or(
                        (&*pg_conn).last_error.as_ptr(),
                        "PostgreSQL execution failed",
                    ))
                };
            }
        }
        return rc;
    }

    let passthrough = crate::db_interpose_common::SHIM_PASSTHROUGH_ONLY
        .load(std::sync::atomic::Ordering::Acquire)
        != 0;
    let force_sqlite = std::env::var("PLEX_PG_FORCE_SQLITE_LIBRARY")
        .is_ok_and(|value| !value.is_empty() && value != "0");
    let filename = crate::db_interpose_open::lookup_db_handle_filename(db);
    let must_route = !passthrough
        && !force_sqlite
        && filename
            .as_ref()
            .is_some_and(|path| crate::pg_config::pg_config_should_redirect(path.as_ptr(), 0) != 0);
    let sql_str = unsafe { CStr::from_ptr(sql).to_str().unwrap_or("") };
    if must_route && !crate::pg_config::is_sqlite_passthrough_str(sql_str) {
        let msg = "PostgreSQL connection unavailable; refusing shadow SQLite execution";
        let handle = crate::pg_client::rust_pg_find_handle_connection(db);
        unsafe {
            pg_path::set_pg_last_error(handle, msg);
            if !errmsg.is_null() {
                *errmsg = exec_error_message(msg);
            }
        }
        return SQLITE_ERROR;
    }

    // For non-PG databases (e.g. :memory:), icu_load_collation may fail because
    // the ICU extension isn't available in the Docker runtime environment.
    // Rewrite to "SELECT NULL" so SOCI's loadOne gets a valid row instead of crashing.
    let sql_str = unsafe { CStr::from_ptr(sql).to_str().unwrap_or("") };
    let trimmed_lower = sql_str.trim().to_ascii_lowercase();
    if trimmed_lower.starts_with("select icu_load_collation")
        || trimmed_lower.starts_with("icu_load_collation")
    {
        return orig_exec(db, c"SELECT NULL".as_ptr(), callback, arg, errmsg);
    }

    let mut cleaned_sql: *mut c_char = std::ptr::null_mut();
    let mut exec_sql = sql;
    let sql_bytes = unsafe { CStr::from_ptr(sql).to_bytes() };
    if contains_icase_bytes(sql_bytes, b"collate icu_root") {
        cleaned_sql = crate::db_interpose_helpers::rust_strip_collate_icu_root(sql);
        if !cleaned_sql.is_null() {
            exec_sql = cleaned_sql;
        }
    }

    unsafe {
        trace_exec_skipped_select(
            b"sqlite_orig\0".as_ptr() as *const c_char,
            db,
            pg_conn,
            exec_sql,
        );
    }
    let rc = orig_exec(db, exec_sql, callback, arg, errmsg);
    if !cleaned_sql.is_null() {
        crate::db_interpose_helpers::rust_free_cstring(cleaned_sql);
    }
    rc
}

#[cfg(test)]
mod tests {
    use super::support::parse_returning_rowid;
    use std::ffi::CString;

    #[test]
    fn parse_returning_rowid_accepts_positive_values() {
        let value = CString::new("12345").unwrap();
        assert_eq!(parse_returning_rowid(value.as_ptr()), Some(12345));
    }

    #[test]
    fn parse_returning_rowid_rejects_null_and_empty_values() {
        let empty = CString::new("").unwrap();
        assert_eq!(parse_returning_rowid(std::ptr::null()), None);
        assert_eq!(parse_returning_rowid(empty.as_ptr()), None);
    }

    #[test]
    fn parse_returning_rowid_accepts_zero_and_negative_values() {
        let zero = CString::new("0").unwrap();
        let negative = CString::new("-9").unwrap();
        assert_eq!(parse_returning_rowid(zero.as_ptr()), Some(0));
        assert_eq!(parse_returning_rowid(negative.as_ptr()), Some(-9));
    }
    #[test]
    fn parse_returning_rowid_rejects_invalid_and_out_of_range_values() {
        for value in [
            "NULL",
            "invalid",
            "1x",
            "9223372036854775808",
            "-9223372036854775809",
        ] {
            let value = CString::new(value).unwrap();
            assert_eq!(parse_returning_rowid(value.as_ptr()), None);
        }
        let non_utf8 = [255_u8, 0];
        assert_eq!(parse_returning_rowid(non_utf8.as_ptr().cast()), None);
    }

    #[test]
    fn parse_returning_rowid_accepts_signed_boundaries() {
        for rowid in [i64::MIN, i64::MAX] {
            let value = CString::new(rowid.to_string()).unwrap();
            assert_eq!(parse_returning_rowid(value.as_ptr()), Some(rowid));
        }
    }
}
