use crate::db_interpose_value_helpers::{
    pg_oid_to_sqlite_type_impl, pg_text_to_double_impl, pg_text_to_int64_impl, pg_text_to_int_impl,
};
use crate::pg_query_cache::rust_query_cache_store;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_uint, c_void};

use super::{
    cstr_to_str, format_epoch_to_datetime_utc_impl, is_aggregate_alias, pg_sql_has_timestamp_hint,
    rewrite_server_library_uri_bytes, rust_decode_hex_bytes, write_i32_to_buf, write_i64_to_buf,
    PGresult, SQLITE_NULL_CONST,
};

extern "C" {
    fn PQgetisnull(res: *const PGresult, row: c_int, col: c_int) -> c_int;
    fn PQgetvalue(res: *const PGresult, row: c_int, col: c_int) -> *const c_char;
    fn PQgetlength(res: *const PGresult, row: c_int, col: c_int) -> c_int;
    fn PQftype(res: *const PGresult, col: c_int) -> c_uint;
    fn PQfname(res: *const PGresult, col: c_int) -> *const c_char;
    fn PQftable(res: *const PGresult, col: c_int) -> c_uint;
    fn PQntuples(res: *const PGresult) -> c_int;
    fn PQnfields(res: *const PGresult) -> c_int;
}

pub fn rust_column_text_reformat_aggregate(
    col_name: *const c_char,
    oid: c_uint,
    pg_sql: *const c_char,
    source_value: *const c_char,
    out: *mut c_char,
    out_len: usize,
) -> c_int {
    if out.is_null() || out_len == 0 {
        return 0;
    }

    let col = match cstr_to_str(col_name) {
        Some(s) if !s.is_empty() => s,
        _ => return 0,
    };

    if !matches!(oid, 20 | 21 | 23) {
        return 0;
    }

    if !is_aggregate_alias(col) {
        return 0;
    }

    if oid == 20 {
        let val = pg_text_to_int64_impl(source_value);
        let pg_sql = cstr_to_str(pg_sql).unwrap_or("");
        if (col.eq_ignore_ascii_case("max") || col.eq_ignore_ascii_case("min"))
            && !pg_sql.is_empty()
            && pg_sql_has_timestamp_hint(pg_sql)
            && format_epoch_to_datetime_utc_impl(val, out, out_len) != 0
        {
            return 1;
        }
        return c_int::from(write_i64_to_buf(out, out_len, val));
    }

    let val = pg_text_to_int_impl(source_value);
    c_int::from(write_i32_to_buf(out, out_len, val))
}

pub fn rust_column_text_transform(
    col_name: *const c_char,
    oid: c_uint,
    pg_sql: *const c_char,
    source_value: *const c_char,
    source_len: usize,
    out: *mut c_char,
    out_len: usize,
) -> c_int {
    if out.is_null() || out_len == 0 || source_value.is_null() {
        return 0;
    }

    let bytes = unsafe { std::slice::from_raw_parts(source_value.cast::<u8>(), source_len) };
    if std::str::from_utf8(bytes).is_err() {
        unsafe {
            *out = 0;
        }
        return -1;
    }

    if rust_column_text_reformat_aggregate(col_name, oid, pg_sql, source_value, out, out_len) != 0 {
        return 1;
    }

    let out_cap = out_len.saturating_sub(1);
    if let Some(rewritten) = rewrite_server_library_uri_bytes(bytes, out_cap) {
        let n = rewritten.len().min(out_cap);
        unsafe {
            std::ptr::copy_nonoverlapping(rewritten.as_ptr(), out.cast::<u8>(), n);
            *out.add(n) = 0;
        }
        return 1;
    }

    0
}

