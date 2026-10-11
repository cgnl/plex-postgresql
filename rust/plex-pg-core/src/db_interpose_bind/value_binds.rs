use super::*;
use crate::db_interpose_bind::support::{
    begin_bind, free_dynamic_param_value, is_pg_routed_noncached, mapped_param_index,
    retry_on_misuse,
};

unsafe fn store_sqlite_value_param(
    pg_stmt: *mut PgStmt,
    pg_idx: usize,
    mutable_value: *mut sqlite3_value,
) {
    let vtype = crate::db_interpose_value::rust_my_sqlite3_value_type(mutable_value);
    free_dynamic_param_value(pg_stmt, pg_idx);
    let stmt = &mut *pg_stmt;

    match vtype {
        SQLITE_INTEGER => {
            let v = crate::db_interpose_value::rust_my_sqlite3_value_int64(mutable_value);
            let mut buf = [0 as c_char; 32];
            libc::snprintf(
                buf.as_mut_ptr(),
                buf.len(),
                b"%lld\0".as_ptr() as *const c_char,
                v as libc::c_longlong,
            );
            stmt.param_values[pg_idx] = libc::strdup(buf.as_ptr());
        }
        SQLITE_FLOAT => {
            let v = crate::db_interpose_value::rust_my_sqlite3_value_double(mutable_value);
            let mut buf = [0 as c_char; 64];
            libc::snprintf(
                buf.as_mut_ptr(),
                buf.len(),
                b"%.17g\0".as_ptr() as *const c_char,
                v,
            );
            stmt.param_values[pg_idx] = libc::strdup(buf.as_ptr());
        }
        SQLITE_TEXT => {
            let v = crate::db_interpose_value::rust_my_sqlite3_value_text(mutable_value);
            if !v.is_null() {
                stmt.param_values[pg_idx] = libc::strdup(v as *const c_char);
            }
        }
        SQLITE_BLOB => {
            let len = crate::db_interpose_value::rust_my_sqlite3_value_bytes(mutable_value);
            let v = crate::db_interpose_value::rust_my_sqlite3_value_blob(mutable_value);
            if !v.is_null() && len > 0 {
                stmt.param_values[pg_idx] = libc::malloc(len as usize) as *mut c_char;
                if !stmt.param_values[pg_idx].is_null() {
                    libc::memcpy(stmt.param_values[pg_idx] as *mut c_void, v, len as usize);
                    if crate::pg_mem_telemetry::rust_mem_telemetry_enabled() != 0 {
                        crate::pg_mem_telemetry::rust_mem_telemetry_add(
                            PMT_BIND_VALUE_BLOB_ALLOC,
                            len as u64,
                            1,
                        );
                    }
                }
                stmt.param_lengths[pg_idx] = len;
                stmt.param_formats[pg_idx] = 1;
            }
        }
        SQLITE_NULL | _ => {}
    }
}

pub(super) fn bind_value_impl(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    p_value: *const sqlite3_value,
) -> c_int {
    let (pg_stmt, guard) = unsafe { begin_bind(PHASE_BIND_VALUE, p_stmt) };

    let rc = if is_pg_routed_noncached(pg_stmt) {
        SQLITE_OK
    } else {
        let mut rc = get_orig_sqlite3_bind_value()
            .map(|f| unsafe { f(p_stmt, idx, p_value) })
            .unwrap_or(SQLITE_ERROR);
        unsafe {
            rc = retry_on_misuse(rc, p_stmt, pg_stmt, || {
                get_orig_sqlite3_bind_value()
                    .map(|f| f(p_stmt, idx, p_value))
                    .unwrap_or(SQLITE_ERROR)
            });
        }
        rc
    };

    if !p_value.is_null() {
        if let Some(pg_idx) = unsafe { mapped_param_index(pg_stmt, p_stmt, idx) } {
            unsafe {
                store_sqlite_value_param(pg_stmt, pg_idx, p_value as *mut sqlite3_value);
            }
        }
    }

    drop(guard);
    rc
}

pub(super) fn bind_null_impl(p_stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    let (pg_stmt, guard) = unsafe { begin_bind(PHASE_BIND_NULL, p_stmt) };

    let rc = if is_pg_routed_noncached(pg_stmt) {
        SQLITE_OK
    } else {
        let mut rc = get_orig_sqlite3_bind_null()
            .map(|f| unsafe { f(p_stmt, idx) })
            .unwrap_or(SQLITE_ERROR);
        unsafe {
            rc = retry_on_misuse(rc, p_stmt, pg_stmt, || {
                get_orig_sqlite3_bind_null()
                    .map(|f| f(p_stmt, idx))
                    .unwrap_or(SQLITE_ERROR)
            });
        }
        rc
    };

    if let Some(pg_idx) = unsafe { mapped_param_index(pg_stmt, p_stmt, idx) } {
        unsafe { free_dynamic_param_value(pg_stmt, pg_idx) };
    }

    drop(guard);
    rc
}
