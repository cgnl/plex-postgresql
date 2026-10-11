use std::os::raw::{c_char, c_int};

use crate::ffi_types::sqlite3_stmt;

mod ring_tracker;
mod statement_ops;

#[cfg(test)]
use ring_tracker::{remember_finalized_stmt, reset_test_state};
use statement_ops::{clear_bindings_impl, finalize_impl, note_stmt_prepare_impl, reset_impl};

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;

use crate::pg_statement::c_abi::{
    pg_clear_cached_stmt, pg_find_any_stmt, pg_find_cached_stmt, pg_find_stmt,
    pg_stmt_clear_result, pg_stmt_unref, pg_unregister_stmt,
};

extern "C" {
    static mut orig_sqlite3_reset: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int>;
    static mut orig_sqlite3_finalize: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int>;
    static mut orig_sqlite3_clear_bindings:
        Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int>;
    static mut orig_sqlite3_sql: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> *const c_char>;

    fn platform_print_backtrace(reason: *const c_char, skip_frames: c_int);
}

#[no_mangle]
pub extern "C" fn rust_pg_note_stmt_prepare(p_stmt: *mut sqlite3_stmt, sql: *const c_char) {
    note_stmt_prepare_impl(p_stmt, sql)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_reset(p_stmt: *mut sqlite3_stmt) -> c_int {
    reset_impl(p_stmt)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_finalize(p_stmt: *mut sqlite3_stmt) -> c_int {
    finalize_impl(p_stmt)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_clear_bindings(p_stmt: *mut sqlite3_stmt) -> c_int {
    clear_bindings_impl(p_stmt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::sync::{LazyLock, Mutex};

    static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
    static FINALIZE_CALLS: AtomicI32 = AtomicI32::new(0);

    unsafe extern "C" fn fake_finalize(_stmt: *mut sqlite3_stmt) -> c_int {
        FINALIZE_CALLS.fetch_add(1, Ordering::AcqRel);
        SQLITE_OK
    }

    unsafe fn reset_finalize_test_state() {
        FINALIZE_CALLS.store(0, Ordering::Relaxed);
        reset_test_state();
    }

    #[test]
    fn finalize_unknown_stmt_still_calls_original_finalize() {
        let _guard = TEST_LOCK.lock().unwrap();
        unsafe {
            reset_finalize_test_state();
            let old_finalize = crate::db_interpose_common::orig_sqlite3_finalize;
            let old_sql = crate::db_interpose_common::orig_sqlite3_sql;
            crate::db_interpose_common::orig_sqlite3_finalize = Some(fake_finalize);
            crate::db_interpose_common::orig_sqlite3_sql = None;

            let stmt = 0x1234usize as *mut sqlite3_stmt;
            let rc = rust_my_sqlite3_finalize(stmt);

            assert_eq!(rc, SQLITE_OK);
            assert_eq!(FINALIZE_CALLS.load(Ordering::Acquire), 1);

            crate::db_interpose_common::orig_sqlite3_finalize = old_finalize;
            crate::db_interpose_common::orig_sqlite3_sql = old_sql;
            reset_finalize_test_state();
        }
    }

    #[test]
    fn finalize_pg_read_stmt_finalizes_its_shadow_statement() {
        let _guard = TEST_LOCK.lock().unwrap();
        unsafe {
            reset_finalize_test_state();
            let old_finalize = crate::db_interpose_common::orig_sqlite3_finalize;
            let old_sql = crate::db_interpose_common::orig_sqlite3_sql;
            crate::db_interpose_common::orig_sqlite3_finalize = Some(fake_finalize);
            crate::db_interpose_common::orig_sqlite3_sql = None;

            let pg_stmt = Box::into_raw(Box::new(crate::ffi_types::PgStmt::new()));
            (*pg_stmt).is_pg = 2;
            let stmt = 0x9abcusize as *mut sqlite3_stmt;
            crate::pg_statement::rust_stmt_register(stmt as usize, pg_stmt as usize);
            let rc = rust_my_sqlite3_finalize(stmt);

            assert_eq!(rc, SQLITE_OK);
            assert_eq!(FINALIZE_CALLS.load(Ordering::Acquire), 1);

            crate::db_interpose_common::orig_sqlite3_finalize = old_finalize;
            crate::db_interpose_common::orig_sqlite3_sql = old_sql;
            reset_finalize_test_state();
        }
    }

    #[test]
    fn finalize_recently_finalized_stmt_skips_original_finalize() {
        let _guard = TEST_LOCK.lock().unwrap();
        unsafe {
            reset_finalize_test_state();
            let old_finalize = crate::db_interpose_common::orig_sqlite3_finalize;
            let old_sql = crate::db_interpose_common::orig_sqlite3_sql;
            crate::db_interpose_common::orig_sqlite3_finalize = Some(fake_finalize);
            crate::db_interpose_common::orig_sqlite3_sql = None;

            let stmt = 0x5678usize as *mut sqlite3_stmt;
            remember_finalized_stmt(stmt, ptr::null(), 0);
            let rc = rust_my_sqlite3_finalize(stmt);

            assert_eq!(rc, SQLITE_OK);
            assert_eq!(FINALIZE_CALLS.load(Ordering::Acquire), 0);

            crate::db_interpose_common::orig_sqlite3_finalize = old_finalize;
            crate::db_interpose_common::orig_sqlite3_sql = old_sql;
            reset_finalize_test_state();
        }
    }
}
