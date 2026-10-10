use std::cell::{Cell, RefCell};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_uchar, c_uint, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once};

#[path = "db_interpose_column_badcast.rs"]
mod badcast;
mod binary_accessors;
mod blob_cache;
mod decltype_accessor;
#[path = "db_interpose_column_decltype.rs"]
mod decltype_cache;
mod fake_value_slot;
mod metadata_accessors;
mod metadata_support;
mod resolve_tables;
mod scalar_accessors;
mod support;
mod text_accessor;
mod type_accessor;
mod value_accessor;

use crate::db_interpose_common::{
    get_orig_sqlite3_column_blob, get_orig_sqlite3_column_bytes, get_orig_sqlite3_column_count,
    get_orig_sqlite3_column_decltype, get_orig_sqlite3_column_double, get_orig_sqlite3_column_int,
    get_orig_sqlite3_column_int64, get_orig_sqlite3_column_name, get_orig_sqlite3_column_text,
    get_orig_sqlite3_column_type, get_orig_sqlite3_column_value, get_orig_sqlite3_data_count,
    get_orig_sqlite3_db_handle, tls_column_type_calls_ptr, tls_in_resolve_tables_ptr,
    tls_last_query_ptr, PgFakeValue, GLOBAL_COLUMN_TYPE_CALLS, MAX_FAKE_VALUES,
    PG_FAKE_VALUE_MAGIC,
};
use crate::db_interpose_conn_utils::{
    cstr_prefix, cstr_to_string_or, log_debug, log_error, log_info, PthreadMutexGuard,
};
use crate::db_interpose_helpers::PGresult as PgResultHelpers;
use crate::db_interpose_trace_helpers::{list_any_token_in_haystack, list_contains_idx};
use crate::db_interpose_value_helpers::{
    pg_oid_to_sqlite_type_impl, pg_text_to_double_impl, pg_text_to_int64_impl, pg_text_to_int_impl,
};
use crate::env_utils;
use crate::ffi_types::{sqlite3, sqlite3_stmt, sqlite3_value, PgConnection, PgStmt};
use crate::libpq_helpers::PGresult as PgResultLibpq;
use badcast::{trace_badcast_log_ctx, trace_badcast_should_log, trace_badcast_should_log_col};
use binary_accessors::{column_blob_impl, column_bytes_impl};
use blob_cache::pg_decode_bytea_cached_impl;
pub use blob_cache::rust_pg_decode_bytea_cached;
use decltype_accessor::column_decltype_impl;
use decltype_cache::{lookup_decltype_direct, lookup_sqlite_decltype};
use fake_value_slot::allocate_fake_sqlite_value;
use metadata_accessors::{column_count_impl, column_name_impl, data_count_impl};
pub(crate) use metadata_support::ensure_pg_result_for_metadata;
use metadata_support::mask_collection_metadata_type;
#[cfg(test)]
use metadata_support::set_metadata_result_state;
use resolve_tables::resolve_column_tables_impl;
use scalar_accessors::{column_double_impl, column_int64_impl, column_int_impl};
use support::{
    helpers_result_ptr, next_text_buffer_index, sqlite_type_name, validate_type_consistency,
};
use text_accessor::column_text_impl;
use type_accessor::column_type_impl;
use value_accessor::column_value_impl;

const SQLITE_INTEGER: c_int = 1;
const SQLITE_FLOAT: c_int = 2;
const SQLITE_TEXT: c_int = 3;
const SQLITE_BLOB: c_int = 4;
const SQLITE_NULL: c_int = 5;

const PGRES_COMMAND_OK: c_int = 1;
const PGRES_TUPLES_OK: c_int = 2;

const DECLTYPE_MAX_KEY_LEN: usize = 128;
const NUM_TEXT_BUFFERS: usize = 64;
const TEXT_BUFFER_SIZE: usize = 8192;

const INVALID_OID: u32 = 0;
const PG_DECLTYPE_CASE_NULL: c_int = 1;
const PG_DECLTYPE_CASE_DT_INTEGER_8: c_int = 2;

const PMT_COLUMN_CACHED_BLOB_ALLOC: c_int = 3;
const PMT_COLUMN_DECODED_BLOB_ALLOC: c_int = 4;