pub fn rust_pg_result_text_copy(
    result: *const PGresult,
    row: c_int,
    col: c_int,
    out: *mut c_char,
    out_len: usize,
) -> c_int {
    if result.is_null() || out.is_null() || out_len == 0 {
        return -1;
    }
    let is_null = unsafe { PQgetisnull(result, row, col) } != 0;
    if is_null {
        return -1;
    }
    let val_ptr = unsafe { PQgetvalue(result, row, col) };
    if val_ptr.is_null() {
        return -1;
    }
    let len = unsafe { PQgetlength(result, row, col) };
    if len < 0 {
        return -1;
    }
    let len_usize = len as usize;
    let copy_len = len_usize.min(out_len.saturating_sub(1));
    unsafe {
        std::ptr::copy_nonoverlapping(val_ptr.cast::<u8>(), out.cast::<u8>(), copy_len);
        *out.add(copy_len) = 0;
    }
    len
}

pub fn rust_pg_result_blob_copy(
    result: *const PGresult,
    row: c_int,
    col: c_int,
    out: *mut u8,
    out_len: usize,
) -> c_int {
    if result.is_null() || out.is_null() || out_len == 0 {
        return -1;
    }
    let is_null = unsafe { PQgetisnull(result, row, col) } != 0;
    if is_null {
        return -1;
    }
    let val_ptr = unsafe { PQgetvalue(result, row, col) };
    if val_ptr.is_null() {
        return -1;
    }
    let len = unsafe { PQgetlength(result, row, col) };
    if len <= 0 {
        return -1;
    }
    let len_usize = len as usize;
    let copy_len = len_usize.min(out_len);
    unsafe {
        std::ptr::copy_nonoverlapping(val_ptr.cast::<u8>(), out, copy_len);
    }
    copy_len as c_int
}

pub fn rust_pg_result_length(result: *const PGresult, row: c_int, col: c_int) -> c_int {
    if result.is_null() {
        return -1;
    }
    let is_null = unsafe { PQgetisnull(result, row, col) } != 0;
    if is_null {
        return -1;
    }
    unsafe { PQgetlength(result, row, col) }
}

pub fn rust_pg_result_int(result: *const PGresult, row: c_int, col: c_int) -> c_int {
    if result.is_null() {
        return 0;
    }
    let is_null = unsafe { PQgetisnull(result, row, col) } != 0;
    if is_null {
        return 0;
    }
    let val_ptr = unsafe { PQgetvalue(result, row, col) };
    if val_ptr.is_null() {
        return 0;
    }
    pg_text_to_int_impl(val_ptr)
}

pub fn rust_pg_result_int64(result: *const PGresult, row: c_int, col: c_int) -> i64 {
    if result.is_null() {
        return 0;
    }
    let is_null = unsafe { PQgetisnull(result, row, col) } != 0;
    if is_null {
        return 0;
    }
    let val_ptr = unsafe { PQgetvalue(result, row, col) };
    if val_ptr.is_null() {
        return 0;
    }
    pg_text_to_int64_impl(val_ptr)
}

pub fn rust_pg_result_double(result: *const PGresult, row: c_int, col: c_int) -> f64 {
    if result.is_null() {
        return 0.0;
    }
    let is_null = unsafe { PQgetisnull(result, row, col) } != 0;
    if is_null {
        return 0.0;
    }
    let val_ptr = unsafe { PQgetvalue(result, row, col) };
    if val_ptr.is_null() {
        return 0.0;
    }
    pg_text_to_double_impl(val_ptr)
}

