use super::*;

pub(super) fn malloc_cstring(value: &str) -> *mut c_char {
    let bytes = value.as_bytes();
    unsafe {
        let ptr = libc::malloc(bytes.len() + 1) as *mut c_char;
        if ptr.is_null() {
            return std::ptr::null_mut();
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
        *ptr.add(bytes.len()) = 0;
        ptr
    }
}

pub(super) unsafe fn owned_db_path(conn: *mut PgConnection) -> Option<CString> {
    if conn.is_null() {
        return None;
    }
    let c = &*conn;
    if c.db_path[0] == 0 {
        return None;
    }
    Some(CStr::from_ptr(c.db_path.as_ptr()).to_owned())
}

pub(super) fn is_duplicate_prepared_stmt(res: *mut PGresult) -> bool {
    if res.is_null() {
        return false;
    }
    let sqlstate = crate::libpq_helpers::rust_pq_result_error_field(res, PG_DIAG_SQLSTATE);
    crate::pg_client::rust_is_duplicate_sqlstate(sqlstate) != 0
}

pub(super) fn is_stale_prepared_stmt(res: *mut PGresult) -> bool {
    if res.is_null() {
        return false;
    }
    let sqlstate = crate::libpq_helpers::rust_pq_result_error_field(res, PG_DIAG_SQLSTATE);
    crate::pg_client::rust_is_stale_sqlstate(sqlstate) != 0
}

pub(super) fn skip_stats_resources_update() -> bool {
    let cached = SKIP_STATS_RESOURCES_UPDATE.load(Ordering::Relaxed);
    if cached != -1 {
        return cached == 1;
    }
    let flag = env_utils::env_string("PLEX_PG_SKIP_STATS_RESOURCES_UPDATE")
        .and_then(|v| v.chars().next())
        .map(|c| c != '0')
        .unwrap_or(false);
    SKIP_STATS_RESOURCES_UPDATE.store(if flag { 1 } else { 0 }, Ordering::Relaxed);
    flag
}

pub(super) unsafe fn param_at(param_values: *const *const c_char, idx: usize) -> *const c_char {
    if param_values.is_null() {
        return std::ptr::null();
    }
    *param_values.add(idx)
}