static DECLTYPE_TEXT: &[u8] = b"TEXT\0";
static DECLTYPE_DT_INTEGER_8: &[u8] = b"dt_integer(8)\0";
static _DECLTYPE_INTEGER: &[u8] = b"INTEGER\0";
static _DECLTYPE_BIGINT: &[u8] = b"BIGINT\0";
static _NEEDLE_TYPE: &[u8] = b"type\0";
static _NEEDLE_METADATA_TYPE: &[u8] = b"metadata_type\0";

thread_local! {
    // Use vec![].into_boxed_slice() to allocate on the heap directly.
    // Box::new([T; 64*8192]) would place 512KB on the stack, exceeding
    // Plex's 544K worker thread stacks.
    static COLUMN_TEXT_BUFFERS: RefCell<Box<[[u8; TEXT_BUFFER_SIZE]]>> =
        RefCell::new(vec![[0u8; TEXT_BUFFER_SIZE]; NUM_TEXT_BUFFERS].into_boxed_slice());
    static COLUMN_TEXT_BUF_IDX: Cell<usize> = Cell::new(0);
}

use crate::pg_statement::c_abi::pg_find_any_stmt;

extern "C" {
    fn pg_get_thread_connection(db_path: *const c_char) -> *mut PgConnection;
    fn pg_get_thread_connection_excluding(
        db_path: *const c_char,
        exclude_conn: *const c_void,
    ) -> *mut PgConnection;
    fn pg_exception_note_phase(
        phase: *const c_char,
        sql: *const c_char,
        stmt: *mut sqlite3_stmt,
        db: *mut sqlite3,
    );
}