pub fn rust_pg_result_text_transform_copy(
    result: *const PGresult,
    row: c_int,
    col: c_int,
    col_name: *const c_char,
    oid: c_uint,
    pg_sql: *const c_char,
    is_null: c_int,
    out: *mut c_char,
    out_len: usize,
    preview: *mut c_char,
    preview_len: usize,
    source_len_out: *mut usize,
) -> c_int {
    if result.is_null() || out.is_null() || out_len == 0 {
        if !out.is_null() && out_len > 0 {
            unsafe {
                *out = 0;
            }
        }
        if !preview.is_null() && preview_len > 0 {
            unsafe {
                *preview = 0;
            }
        }
        if !source_len_out.is_null() {
            unsafe {
                *source_len_out = 0;
            }
        }
        return -3;
    }

    if is_null != 0 {
        unsafe {
            *out = 0;
        }
        if !preview.is_null() && preview_len > 0 {
            unsafe {
                *preview = 0;
            }
        }
        if !source_len_out.is_null() {
            unsafe {
                *source_len_out = 0;
            }
        }
        return -2;
    }

    let val_ptr = unsafe { PQgetvalue(result, row, col) };
    if val_ptr.is_null() {
        unsafe {
            *out = 0;
        }
        if !preview.is_null() && preview_len > 0 {
            unsafe {
                *preview = 0;
            }
        }
        if !source_len_out.is_null() {
            unsafe {
                *source_len_out = 0;
            }
        }
        return -2;
    }

    let len = unsafe { PQgetlength(result, row, col) };
    if len < 0 {
        unsafe {
            *out = 0;
        }
        if !preview.is_null() && preview_len > 0 {
            unsafe {
                *preview = 0;
            }
        }
        if !source_len_out.is_null() {
            unsafe {
                *source_len_out = 0;
            }
        }
        return -3;
    }
    let len_usize = len as usize;
    if !source_len_out.is_null() {
        unsafe {
            *source_len_out = len_usize;
        }
    }

    if !preview.is_null() && preview_len > 0 {
        let copy_len = len_usize.min(preview_len.saturating_sub(1));
        unsafe {
            std::ptr::copy_nonoverlapping(val_ptr.cast::<u8>(), preview.cast::<u8>(), copy_len);
            *preview.add(copy_len) = 0;
        }
    }

    let transform_rc =
        rust_column_text_transform(col_name, oid, pg_sql, val_ptr, len_usize, out, out_len);
    if transform_rc != 0 {
        return transform_rc;
    }

    let copy_len = len_usize.min(out_len.saturating_sub(1));
    unsafe {
        std::ptr::copy_nonoverlapping(val_ptr.cast::<u8>(), out.cast::<u8>(), copy_len);
        *out.add(copy_len) = 0;
    }
    0
}

pub fn rust_pg_result_value_ptr_len(
    result: *const PGresult,
    row: c_int,
    col: c_int,
    ptr_out: *mut *const c_char,
    len_out: *mut c_int,
    is_null_out: *mut c_int,
) -> c_int {
    if result.is_null() {
        return 0;
    }

    let is_null = unsafe { PQgetisnull(result, row, col) };
    if !is_null_out.is_null() {
        unsafe {
            *is_null_out = is_null;
        }
    }
    if is_null != 0 {
        if !ptr_out.is_null() {
            unsafe {
                *ptr_out = std::ptr::null();
            }
        }
        if !len_out.is_null() {
            unsafe {
                *len_out = 0;
            }
        }
        return 1;
    }

    let val_ptr = unsafe { PQgetvalue(result, row, col) };
    if val_ptr.is_null() {
        if !ptr_out.is_null() {
            unsafe {
                *ptr_out = std::ptr::null();
            }
        }
        if !len_out.is_null() {
            unsafe {
                *len_out = 0;
            }
        }
        return 0;
    }
    let len = unsafe { PQgetlength(result, row, col) };
    if len < 0 {
        if !ptr_out.is_null() {
            unsafe {
                *ptr_out = std::ptr::null();
            }
        }
        if !len_out.is_null() {
            unsafe {
                *len_out = 0;
            }
        }
        return 0;
    }

    if !ptr_out.is_null() {
        unsafe {
            *ptr_out = val_ptr;
        }
    }
    if !len_out.is_null() {
        unsafe {
            *len_out = len;
        }
    }
    1
}

pub fn rust_pg_result_col_oid(result: *const PGresult, col: c_int) -> c_uint {
    if result.is_null() {
        return 0;
    }
    unsafe { PQftype(result, col) }
}

pub fn rust_pg_result_col_table_oid(result: *const PGresult, col: c_int) -> c_uint {
    if result.is_null() {
        return 0;
    }
    unsafe { PQftable(result, col) }
}

