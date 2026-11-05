use std::ffi::CString;
use std::os::raw::{c_char, c_void};
use std::sync::atomic::Ordering;

use crate::env_utils;
use crate::ffi_types::PgConnection;
use crate::libpq_helpers::{rust_pg_align_idle_timeout_with_server, rust_pg_probe_max_connections};

use super::pool_lookup::find_any_library_connection;
use super::pool_runtime::{pool_check_health_inner, pool_release_for_db_inner};
use super::{
    close_handle_connection, conn_config, destroy_pool_connection, log_error,
    parse_positive_env_or_default, pool, pool_find_connection_for_db, pool_get_connection_inner,
    pool_get_connection_inner_excluding, CLIENT_INIT, POOL, POOL_SIZE_DEFAULT,
};
use crate::log_info_lazy;

#[no_mangle]
pub extern "C" fn rust_pool_init(pool_size: i32, pool_max: i32, idle_timeout: i32) {
    let requested_max = if pool_max > 0 {
        pool_max as usize
    } else {
        POOL_SIZE_DEFAULT
    };
    let requested_size = if pool_size > 0 {
        pool_size as usize
    } else {
        POOL_SIZE_DEFAULT
    };

    let pm = POOL.get_or_init(|| super::PoolManager::new(requested_size, requested_max));
    let slots_cap = pm.slots.len();
    let max_sz = requested_max.min(slots_cap).max(1);
    if requested_max > slots_cap {
        log_error(&format!(
            "Pool: requested max {} exceeds initialized slot capacity {}; clamping",
            requested_max, slots_cap
        ));
    }
    pm.configured_max_size.store(max_sz, Ordering::Relaxed);

    if requested_size <= max_sz {
        pm.configured_size.store(requested_size, Ordering::Relaxed);
    } else {
        pm.configured_size.store(max_sz, Ordering::Relaxed);
    }
    if idle_timeout >= 10 {
        pm.idle_timeout_secs
            .store(idle_timeout as u32, Ordering::Relaxed);
    }
    pm.init_pid.store(std::process::id(), Ordering::Release);
}

#[no_mangle]
pub extern "C" fn rust_pool_cleanup() {
    let pm = pool();
    let pool_size = pm.pool_size();

    for i in 0..pool_size {
        let slot = &pm.slots[i];
        slot.state.store(super::SLOT_FREE, Ordering::SeqCst);

        let conn = slot.conn.swap(std::ptr::null_mut(), Ordering::SeqCst);
        if !conn.is_null() {
            destroy_pool_connection(conn);
        }
        slot.owner_thread.store(0, Ordering::Release);
        slot.generation.store(0, Ordering::Release);
    }

    pm.db_to_pool.clear();
    pm.registry.clear();
    crate::pg_client_stmt_cache::clear_all_stmt_caches();
}

/// # Safety
/// `db_path` must be a valid null-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn rust_pool_get_connection(db_path: *const c_char) -> *mut c_void {
    pool_get_connection_inner(db_path)
}

pub(crate) unsafe fn rust_pool_get_connection_excluding(
    db_path: *const c_char,
    exclude_conn: *const c_void,
) -> *mut c_void {
    pool_get_connection_inner_excluding(db_path, exclude_conn)
}

#[no_mangle]
pub extern "C" fn rust_pool_release_for_db(db: *const c_void) {
    pool_release_for_db_inner(db as usize);
}

#[no_mangle]
pub extern "C" fn rust_pool_validate_connection(conn: *const c_void) -> i32 {
    i32::from(pool().validate_connection(conn))
}

#[no_mangle]
pub extern "C" fn rust_pool_touch_connection(conn: *const c_void) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    pool().touch_connection(conn, now);
}

#[no_mangle]
pub extern "C" fn rust_pool_clear_streaming_active(conn: *const c_void) -> i32 {
    i32::from(pool().clear_streaming_active(conn))
}

#[no_mangle]
pub extern "C" fn rust_pool_is_live_connection(conn: *const c_void) -> i32 {
    i32::from(pool().is_live_pool_connection(conn))
}

#[no_mangle]
pub extern "C" fn rust_pool_is_tracked_connection(conn: *const c_void) -> i32 {
    i32::from(pool().is_tracked_connection(conn))
}

#[no_mangle]
pub extern "C" fn rust_pool_check_health(conn: *mut c_void) -> i32 {
    pool_check_health_inner(conn)
}

#[no_mangle]
pub extern "C" fn rust_pool_cleanup_after_fork() {
    pool().reset_for_child();
}

#[no_mangle]
pub extern "C" fn rust_register_connection(db_handle: *const c_void, conn: *const c_void) {
    pool().registry.register(db_handle as usize, conn as usize);
}

#[no_mangle]
pub extern "C" fn rust_unregister_connection(db_handle: *const c_void) {
    pool().registry.unregister(db_handle as usize);
}

