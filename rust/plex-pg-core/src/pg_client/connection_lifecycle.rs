use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};

use crate::db_interpose_conn_utils::{log_error, log_info, PthreadMutexGuard};
use crate::db_interpose_helpers::cstr_to_str_or_empty;
use crate::ffi_types::{sqlite3, PgConnection};
use crate::libpq_helpers::{
    rust_pq_connectdb, rust_pq_error_message, rust_pq_finish, rust_pq_reset, rust_pq_status,
    rust_pq_transaction_status, PGconn,
};

use super::session::{apply_session_settings, exec_tuples, pg_set_socket_timeout, CONNECTION_OK};
use super::{
    conn_config, is_library_db, pool, rust_stmt_cache_clear, rust_stmt_cache_drop,
    write_str_to_cbuf, ConnConfig,
};
use crate::log_debug_lazy;
use crate::log_info_lazy;

fn conn_error_string(pg_conn: *mut PGconn) -> String {
    if pg_conn.is_null() {
        return "<null connection>".to_string();
    }

    let msg = rust_pq_error_message(pg_conn);
    if msg.is_null() {
        "<null>".to_string()
    } else {
        unsafe { CStr::from_ptr(msg).to_string_lossy().into_owned() }
    }
}

pub(super) fn build_conninfo(cfg: &ConnConfig, with_keepalives: bool) -> String {
    if with_keepalives {
        format!(
            "host={} port={} dbname={} user={} password={} \
             connect_timeout=5 keepalives=1 keepalives_idle=30 \
             keepalives_interval=10 keepalives_count=3",
            cfg.host, cfg.port, cfg.database, cfg.user, cfg.password
        )
    } else {
        format!(
            "host={} port={} dbname={} user={} password={} connect_timeout=5",
            cfg.host, cfg.port, cfg.database, cfg.user, cfg.password
        )
    }
}

pub(super) fn create_connection_struct(
    db_path: &str,
    shadow_db: *mut sqlite3,
) -> *mut PgConnection {
    unsafe {
        let conn_ptr = libc::calloc(1, std::mem::size_of::<PgConnection>()) as *mut PgConnection;
        if conn_ptr.is_null() {
            log_error("Failed to allocate pg_connection_t");
            return std::ptr::null_mut();
        }
        // Use PTHREAD_MUTEX_RECURSIVE to match PgStmt.mutex and prevent
        // self-deadlock when ensure_pg_result_for_metadata calls
        // resolve_column_tables_impl on the same connection.
        let mut attr: libc::pthread_mutexattr_t = std::mem::zeroed();
        if libc::pthread_mutexattr_init(&mut attr as *mut _) != 0 {
            log_error("pthread_mutexattr_init failed for pg_connection_t");
            libc::free(conn_ptr as *mut libc::c_void);
            return std::ptr::null_mut();
        }
        libc::pthread_mutexattr_settype(&mut attr as *mut _, libc::PTHREAD_MUTEX_RECURSIVE);
        let cp = &mut *conn_ptr;
        if libc::pthread_mutex_init(&mut cp.mutex as *mut _, &attr as *const _) != 0 {
            log_error("pthread_mutex_init failed for pg_connection_t");
            libc::pthread_mutexattr_destroy(&mut attr as *mut _);
            libc::free(conn_ptr as *mut libc::c_void);
            return std::ptr::null_mut();
        }
        libc::pthread_mutexattr_destroy(&mut attr as *mut _);
        cp.shadow_db = shadow_db;
        write_str_to_cbuf(&mut cp.db_path, db_path);
        conn_ptr
    }
}

pub(super) fn destroy_connection_struct(conn: *mut PgConnection) {
    if conn.is_null() {
        return;
    }
    unsafe {
        let c = &mut *conn;
        libc::pthread_mutex_destroy(&mut c.mutex as *mut _);
        libc::free(conn as *mut libc::c_void);
    }
}

pub(super) fn create_pool_connection(db_path: *const c_char) -> *mut c_void {
    let cfg = conn_config();
    log_debug_lazy!(
        "create_pool_connection: host='{}' port={} db='{}' user='{}' schema='{}'",
        cfg.host,
        cfg.port,
        cfg.database,
        cfg.user,
        cfg.schema
    );

    if cfg.host.is_empty() || cfg.port == 0 {
        log_error(&format!(
            "Pool connection skipped: config not loaded (host='{}' port={}). Check PLEX_PG_HOST/PLEX_PG_PORT env vars.",
            cfg.host, cfg.port
        ));
        return std::ptr::null_mut();
    }

    let db_path_str = if db_path.is_null() {
        ""
    } else {
        unsafe { cstr_to_str_or_empty(db_path) }
    };

    let conn_ptr = create_connection_struct(db_path_str, std::ptr::null_mut());
    if conn_ptr.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: conn_ptr was just allocated and is non-null.
    let conn = unsafe { &mut *conn_ptr };

    let conninfo = build_conninfo(cfg, true);
    let conninfo_c = match CString::new(conninfo) {
        Ok(s) => s,
        Err(_) => {
            destroy_connection_struct(conn_ptr);
            return std::ptr::null_mut();
        }
    };

    conn.conn = rust_pq_connectdb(conninfo_c.as_ptr());
    if rust_pq_status(conn.conn) != CONNECTION_OK {
        log_error(&format!(
            "Pool connection failed: {}",
            conn_error_string(conn.conn)
        ));
        if !conn.conn.is_null() {
            rust_pq_finish(conn.conn);
        }
        conn.conn = std::ptr::null_mut();
    } else {
        pg_set_socket_timeout(conn.conn);
        apply_session_settings(conn.conn, &cfg.schema, true);
        conn.is_pg_active = 1;
        // Only publish the pointer after libpq connect + session setup has
        // succeeded. During startup Plex opens many DB handles in parallel;
        // exposing a half-initialized pool connection widens races for
        // cleanup/invariant code that only needs to reason about fully
        // usable live connections.
        pool().note_live_pool_connection(conn_ptr as *const c_void);
    }

    conn_ptr as *mut c_void
}

