use crate::db_interpose_prepare_helpers::{
    add_if_not_exists_for_sqlite_ddl, alias_collection_sync_aggregates, prepare_query_loop_tick,
    prepare_simple_hash, prepare_time_ms, simplify_fts_for_sqlite, strip_collate_icu_root,
};
use crate::db_interpose_trace_helpers::{
    getenv_nonempty, list_any_token_in_haystack, list_contains_idx, read_first_line_trimmed,
    trim_first_line,
};
use crate::db_interpose_value_helpers::{
    pg_oid_to_sqlite_type_impl, pg_text_to_double_impl, pg_text_to_int64_impl, pg_text_to_int_impl,
    SQLITE_BLOB_CONST, SQLITE_FLOAT_CONST, SQLITE_INTEGER_CONST, SQLITE_TEXT_CONST,
};
pub use crate::libpq_helpers::PGresult;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::os::raw::c_int;
use std::os::raw::c_uint;

mod badcast_trace;
mod decltype;
mod ffi_strings;
mod pg_result;
mod pure_helpers;
mod string_utils;
mod type_cache;

pub use badcast_trace::{
    rust_env_truthy, rust_load_badcast_config, rust_read_first_line_trim_to_buf,
    rust_trace_prepare_sql_ok,
};
pub(crate) use decltype::{normalize_sqlite_decltype_impl, pg_udt_to_sqlite_decltype_impl};
pub use ffi_strings::{
    rust_free_cstring, rust_free_normalized_sql, rust_normalize_sql_literals,
    rust_rewrite_server_library_uri, rust_validate_utf8, RustNormalizedSql,
};
pub use pg_result::{
    rust_column_text_reformat_aggregate, rust_column_text_transform, rust_get_table_from_pgresult,
    rust_pg_create_column_value, rust_pg_decode_bytea, rust_pg_result_blob_copy,
    rust_pg_result_col_name, rust_pg_result_col_oid, rust_pg_result_col_table_oid,
    rust_pg_result_double, rust_pg_result_int, rust_pg_result_int64, rust_pg_result_length,
    rust_pg_result_text_copy, rust_pg_result_text_transform_copy, rust_pg_result_type_info,
    rust_pg_result_value_ptr_len, rust_query_cache_store_from_pgresult, rust_step_clear_row_caches,
};
use pure_helpers::{
    bytes_to_pg_hex_impl, contains_binary_bytes_impl, find_insert_column_index_impl,
    format_epoch_to_datetime_utc_impl, is_aggregate_alias, is_blobs_db_path_impl,
    is_junk_metadata_insert_impl, is_library_db_path_impl, is_library_or_blobs_db_path_impl,
    is_related_items_query_impl, normalize_sql_literals_impl, pg_sql_has_timestamp_hint,
    rewrite_server_library_uri_bytes, should_mask_collection_metadata_type_impl,
};
pub(crate) use string_utils::cstr_to_str_or_empty;
use string_utils::{
    contains_ascii_icase, cstr_to_str, find_ascii_icase, find_closing_paren, find_subslice,
    has_boundary, is_next_numeric_boundary, is_prev_numeric_boundary, normalize_ident_token,
    push_capped, split_csv_simple, starts_with_icase, write_buf, write_i32_to_buf,
    write_i64_to_buf,
};
pub use type_cache::{
    rust_decltype_cache_insert, rust_decltype_cache_lookup, rust_decltype_cache_lookup_alias,
    rust_decltype_hash, rust_expected_sqlite_type_for_decltype, rust_oid_table_cache_insert,
    rust_oid_table_cache_lookup,
};

const SQLITE_NULL_CONST: i32 = 5;