#[no_mangle]
pub extern "C" fn rust_find_registered_connection(db_handle: *const c_void) -> *mut c_void {
    pool()
        .registry
        .find(db_handle as usize)
        .map(|p| p as *mut c_void)
        .unwrap_or(std::ptr::null_mut())
}

/// # Safety
/// `db_path` must be a valid null-terminated C string. `db_handle` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn rust_pool_find_connection(
    db_handle: *const c_void,
    db_path: *const c_char,
) -> *mut c_void {
    pool_find_connection_for_db(db_handle as usize, db_path)
}

#[no_mangle]
pub extern "C" fn rust_find_any_library_connection() -> *mut c_void {
    find_any_library_connection()
}

#[no_mangle]
pub extern "C" fn rust_pool_check_fork() -> i32 {
    let pm = pool();
    let init_pid = pm.init_pid.load(Ordering::Acquire);
    let current_pid = std::process::id();
    if init_pid != 0 && init_pid != current_pid {
        pm.reset_for_child();
        return 1;
    }
    0
}

#[no_mangle]
pub extern "C" fn rust_pg_client_init() {
    CLIENT_INIT.call_once(|| {
        let mut pool_size =
            parse_positive_env_or_default("PLEX_PG_POOL_SIZE", POOL_SIZE_DEFAULT as i32);
        let explicit_max = env_utils::env_string("PLEX_PG_POOL_MAX").is_some();
        let mut pool_max = parse_positive_env_or_default("PLEX_PG_POOL_MAX", pool_size);

        let cfg = conn_config();

        let db_max_connections = if cfg.host.is_empty() || cfg.port <= 0 {
            0
        } else {
            let host = CString::new(cfg.host.clone()).ok();
            let db = CString::new(cfg.database.clone()).ok();
            let user = CString::new(cfg.user.clone()).ok();
            let pass = CString::new(cfg.password.clone()).ok();
            if let (Some(h), Some(d), Some(u), Some(p)) = (host, db, user, pass) {
                rust_pg_probe_max_connections(
                    h.as_ptr(),
                    cfg.port,
                    d.as_ptr(),
                    u.as_ptr(),
                    p.as_ptr(),
                )
            } else {
                0
            }
        };

        if db_max_connections > 0 {
            // Every scanner, matcher and analysis job is a separate process
            // with its own connections, and each SQLite handle keeps a session
            // outside the pool. Without an explicit PLEX_PG_POOL_MAX the pool
            // may grow to half the server's slots, leaving the rest for them;
            // growing to all of them starved scanner jobs ("too many clients").
            let aligned = if explicit_max {
                pool_max.min(db_max_connections)
            } else {
                (db_max_connections / 2).max(1)
            };
            if pool_max != aligned {
                log_info_lazy!(
                    "Pool max ({}) adjusted to {} for database max_connections {}",
                    pool_max,
                    aligned,
                    db_max_connections
                );
                pool_max = aligned;
            }
        } else {
            log_info_lazy!(
                "Pool init: could not read database max_connections; keeping pool max={}",
                pool_max
            );
        }

        if pool_size > pool_max {
            log_info_lazy!(
                "Pool size {} exceeds pool max {}; clamping",
                pool_size,
                pool_max
            );
            pool_size = pool_max;
        }

        let mut idle_timeout = 300;
        if let Some(val) = env_utils::env_string("PLEX_PG_IDLE_TIMEOUT") {
            if let Ok(v) = val.trim().parse::<i32>() {
                if v >= 10 {
                    idle_timeout = v;
                }
            }
        }

        if !cfg.host.is_empty() && cfg.port > 0 {
            let host = CString::new(cfg.host.clone()).ok();
            let db = CString::new(cfg.database.clone()).ok();
            let user = CString::new(cfg.user.clone()).ok();
            let pass = CString::new(cfg.password.clone()).ok();
            if let (Some(h), Some(d), Some(u), Some(p)) = (host, db, user, pass) {
                idle_timeout = rust_pg_align_idle_timeout_with_server(
                    idle_timeout,
                    h.as_ptr(),
                    cfg.port,
                    d.as_ptr(),
                    u.as_ptr(),
                    p.as_ptr(),
                );
            }
        }

        rust_pool_init(pool_size, pool_max, idle_timeout);

        log_info_lazy!(
            "pg_client initialized (Rust pool): pool_size={}, pool_max={}, idle_timeout={}s",
            pool_size,
            pool_max,
            idle_timeout
        );
    });
}

#[no_mangle]
pub extern "C" fn rust_pg_client_cleanup() {
    let conns = pool().registry.drain_all();
    for conn in conns {
        close_handle_connection(conn as *mut PgConnection);
    }
    rust_pool_cleanup();
}