pub(super) fn destroy_pool_connection(conn: *mut c_void) {
    let conn_ptr = conn as *mut PgConnection;
    if conn_ptr.is_null() {
        return;
    }
    pool().forget_live_pool_connection(conn_ptr as *const c_void);
    rust_stmt_cache_drop(conn_ptr as *mut c_void);
    super::transaction::forget_connection(conn_ptr);
    // SAFETY: conn_ptr is non-null after the check above.
    let conn = unsafe { &mut *conn_ptr };
    if !conn.conn.is_null() {
        rust_pq_finish(conn.conn);
    }
    destroy_connection_struct(conn_ptr);
}

pub(super) fn check_conn_ok(conn: *mut c_void) -> bool {
    let conn_ptr = conn as *mut PgConnection;
    if conn_ptr.is_null() {
        return false;
    }
    // SAFETY: conn_ptr is non-null after the check above.
    let conn = unsafe { &*conn_ptr };
    !conn.conn.is_null() && rust_pq_status(conn.conn) == CONNECTION_OK
}

pub(super) fn reset_conn(conn: *mut c_void) -> bool {
    let conn_ptr = conn as *mut PgConnection;
    if conn_ptr.is_null() {
        return false;
    }
    // SAFETY: conn_ptr is non-null after the check above.
    let conn = unsafe { &mut *conn_ptr };
    if conn.conn.is_null() {
        return false;
    }
    rust_pq_reset(conn.conn);
    // Whatever transaction the old session had is gone with it.
    crate::descriptor_cache::connection_idle(conn_ptr as usize);
    if rust_pq_status(conn.conn) != CONNECTION_OK {
        return false;
    }
    pg_set_socket_timeout(conn.conn);
    let cfg = conn_config();
    apply_session_settings(conn.conn, &cfg.schema, false);
    true
}

/// Reset a session that was left unusable mid-command and restore what a
/// fresh session needs (search_path, timeouts); its prepared statements are
/// gone, so forget them too.
pub(crate) fn reset_session(conn: *mut PgConnection) -> bool {
    if conn.is_null() {
        return false;
    }
    rust_stmt_cache_clear(conn as *mut c_void);
    unsafe { (*conn).streaming_active.store(0, std::sync::atomic::Ordering::Release) };
    reset_conn(conn as *mut c_void)
}

pub(super) fn reconnect_conn(raw_conn: *mut c_void) -> bool {
    let conn_ptr = raw_conn as *mut PgConnection;
    if conn_ptr.is_null() {
        return false;
    }
    let cfg = conn_config();
    let conninfo = build_conninfo(cfg, true);
    let conninfo_c = match CString::new(conninfo) {
        Ok(s) => s,
        Err(_) => return false,
    };
    // SAFETY: conn_ptr is non-null after the check above.
    let conn = unsafe { &mut *conn_ptr };
    let _conn_guard = unsafe { PthreadMutexGuard::lock(&mut conn.mutex as *mut _) };
    if !conn.conn.is_null() {
        rust_pq_finish(conn.conn);
        conn.conn = std::ptr::null_mut();
    }

    let new_pg = rust_pq_connectdb(conninfo_c.as_ptr());
    if rust_pq_status(new_pg) == CONNECTION_OK {
        pg_set_socket_timeout(new_pg);
        apply_session_settings(new_pg, &cfg.schema, false);
        conn.conn = new_pg;
        conn.is_pg_active = 1;
        return true;
    }

    log_error(&format!(
        "Pool: reconnect failed: {}",
        conn_error_string(new_pg)
    ));
    if !new_pg.is_null() {
        rust_pq_finish(new_pg);
    }
    conn.conn = std::ptr::null_mut();
    conn.is_pg_active = 0;
    false
}

