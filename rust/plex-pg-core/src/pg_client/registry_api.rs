use std::os::raw::c_void;

use crate::ffi_types::{sqlite3, PgConnection};

use super::config::env_nonzero;
use super::connection_helpers::{conn_db_path, conn_is_pg_active};
use super::pool_lookup::is_library_db;
use super::{pool, rust_find_any_library_connection, rust_find_registered_connection};

/// Register a non-pooled connection using its shadow DB handle.
#[no_mangle]
pub extern "C" fn rust_pg_register_connection(conn_ptr: *mut PgConnection) {
    if conn_ptr.is_null() {
        return;
    }
    // SAFETY: conn_ptr is non-null after the check above.
    let conn = unsafe { &*conn_ptr };
    let db = conn.shadow_db;
    if db.is_null() {
        return;
    }
    pool().registry.register(db as usize, conn_ptr as usize);
}

/// Unregister a non-pooled connection using its shadow DB handle.
#[no_mangle]
pub extern "C" fn rust_pg_unregister_connection(conn_ptr: *mut PgConnection) {
    if conn_ptr.is_null() {
        return;
    }
    // SAFETY: conn_ptr is non-null after the check above.
    let conn = unsafe { &*conn_ptr };
    let db = conn.shadow_db;
    if db.is_null() {
        return;
    }
    pool().registry.unregister(db as usize);
    super::transaction::forget_connection(conn_ptr);
}

/// Find the handle connection for a sqlite3* handle.
#[no_mangle]
pub extern "C" fn rust_pg_find_handle_connection(db_handle: *const sqlite3) -> *mut PgConnection {
    if db_handle.is_null() {
        return std::ptr::null_mut();
    }
    rust_find_registered_connection(db_handle as *const c_void) as *mut PgConnection
}

/// Find the active connection for a sqlite3* handle, including pool logic.
#[no_mangle]
pub extern "C" fn rust_pg_find_connection(db_handle: *const sqlite3) -> *mut PgConnection {
    if db_handle.is_null() {
        return std::ptr::null_mut();
    }

    let _ = super::rust_pool_check_fork();

    let handle_conn =
        rust_find_registered_connection(db_handle as *const c_void) as *mut PgConnection;
    if handle_conn.is_null() {
        return std::ptr::null_mut();
    }

    let handle_ref = unsafe { &*handle_conn };
    let path = conn_db_path(handle_ref);

    if is_library_db(&path) {
        if env_nonzero("PLEX_PG_FORCE_SQLITE_LIBRARY") {
            return std::ptr::null_mut();
        }

        if conn_is_pg_active(handle_ref) {
            super::transaction::ensure_handle_session(handle_conn);
            return handle_conn;
        }
        return std::ptr::null_mut();
    }

    if conn_is_pg_active(handle_ref) {
        handle_conn
    } else {
        std::ptr::null_mut()
    }
}

/// Find any active library connection (pool or handle).
#[no_mangle]
pub extern "C" fn rust_pg_find_any_library_connection() -> *mut PgConnection {
    rust_find_any_library_connection() as *mut PgConnection
}