pub fn rust_decode_hex_bytes(
    hex: *const c_char,
    hex_len: usize,
    out: *mut u8,
    out_len: usize,
) -> c_int {
    if hex.is_null() || out.is_null() {
        return 0;
    }
    if hex_len == 0 || !hex_len.is_multiple_of(2) {
        return 0;
    }
    let expected = hex_len / 2;
    if out_len < expected {
        return 0;
    }
    let bytes = unsafe { std::slice::from_raw_parts(hex.cast::<u8>(), hex_len) };

    fn hex_val(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }

    let out_slice = unsafe { std::slice::from_raw_parts_mut(out, expected) };
    for i in 0..expected {
        let hi = match hex_val(bytes[i * 2]) {
            Some(v) => v,
            None => return 0,
        };
        let lo = match hex_val(bytes[i * 2 + 1]) {
            Some(v) => v,
            None => return 0,
        };
        out_slice[i] = (hi << 4) | lo;
    }
    expected as c_int
}

pub fn rust_pg_udt_to_sqlite_decltype(ptr: *const c_char) -> *const c_char {
    pg_udt_to_sqlite_decltype_impl(cstr_to_str(ptr))
}

pub fn rust_normalize_sqlite_decltype(ptr: *const c_char) -> *const c_char {
    normalize_sqlite_decltype_impl(cstr_to_str(ptr))
}

pub fn rust_prepare_simple_hash(ptr: *const c_char, max_len: i32) -> u32 {
    let s = cstr_to_str(ptr).unwrap_or("");
    prepare_simple_hash(s, max_len)
}

pub fn rust_prepare_time_ms() -> u64 {
    prepare_time_ms()
}

pub fn rust_prepare_query_loop_tick(
    sql: *const c_char,
    count_out: *mut c_int,
    elapsed_ms_out: *mut u64,
) -> c_int {
    if sql.is_null() {
        return 0;
    }
    let s = cstr_to_str(sql).unwrap_or("");
    let (detected, count, elapsed) = match prepare_query_loop_tick(s) {
        Some((count, elapsed)) => (1, count, elapsed),
        None => (0, 0, 0),
    };

    if !count_out.is_null() {
        unsafe {
            *count_out = count;
        }
    }
    if !elapsed_ms_out.is_null() {
        unsafe {
            *elapsed_ms_out = elapsed;
        }
    }
    detected
}

pub fn rust_maybe_alias_collection_sync_aggregates(
    sqlite_sql: *const c_char,
    pg_sql: *const c_char,
) -> *mut c_char {
    if sqlite_sql.is_null() || pg_sql.is_null() {
        return std::ptr::null_mut();
    }
    let sqlite_sql = match unsafe { CStr::from_ptr(sqlite_sql) }.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let pg_sql = match unsafe { CStr::from_ptr(pg_sql) }.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };

    let Some(out) = alias_collection_sync_aggregates(sqlite_sql, pg_sql) else {
        return std::ptr::null_mut();
    };
    match std::ffi::CString::new(out) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