pub(super) fn get_txn_status(conn: *mut c_void) -> i32 {
    let conn_ptr = conn as *mut PgConnection;
    if conn_ptr.is_null() {
        return 0;
    }
    // SAFETY: conn_ptr is non-null after the check above.
    let conn = unsafe { &*conn_ptr };
    if conn.conn.is_null() {
        return 0;
    }
    rust_pq_transaction_status(conn.conn)
}

pub(super) fn close_handle_connection(conn_ptr: *mut PgConnection) {
    if conn_ptr.is_null() {
        return;
    }
    rust_stmt_cache_clear(conn_ptr as *mut c_void);
    rust_stmt_cache_drop(conn_ptr as *mut c_void);
    // A later connection may be allocated at this address; it must not
    // inherit this one's transaction state.
    super::transaction::forget_connection(conn_ptr);
    // SAFETY: conn_ptr is non-null after the check above.
    let conn = unsafe { &mut *conn_ptr };
    let _conn_guard = unsafe { PthreadMutexGuard::lock(&mut conn.mutex as *mut _) };
    if !conn.conn.is_null() {
        rust_pq_finish(conn.conn);
        conn.conn = std::ptr::null_mut();
    }
    drop(_conn_guard);
    destroy_connection_struct(conn_ptr);
}

#[no_mangle]
pub extern "C" fn rust_pg_connect(
    db_path: *const c_char,
    shadow_db: *mut sqlite3,
) -> *mut PgConnection {
    let db_path_str = if db_path.is_null() {
        ""
    } else {
        unsafe { cstr_to_str_or_empty(db_path) }
    };

    let conn_ptr = create_connection_struct(db_path_str, shadow_db);
    if conn_ptr.is_null() {
        return std::ptr::null_mut();
    }

    // SAFETY: conn_ptr was just allocated and is non-null.
    let conn = unsafe { &mut *conn_ptr };

    if is_library_db(db_path_str) {
        conn.conn = std::ptr::null_mut();
        conn.is_pg_active = 1;
        log_info_lazy!("PostgreSQL pool-only connection for: {}", db_path_str);
        return conn_ptr;
    }

    let cfg = conn_config();
    let conninfo = build_conninfo(cfg, false);
    let conninfo_c = match CString::new(conninfo) {
        Ok(s) => s,
        Err(_) => return conn_ptr,
    };

    conn.conn = rust_pq_connectdb(conninfo_c.as_ptr());
    if rust_pq_status(conn.conn) != CONNECTION_OK {
        log_error(&format!(
            "PostgreSQL connection failed: {}",
            conn_error_string(conn.conn)
        ));
        if !conn.conn.is_null() {
            rust_pq_finish(conn.conn);
        }
        conn.conn = std::ptr::null_mut();
    } else {
        log_info_lazy!("PostgreSQL connected for: {}", db_path_str);
        pg_set_socket_timeout(conn.conn);
        apply_session_settings(conn.conn, &cfg.schema, false);
        conn.is_pg_active = 1;
    }

    conn_ptr
}

#[no_mangle]
pub extern "C" fn rust_pg_ensure_connection(conn_ptr: *mut PgConnection) -> i32 {
    if conn_ptr.is_null() {
        return 0;
    }
    // SAFETY: conn_ptr is non-null after the check above. This is an FFI
    // entry point so we convert the raw pointer to a safe reference here.
    let conn = unsafe { &mut *conn_ptr };

    let mut conn_guard = unsafe { PthreadMutexGuard::lock(&mut conn.mutex as *mut _) };

    if !conn.conn.is_null() && rust_pq_status(conn.conn) == CONNECTION_OK {
        if exec_tuples(conn.conn, "SELECT 1") {
            unsafe { conn_guard.unlock() };
            return 1;
        }
        log_info("Connection health check failed, will reconnect");
    }

    if !conn.conn.is_null() {
        rust_pq_finish(conn.conn);
        conn.conn = std::ptr::null_mut();
    }

    let cfg = conn_config();
    let conninfo = build_conninfo(cfg, false);
    let conninfo_c = match CString::new(conninfo) {
        Ok(s) => s,
        Err(_) => {
            conn.is_pg_active = 0;
            unsafe { conn_guard.unlock() };
            return 0;
        }
    };

    conn.conn = rust_pq_connectdb(conninfo_c.as_ptr());
    if rust_pq_status(conn.conn) != CONNECTION_OK {
        log_error(&format!(
            "PostgreSQL reconnection failed: {}",
            conn_error_string(conn.conn)
        ));
        if !conn.conn.is_null() {
            rust_pq_finish(conn.conn);
        }
        conn.conn = std::ptr::null_mut();
        conn.is_pg_active = 0;
        unsafe { conn_guard.unlock() };
        return 0;
    }

    log_info("PostgreSQL reconnected successfully");
    pg_set_socket_timeout(conn.conn);
    apply_session_settings(conn.conn, &cfg.schema, false);
    conn.is_pg_active = 1;
    unsafe { conn_guard.unlock() };
    1
}

#[no_mangle]
pub extern "C" fn rust_pg_close(conn: *mut PgConnection) {
    close_handle_connection(conn);
}