pub fn rust_pg_decode_bytea(
    result: *const PGresult,
    row: c_int,
    col: c_int,
    ptr_out: *mut *mut u8,
    len_out: *mut c_int,
    is_hex_out: *mut c_int,
    is_null_out: *mut c_int,
) -> c_int {
    if result.is_null()
        || ptr_out.is_null()
        || len_out.is_null()
        || is_hex_out.is_null()
        || is_null_out.is_null()
    {
        return 0;
    }

    let is_null = unsafe { PQgetisnull(result, row, col) };
    unsafe {
        *is_null_out = is_null;
    }
    if is_null != 0 {
        unsafe {
            *ptr_out = std::ptr::null_mut();
            *len_out = 0;
            *is_hex_out = 0;
        }
        return 1;
    }

    let val_ptr = unsafe { PQgetvalue(result, row, col) };
    if val_ptr.is_null() {
        unsafe {
            *ptr_out = std::ptr::null_mut();
            *len_out = 0;
            *is_hex_out = 0;
        }
        return 0;
    }
    let len = unsafe { PQgetlength(result, row, col) };
    if len < 0 {
        unsafe {
            *ptr_out = std::ptr::null_mut();
            *len_out = 0;
            *is_hex_out = 0;
        }
        return 0;
    }

    let len_usize = len as usize;
    let bytes = unsafe { std::slice::from_raw_parts(val_ptr.cast::<u8>(), len_usize) };
    if len_usize < 2 || bytes[0] != b'\\' || bytes[1] != b'x' {
        unsafe {
            *ptr_out = val_ptr.cast_mut().cast::<u8>();
            *len_out = len;
            *is_hex_out = 0;
        }
        return 1;
    }

    let hex = &bytes[2..];
    if (hex.len() % 2) != 0 {
        unsafe {
            *ptr_out = std::ptr::null_mut();
            *len_out = 0;
            *is_hex_out = 1;
        }
        return 0;
    }
    let bin_len = hex.len() / 2;
    let alloc = unsafe { libc::malloc(bin_len + 1) } as *mut u8;
    if alloc.is_null() {
        unsafe {
            *ptr_out = std::ptr::null_mut();
            *len_out = 0;
            *is_hex_out = 1;
        }
        return 0;
    }
    let ok = rust_decode_hex_bytes(hex.as_ptr() as *const c_char, hex.len(), alloc, bin_len);
    if ok == 0 {
        unsafe {
            libc::free(alloc as *mut c_void);
            *ptr_out = std::ptr::null_mut();
            *len_out = 0;
            *is_hex_out = 1;
        }
        return 0;
    }

    unsafe {
        *ptr_out = alloc;
        *len_out = bin_len as c_int;
        *is_hex_out = 1;
    }
    1
}

pub fn rust_query_cache_store_from_pgresult(
    cache_key: u64,
    result: *const PGresult,
    num_rows: c_int,
    num_cols: c_int,
    pg_sql: *const c_char,
) {
    if result.is_null() || cache_key == 0 || num_rows <= 0 || num_cols <= 0 {
        return;
    }

    let nr = num_rows as usize;
    let nc = num_cols as usize;
    let total = nr.saturating_mul(nc);

    let mut col_types: Vec<u32> = Vec::with_capacity(nc);
    let mut col_names: Vec<*const c_char> = Vec::with_capacity(nc);
    for c in 0..nc {
        col_types.push(unsafe { PQftype(result, c as c_int) } as u32);
        col_names.push(unsafe { PQfname(result, c as c_int) });
    }

    let mut values: Vec<*const c_char> = Vec::with_capacity(total);
    let mut lengths: Vec<i32> = Vec::with_capacity(total);
    let mut is_null: Vec<i32> = Vec::with_capacity(total);

    for r in 0..nr {
        for c in 0..nc {
            let null_flag = unsafe { PQgetisnull(result, r as c_int, c as c_int) };
            if null_flag != 0 {
                is_null.push(1);
                values.push(std::ptr::null());
                lengths.push(0);
            } else {
                let val_ptr = unsafe { PQgetvalue(result, r as c_int, c as c_int) };
                let len = unsafe { PQgetlength(result, r as c_int, c as c_int) };
                is_null.push(0);
                values.push(val_ptr);
                lengths.push(if len < 0 { 0 } else { len });
            }
        }
    }

    rust_query_cache_store(
        cache_key,
        num_rows,
        num_cols,
        col_types.as_ptr(),
        col_names.as_ptr(),
        values.as_ptr(),
        lengths.as_ptr(),
        is_null.as_ptr(),
        pg_sql,
    );
}