#[no_mangle]
pub extern "C" fn rust_resolve_column_tables(
    pg_stmt: *mut PgStmt,
    pg_conn: *mut PgConnection,
) -> c_int {
    resolve_column_tables_impl(pg_stmt, pg_conn)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_count(p_stmt: *mut sqlite3_stmt) -> c_int {
    column_count_impl(p_stmt)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_type(p_stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    column_type_impl(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_int(p_stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    column_int_impl(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_int64(p_stmt: *mut sqlite3_stmt, idx: c_int) -> i64 {
    column_int64_impl(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_double(p_stmt: *mut sqlite3_stmt, idx: c_int) -> f64 {
    column_double_impl(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_text(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
) -> *const c_uchar {
    column_text_impl(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_blob(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
) -> *const c_void {
    column_blob_impl(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_bytes(p_stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    column_bytes_impl(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_name(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
) -> *const c_char {
    column_name_impl(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_decltype(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
) -> *const c_char {
    column_decltype_impl(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_value(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
) -> *mut sqlite3_value {
    column_value_impl(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_data_count(p_stmt: *mut sqlite3_stmt) -> c_int {
    data_count_impl(p_stmt)
}

#[cfg(test)]
mod tests {
    use super::{
        next_text_buffer_index, rust_my_sqlite3_column_count, rust_my_sqlite3_column_decltype,
        set_metadata_result_state, COLUMN_TEXT_BUF_IDX, NUM_TEXT_BUFFERS,
    };
    use crate::db_interpose_common::{orig_sqlite3_column_count, orig_sqlite3_column_decltype};
    use crate::ffi_types::{sqlite3_stmt, PgConnection, PgStmt};
    use crate::libpq_helpers::PGresult as PgResultLibpq;
    use crate::pg_statement::{
        rust_stmt_create, rust_stmt_free, rust_stmt_register, rust_stmt_unregister,
    };
    use std::collections::HashSet;
    use std::ffi::{CStr, CString};
    use std::os::raw::{c_char, c_int};

    fn reset_text_buffer_idx() {
        COLUMN_TEXT_BUF_IDX.with(|idx| idx.set(0));
    }

    fn make_stmt() -> *mut PgStmt {
        let sql = CString::new("SELECT 1").unwrap();
        let stmt = rust_stmt_create(std::ptr::null_mut(), sql.as_ptr(), std::ptr::null_mut());
        assert!(!stmt.is_null());
        stmt
    }

    #[test]
    fn column_text_buffer_wraps_after_num_buffers() {
        reset_text_buffer_idx();

        let mut indices = Vec::with_capacity(NUM_TEXT_BUFFERS);
        for _ in 0..NUM_TEXT_BUFFERS {
            indices.push(next_text_buffer_index());
        }

        let unique: HashSet<usize> = indices.iter().copied().collect();
        assert_eq!(unique.len(), NUM_TEXT_BUFFERS);

        let wrapped = next_text_buffer_index();
        assert_eq!(wrapped, 0);
    }

    #[test]
    fn column_text_buffer_thread_local_indices_start_at_zero() {
        reset_text_buffer_idx();
        assert_eq!(next_text_buffer_index(), 0);

        let child_first = std::thread::spawn(|| {
            COLUMN_TEXT_BUF_IDX.with(|idx| idx.set(0));
            next_text_buffer_index()
        })
        .join()
        .expect("thread should join");

        assert_eq!(child_first, 0);
    }

    #[test]
    fn metadata_result_state_assigns_result_owner() {
        let stmt = make_stmt();
        let result = 0x1234usize as *mut PgResultLibpq;
        let exec_conn = 0x5678usize as *mut PgConnection;

        unsafe {
            set_metadata_result_state(&mut *stmt, result, exec_conn, 0, 0);
        }
        let s = unsafe { &mut *stmt };
        assert_eq!(s.result, result);
        assert_eq!(s.result_conn, exec_conn);
        assert_eq!(s.metadata_only_result, 1);

        s.result = std::ptr::null_mut();
        s.result_conn = std::ptr::null_mut();

        rust_stmt_free(stmt);
    }

    extern "C" fn shadow_column_count_stub(_: *mut sqlite3_stmt) -> c_int {
        1
    }

    extern "C" fn shadow_column_decltype_stub(_: *mut sqlite3_stmt, _: c_int) -> *const c_char {
        c"INTEGER".as_ptr()
    }

    #[test]
    fn pg_column_count_without_metadata_does_not_fall_back_to_shadow_count() {
        let stmt = make_stmt();
        let shadow_stmt = 0x10000usize as *mut sqlite3_stmt;
        let pg_sql = CString::new(
            "SELECT plugins.*, plugin_prefixes.* FROM plugins \
             JOIN plugin_prefixes ON plugin_prefixes.plugin_id = plugins.id \
             WHERE plugin_prefixes.prefix = $1",
        )
        .unwrap();

        let old = unsafe { orig_sqlite3_column_count };
        unsafe {
            orig_sqlite3_column_count = Some(shadow_column_count_stub);
            let s = &mut *stmt;
            s.is_pg = 2;
            s.pg_sql = libc::strdup(pg_sql.as_ptr());
        }
        rust_stmt_register(shadow_stmt as usize, stmt as usize);

        let count = rust_my_sqlite3_column_count(shadow_stmt);
        assert_eq!(count, 0);

        rust_stmt_unregister(shadow_stmt as usize);
        unsafe {
            orig_sqlite3_column_count = old;
        }
        rust_stmt_free(stmt);
    }

    #[test]
    fn pg_column_decltype_without_metadata_does_not_use_shadow_decltype() {
        let stmt = make_stmt();
        let shadow_stmt = 0x20000usize as *mut sqlite3_stmt;
        let pg_sql = CString::new(
            "SELECT plugins.*, plugin_prefixes.* FROM plugins \
             JOIN plugin_prefixes ON plugin_prefixes.plugin_id = plugins.id \
             WHERE plugin_prefixes.prefix = $1",
        )
        .unwrap();

        let old = unsafe { orig_sqlite3_column_decltype };
        unsafe {
            orig_sqlite3_column_decltype = Some(shadow_column_decltype_stub);
            let s = &mut *stmt;
            s.is_pg = 2;
            s.pg_sql = libc::strdup(pg_sql.as_ptr());
        }
        rust_stmt_register(shadow_stmt as usize, stmt as usize);

        let decl = rust_my_sqlite3_column_decltype(shadow_stmt, 0);
        let decl = unsafe { CStr::from_ptr(decl) }.to_str().unwrap();
        assert_eq!(decl, "TEXT");

        rust_stmt_unregister(shadow_stmt as usize);
        unsafe {
            orig_sqlite3_column_decltype = old;
        }
        rust_stmt_free(stmt);
    }
}
