use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};

use crate::byte_utils::contains_icase_bytes;
use crate::db_interpose_common::{
    get_orig_sqlite3_bind_parameter_count, get_orig_sqlite3_bind_parameter_index,
    get_orig_sqlite3_bind_parameter_name, get_orig_sqlite3_create_collation,
    get_orig_sqlite3_create_collation_v2, get_orig_sqlite3_db_handle, get_orig_sqlite3_errcode,
    get_orig_sqlite3_errmsg, get_orig_sqlite3_expanded_sql, get_orig_sqlite3_extended_errcode,
    get_orig_sqlite3_changes, get_orig_sqlite3_changes64, get_orig_sqlite3_last_insert_rowid,
    get_orig_sqlite3_free, get_orig_sqlite3_get_table, get_orig_sqlite3_malloc,
    get_orig_sqlite3_sql, get_orig_sqlite3_stmt_busy, get_orig_sqlite3_stmt_readonly,
    get_orig_sqlite3_stmt_status, get_shim_sqlite3_errcode, get_shim_sqlite3_errmsg,
    tls_in_interpose_call_ptr,
};
use crate::db_interpose_conn_utils::{cstr_to_string_or, log_debug, PthreadMutexGuard};
use crate::ffi_types::{sqlite3, sqlite3_stmt, PgStmt};

mod collation_alloc;
mod connection_state;
mod stmt_metadata;

use collation_alloc::{create_collation_impl, create_collation_v2_impl, free_impl, malloc_impl};
use connection_state::{
    changes64_impl, changes_impl, errcode_impl, errmsg_impl, extended_errcode_impl, get_table_impl,
    last_insert_rowid_impl,
};
use stmt_metadata::{
    bind_parameter_count_impl, bind_parameter_index_impl, bind_parameter_name_impl, db_handle_impl,
    expanded_sql_impl, sql_impl, stmt_busy_impl, stmt_readonly_impl, stmt_status_impl,
};

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;

const PGRES_TUPLES_OK: c_int = 2;
const PGRES_FATAL_ERROR: c_int = 7;

static NOT_AN_ERROR: &[u8] = b"not an error\0";

type CollationCompare =
    Option<unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int>;
type CollationDestroy = Option<unsafe extern "C" fn(*mut c_void)>;

#[repr(C)]
struct SqlTranslation {
    sql: *mut c_char,
    param_names: *mut *mut c_char,
    param_count: c_int,
    success: c_int,
    error: [c_char; 256],
}

extern "C" {
    fn sql_translate(sql: *const c_char) -> SqlTranslation;
    fn sql_translation_free(result: *mut SqlTranslation);
}

struct InterposeGuard;

impl InterposeGuard {
    fn try_enter() -> Option<Self> {
        unsafe {
            let flag = tls_in_interpose_call_ptr();
            if *flag != 0 {
                return None;
            }
            *flag = 1;
            Some(InterposeGuard)
        }
    }
}

impl Drop for InterposeGuard {
    fn drop(&mut self) {
        unsafe {
            *tls_in_interpose_call_ptr() = 0;
        }
    }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_changes(db: *mut sqlite3) -> c_int {
    changes_impl(db)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_changes64(db: *mut sqlite3) -> i64 {
    changes64_impl(db)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_last_insert_rowid(db: *mut sqlite3) -> i64 {
    last_insert_rowid_impl(db)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_errmsg(db: *mut sqlite3) -> *const c_char {
    errmsg_impl(db)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_errcode(db: *mut sqlite3) -> c_int {
    errcode_impl(db)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_extended_errcode(db: *mut sqlite3) -> c_int {
    extended_errcode_impl(db)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_get_table(
    db: *mut sqlite3,
    sql: *const c_char,
    paz_result: *mut *mut *mut c_char,
    pn_row: *mut c_int,
    pn_column: *mut c_int,
    pz_err_msg: *mut *mut c_char,
) -> c_int {
    get_table_impl(db, sql, paz_result, pn_row, pn_column, pz_err_msg)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_create_collation(
    db: *mut sqlite3,
    name: *const c_char,
    text_rep: c_int,
    arg: *mut c_void,
    compare: CollationCompare,
) -> c_int {
    create_collation_impl(db, name, text_rep, arg, compare)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_create_collation_v2(
    db: *mut sqlite3,
    name: *const c_char,
    text_rep: c_int,
    arg: *mut c_void,
    compare: CollationCompare,
    destroy: CollationDestroy,
) -> c_int {
    create_collation_v2_impl(db, name, text_rep, arg, compare, destroy)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_free(ptr: *mut c_void) {
    free_impl(ptr)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_malloc(n: c_int) -> *mut c_void {
    malloc_impl(n)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_db_handle(p_stmt: *mut sqlite3_stmt) -> *mut sqlite3 {
    db_handle_impl(p_stmt)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_sql(p_stmt: *mut sqlite3_stmt) -> *const c_char {
    sql_impl(p_stmt)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_parameter_count(p_stmt: *mut sqlite3_stmt) -> c_int {
    bind_parameter_count_impl(p_stmt)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_stmt_readonly(p_stmt: *mut sqlite3_stmt) -> c_int {
    stmt_readonly_impl(p_stmt)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_stmt_busy(p_stmt: *mut sqlite3_stmt) -> c_int {
    stmt_busy_impl(p_stmt)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_stmt_status(
    p_stmt: *mut sqlite3_stmt,
    op: c_int,
    reset: c_int,
) -> c_int {
    stmt_status_impl(p_stmt, op, reset)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_parameter_name(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
) -> *const c_char {
    bind_parameter_name_impl(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_parameter_index(
    p_stmt: *mut sqlite3_stmt,
    name: *const c_char,
) -> c_int {
    bind_parameter_index_impl(p_stmt, name)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_expanded_sql(p_stmt: *mut sqlite3_stmt) -> *mut c_char {
    expanded_sql_impl(p_stmt)
}