pub fn rust_get_table_from_pgresult(
    result: *const PGresult,
    out_rows: *mut *mut *mut c_char,
    out_rows_count: *mut c_int,
    out_cols_count: *mut c_int,
) -> c_int {
    if result.is_null()
        || out_rows.is_null()
        || out_rows_count.is_null()
        || out_cols_count.is_null()
    {
        return 0;
    }

    let num_rows = unsafe { PQntuples(result) };
    let num_cols = unsafe { PQnfields(result) };
    if num_rows < 0 || num_cols <= 0 {
        unsafe {
            *out_rows = std::ptr::null_mut();
            *out_rows_count = 0;
            *out_cols_count = 0;
        }
        return 0;
    }

    let total = (num_rows as usize + 1).saturating_mul(num_cols as usize) + 1;
    let rows_ptr =
        unsafe { libc::malloc(total * std::mem::size_of::<*mut c_char>()) as *mut *mut c_char };
    if rows_ptr.is_null() {
        unsafe {
            *out_rows = std::ptr::null_mut();
            *out_rows_count = 0;
            *out_cols_count = 0;
        }
        return 0;
    }
    unsafe {
        std::ptr::write_bytes(rows_ptr, 0, total);
    }

    let mut ok = true;
    for c in 0..num_cols {
        let name = unsafe { PQfname(result, c) };
        if !name.is_null() {
            let bytes = unsafe { CStr::from_ptr(name).to_bytes() };
            let buf = unsafe { libc::malloc(bytes.len() + 1) } as *mut u8;
            if buf.is_null() {
                ok = false;
                break;
            }
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, bytes.len());
                *buf.add(bytes.len()) = 0;
                *rows_ptr.add(c as usize) = buf as *mut c_char;
            }
        }
    }

    if ok {
        for r in 0..num_rows {
            for c in 0..num_cols {
                let idx = (r as usize + 1) * (num_cols as usize) + c as usize;
                let is_null = unsafe { PQgetisnull(result, r, c) };
                if is_null != 0 {
                    unsafe { *rows_ptr.add(idx) = std::ptr::null_mut() };
                    continue;
                }
                let val_ptr = unsafe { PQgetvalue(result, r, c) };
                if val_ptr.is_null() {
                    unsafe { *rows_ptr.add(idx) = std::ptr::null_mut() };
                    continue;
                }
                let len = unsafe { PQgetlength(result, r, c) };
                let len_usize = if len < 0 { 0 } else { len as usize };
                let buf = unsafe { libc::malloc(len_usize + 1) } as *mut u8;
                if buf.is_null() {
                    ok = false;
                    break;
                }
                unsafe {
                    if len_usize > 0 {
                        std::ptr::copy_nonoverlapping(val_ptr.cast::<u8>(), buf, len_usize);
                    }
                    *buf.add(len_usize) = 0;
                    *rows_ptr.add(idx) = buf as *mut c_char;
                }
            }
            if !ok {
                break;
            }
        }
    }

    if !ok {
        unsafe {
            for i in 0..total {
                let ptr = *rows_ptr.add(i);
                if !ptr.is_null() {
                    libc::free(ptr as *mut c_void);
                }
            }
            libc::free(rows_ptr as *mut c_void);
            *out_rows = std::ptr::null_mut();
            *out_rows_count = 0;
            *out_cols_count = 0;
        }
        return 0;
    }

    unsafe {
        *rows_ptr.add(total - 1) = std::ptr::null_mut();
        *out_rows = rows_ptr;
        *out_rows_count = num_rows;
        *out_cols_count = num_cols;
    }
    1
}