pub fn rust_strip_collate_icu_root(sql: *const c_char) -> *mut c_char {
    if sql.is_null() {
        return std::ptr::null_mut();
    }
    let sql = match unsafe { CStr::from_ptr(sql) }.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let Some(out) = strip_collate_icu_root(sql) else {
        return std::ptr::null_mut();
    };
    match std::ffi::CString::new(out) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

pub fn rust_is_junk_metadata_insert(sql: *const c_char) -> c_int {
    if sql.is_null() {
        return 0;
    }
    let sql = match unsafe { CStr::from_ptr(sql) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    i32::from(is_junk_metadata_insert_impl(sql))
}

pub fn rust_is_library_db_path(path: *const c_char) -> c_int {
    if path.is_null() {
        return 0;
    }
    let path = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    i32::from(is_library_db_path_impl(path))
}

pub fn rust_is_library_or_blobs_db_path(path: *const c_char) -> c_int {
    if path.is_null() {
        return 0;
    }
    let path = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    i32::from(is_library_or_blobs_db_path_impl(path))
}

pub fn rust_is_blobs_db_path(path: *const c_char) -> c_int {
    if path.is_null() {
        return 0;
    }
    let path = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    i32::from(is_blobs_db_path_impl(path))
}

pub fn rust_trace_list_contains_idx(list: *const c_char, idx: c_int) -> c_int {
    if list.is_null() {
        return 0;
    }
    let list = match unsafe { CStr::from_ptr(list) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    i32::from(list_contains_idx(list, idx))
}

pub fn rust_trace_list_any_token_in_haystack(
    list: *const c_char,
    haystack: *const c_char,
) -> c_int {
    if list.is_null() || haystack.is_null() {
        return 0;
    }
    let list = match unsafe { CStr::from_ptr(list) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let haystack = match unsafe { CStr::from_ptr(haystack) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    i32::from(list_any_token_in_haystack(list, haystack))
}

pub fn rust_simplify_fts_for_sqlite(sql: *const c_char) -> *mut c_char {
    if sql.is_null() {
        return std::ptr::null_mut();
    }
    let sql = match unsafe { CStr::from_ptr(sql) }.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let Some(out) = simplify_fts_for_sqlite(sql) else {
        return std::ptr::null_mut();
    };
    match std::ffi::CString::new(out) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

pub fn rust_add_if_not_exists_for_sqlite_ddl(sql: *const c_char) -> *mut c_char {
    if sql.is_null() {
        return std::ptr::null_mut();
    }
    let sql = match unsafe { CStr::from_ptr(sql) }.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let Some(out) = add_if_not_exists_for_sqlite_ddl(sql) else {
        return std::ptr::null_mut();
    };
    match std::ffi::CString::new(out) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

pub fn rust_format_epoch_to_datetime_utc(epoch: i64, out: *mut c_char, out_len: usize) -> c_int {
    format_epoch_to_datetime_utc_impl(epoch, out, out_len)
}

pub fn rust_contains_binary_bytes(data: *const u8, len: usize) -> c_int {
    if data.is_null() || len == 0 {
        return 0;
    }
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    i32::from(contains_binary_bytes_impl(bytes))
}

pub fn rust_bytes_to_pg_hex(data: *const u8, len: usize) -> *mut c_char {
    if data.is_null() || len == 0 {
        return match std::ffi::CString::new("") {
            Ok(s) => s.into_raw(),
            Err(_) => std::ptr::null_mut(),
        };
    }
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    match std::ffi::CString::new(bytes_to_pg_hex_impl(bytes)) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

pub fn rust_is_related_items_query(pg_sql: *const c_char) -> c_int {
    if pg_sql.is_null() {
        return 0;
    }
    let pg_sql = match unsafe { CStr::from_ptr(pg_sql) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    i32::from(is_related_items_query_impl(pg_sql))
}

pub fn rust_should_mask_collection_metadata_type(
    pg_sql: *const c_char,
    col_name: *const c_char,
    raw_val: i64,
) -> c_int {
    if pg_sql.is_null() || col_name.is_null() {
        return 0;
    }
    let pg_sql = match unsafe { CStr::from_ptr(pg_sql) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let col_name = match unsafe { CStr::from_ptr(col_name) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    i32::from(should_mask_collection_metadata_type_impl(
        pg_sql, col_name, raw_val,
    ))
}

pub fn rust_find_insert_column_index(sql: *const c_char, column_name: *const c_char) -> c_int {
    if sql.is_null() || column_name.is_null() {
        return -1;
    }
    let sql = match unsafe { CStr::from_ptr(sql) }.to_str() {
        Ok(s) => s,
        Err(_) => return -1,
    };
    let column_name = match unsafe { CStr::from_ptr(column_name) }.to_str() {
        Ok(s) => s,
        Err(_) => return -1,
    };
    find_insert_column_index_impl(sql, column_name)
}

pub fn rust_pg_oid_to_sqlite_type(oid: c_uint) -> c_int {
    pg_oid_to_sqlite_type_impl(oid)
}

pub fn rust_pg_text_to_int(value: *const c_char) -> c_int {
    pg_text_to_int_impl(value)
}

pub fn rust_pg_text_to_int64(value: *const c_char) -> i64 {
    pg_text_to_int64_impl(value)
}

pub fn rust_pg_text_to_double(value: *const c_char) -> f64 {
    pg_text_to_double_impl(value)
}

#[cfg(test)]
#[path = "db_interpose_helpers_tests.rs"]
mod db_interpose_helpers_tests;
