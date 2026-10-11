use std::cell::Cell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

use crate::db_interpose_common::{tls_in_interpose_call_ptr, tls_prepare_v2_depth_ptr};
use crate::db_interpose_conn_utils::{cstr_prefix, cstr_to_string_or, log_error, log_info};
use crate::db_interpose_prepare_utils::{
    contains_ascii_icase, contains_icase_ptr, starts_with_ascii_icase,
};
use crate::ffi_types::{sqlite3, sqlite3_stmt, PgConnection, PgStmt};

mod internal_flow;
mod pg_route;
mod pg_stmt_setup;
mod sqlite_path;
mod sqlite_schema;
mod stack_guard;
mod stack_paths;
mod support;
mod wrappers;

use internal_flow::prepare_v2_internal_impl;
use pg_route::{describe_before_publication, maybe_register_pg_stmt, should_use_dummy_shadow};
use pg_stmt_setup::{apply_prepared_stmt_settings, copy_param_names};
use sqlite_path::{prepare_dummy_shadow_stmt, prepare_real_sqlite_stmt};
use sqlite_schema::maybe_skip_alter_table_add;
use stack_guard::{log_stack_info, PrepareDepthGuard};
use stack_paths::{
    maybe_delegate_prepare_to_worker, maybe_handle_low_stack_prepare_path,
    maybe_handle_ondeck_low_stack,
};
use support::{
    detect_query_loop, is_txn_control_sql, prepared_statements_disabled,
    should_bypass_worker_delegation, trace_prepare_pgsql_if_enabled, trace_prepare_sql_ok,
};
use wrappers::{prepare16_v2_impl, prepare_impl, prepare_v2_impl, prepare_v3_impl};

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;
const SQLITE_ROW: c_int = 100;
const SQLITE_NOMEM: c_int = 7;

const WORKER_DELEGATION_THRESHOLD: isize = 400_000;

static TXN_ROUTE_TOTAL: AtomicU64 = AtomicU64::new(0);
static TXN_ROUTE_SKIPPED: AtomicU64 = AtomicU64::new(0);
static TXN_ROUTE_PG: AtomicU64 = AtomicU64::new(0);

static DISABLE_PREPARED_CACHED: AtomicI32 = AtomicI32::new(-1);

thread_local! {
    static STACK_LOG_COUNTER: Cell<i32> = Cell::new(0);
    static QUERY_LOOP_LOG_COUNTER: Cell<i32> = Cell::new(0);
}

#[repr(C)]
struct SqlTranslation {
    sql: *mut c_char,
    param_names: *mut *mut c_char,
    param_count: c_int,
    success: c_int,
    error: [c_char; 256],
}

use crate::pg_statement::c_abi::{pg_register_stmt, pg_stmt_create};

extern "C" {
    static mut worker_running: c_int;

    static mut shim_sqlite3_prepare_v2: Option<
        unsafe extern "C" fn(
            *mut sqlite3,
            *const c_char,
            c_int,
            *mut *mut sqlite3_stmt,
            *mut *const c_char,
        ) -> c_int,
    >;

    static mut orig_sqlite3_step: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int>;
    static mut orig_sqlite3_column_text:
        Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> *const u8>;
    static mut orig_sqlite3_finalize: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int>;
    static mut orig_sqlite3_errmsg: Option<unsafe extern "C" fn(*mut sqlite3) -> *const c_char>;
    static mut orig_sqlite3_errcode: Option<unsafe extern "C" fn(*mut sqlite3) -> c_int>;
    static mut orig_sqlite3_prepare16_v2: Option<
        unsafe extern "C" fn(
            *mut sqlite3,
            *const c_void,
            c_int,
            *mut *mut sqlite3_stmt,
            *mut *const c_void,
        ) -> c_int,
    >;

    fn ensure_real_sqlite_loaded();
    fn delegate_prepare_to_worker(
        db: *mut sqlite3,
        sql: *const c_char,
        n: c_int,
        stmt: *mut *mut sqlite3_stmt,
        tail: *mut *const c_char,
    ) -> c_int;

    fn pg_exception_note_phase(
        phase: *const c_char,
        sql: *const c_char,
        stmt: *mut sqlite3_stmt,
        db: *mut sqlite3,
    );
    fn pg_exception_note_query(sql: *const c_char);

    fn pg_note_stmt_prepare(stmt: *mut sqlite3_stmt, sql: *const c_char);
    fn rewrite_blobs_schema_migrations(sql: *const c_char, db_path: *const c_char) -> *mut c_char;
    fn pg_hash_sql(sql: *const c_char) -> u64;

    fn sql_translate(sql: *const c_char) -> SqlTranslation;
    fn sql_translation_free(result: *mut SqlTranslation);
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_prepare_v2_internal(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
    from_worker: c_int,
) -> c_int {
    prepare_v2_internal_impl(db, z_sql, n_byte, pp_stmt, pz_tail, from_worker)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_prepare_v2(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    prepare_v2_impl(db, z_sql, n_byte, pp_stmt, pz_tail)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_prepare(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    prepare_impl(db, z_sql, n_byte, pp_stmt, pz_tail)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_prepare16_v2(
    db: *mut sqlite3,
    z_sql: *const c_void,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_void,
) -> c_int {
    prepare16_v2_impl(db, z_sql, n_byte, pp_stmt, pz_tail)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_prepare_v3(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    prep_flags: u32,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    prepare_v3_impl(db, z_sql, n_byte, prep_flags, pp_stmt, pz_tail)
}

#[cfg(test)]
mod tests {
    use super::should_bypass_worker_delegation;
    use std::ffi::CString;

    #[test]
    fn worker_delegation_bypass_matches_skip_policy_for_pragma() {
        let sql = CString::new(" PRAGMA journal_mode=WAL").unwrap();
        assert!(should_bypass_worker_delegation(sql.as_ptr()));
    }

    #[test]
    fn worker_delegation_does_not_bypass_passthrough_sql() {
        // fts3_tokenizer is sqlite-passthrough (not skip-SQL), so worker
        // delegation should NOT be bypassed — it needs real SQLite prepare.
        let sql = CString::new("SELECT fts3_tokenizer(?, ?)").unwrap();
        assert!(!should_bypass_worker_delegation(sql.as_ptr()));
    }

    #[test]
    fn worker_delegation_bypass_keeps_normal_selects_enabled() {
        let sql = CString::new("SELECT 1").unwrap();
        assert!(!should_bypass_worker_delegation(sql.as_ptr()));
    }
}