pub fn rust_pg_create_column_value(
    result: *const PGresult,
    current_row: c_int,
    num_rows: c_int,
    col_idx: c_int,
) -> c_int {
    if result.is_null() || current_row < 0 || current_row >= num_rows {
        SQLITE_NULL_CONST
    } else {
        let is_null = unsafe { PQgetisnull(result, current_row, col_idx) };
        if is_null != 0 {
            SQLITE_NULL_CONST
        } else {
            let oid = unsafe { PQftype(result, col_idx) };
            pg_oid_to_sqlite_type_impl(oid as u32)
        }
    }
}

pub fn rust_pg_result_type_info(
    result: *const PGresult,
    row: c_int,
    col: c_int,
    oid_out: *mut c_uint,
    is_null_out: *mut c_int,
    sqlite_type_out: *mut c_int,
) -> c_int {
    if result.is_null() {
        return 0;
    }
    let is_null = unsafe { PQgetisnull(result, row, col) };
    if !is_null_out.is_null() {
        unsafe {
            *is_null_out = is_null;
        }
    }
    let oid = unsafe { PQftype(result, col) };
    if !oid_out.is_null() {
        unsafe {
            *oid_out = oid;
        }
    }
    if !sqlite_type_out.is_null() {
        let sqlite_type = if is_null != 0 {
            SQLITE_NULL_CONST
        } else {
            pg_oid_to_sqlite_type_impl(oid as u32)
        };
        unsafe {
            *sqlite_type_out = sqlite_type;
        }
    }
    1
}

pub fn rust_pg_result_col_name(result: *const PGresult, col: c_int) -> *const c_char {
    if result.is_null() {
        return std::ptr::null();
    }
    unsafe { PQfname(result, col) }
}

pub fn rust_step_clear_row_caches(
    cached_text: *mut *mut c_char,
    cached_blob: *mut *mut c_void,
    cached_blob_len: *mut c_int,
    decoded_blobs: *mut *mut c_void,
    decoded_blob_lens: *mut c_int,
    max_params: c_int,
    cached_row: *mut c_int,
    decoded_blob_row: *mut c_int,
) {
    if max_params <= 0 {
        if !cached_row.is_null() {
            unsafe { *cached_row = -1 };
        }
        if !decoded_blob_row.is_null() {
            unsafe { *decoded_blob_row = -1 };
        }
        return;
    }

    for i in 0..max_params as isize {
        unsafe {
            if !cached_text.is_null() {
                let slot = cached_text.offset(i);
                let ptr = *slot;
                if !ptr.is_null() {
                    libc::free(ptr as *mut c_void);
                    *slot = std::ptr::null_mut();
                }
            }

            if !cached_blob.is_null() {
                let slot = cached_blob.offset(i);
                let ptr = *slot;
                if !ptr.is_null() {
                    libc::free(ptr);
                    *slot = std::ptr::null_mut();
                }
            }

            if !cached_blob_len.is_null() {
                *cached_blob_len.offset(i) = 0;
            }

            if !decoded_blobs.is_null() {
                let slot = decoded_blobs.offset(i);
                let ptr = *slot;
                if !ptr.is_null() {
                    libc::free(ptr);
                    *slot = std::ptr::null_mut();
                }
            }

            if !decoded_blob_lens.is_null() {
                *decoded_blob_lens.offset(i) = 0;
            }
        }
    }

    if !cached_row.is_null() {
        unsafe { *cached_row = -1 };
    }
    if !decoded_blob_row.is_null() {
        unsafe { *decoded_blob_row = -1 };
    }
}
