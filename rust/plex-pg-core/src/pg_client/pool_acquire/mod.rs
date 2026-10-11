use std::ffi::CString;
use std::os::raw::{c_char, c_void};

use super::pool;
use super::threading::current_thread_id;

mod maintenance;
mod pathing;
mod phases;
mod retry;
mod shared;

use maintenance::reclaim_zombies_and_reap;
use pathing::{remember_library_path, resolve_selected_pool_path};
use phases::{
    phase1_existing_ready, phase2_reuse_existing, phase3_create_empty, phase4_reclaim_error,
    phase5_autogrow,
};
use retry::phase6_retry;
use shared::{AcquireCtx, AcquireDecision};

pub(super) fn pool_get_connection_inner(db_path: *const c_char) -> *mut c_void {
    pool_get_connection_inner_excluding(db_path, std::ptr::null())
}

pub(super) fn pool_get_connection_inner_excluding(
    db_path: *const c_char,
    exclude_conn: *const c_void,
) -> *mut c_void {
    let current_thread = current_thread_id();
    if current_thread == 0 {
        return std::ptr::null_mut();
    }
    let pm = pool();
    let selected_path = match resolve_selected_pool_path(pm, db_path, exclude_conn) {
        Some(path) => path,
        None => return std::ptr::null_mut(),
    };
    let selected_path_c =
        CString::new(selected_path.clone()).expect("selected library path cannot contain NUL");
    let db_path = selected_path_c.as_ptr();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    remember_library_path(pm, &selected_path);

    let ctx = AcquireCtx {
        pm,
        current_thread,
        now,
        pool_size: pm.pool_size(),
        db_path,
        exclude_conn,
    };

    if let AcquireDecision::Return(conn) = phase1_existing_ready(&ctx) {
        return conn;
    }

    reclaim_zombies_and_reap(&ctx);

    if let AcquireDecision::Return(conn) = phase2_reuse_existing(&ctx) {
        return conn;
    }
    if let AcquireDecision::Return(conn) = phase3_create_empty(&ctx) {
        return conn;
    }
    if let AcquireDecision::Return(conn) = phase4_reclaim_error(&ctx) {
        return conn;
    }
    if let AcquireDecision::Return(conn) = phase5_autogrow(&ctx) {
        return conn;
    }

    phase6_retry(&ctx)
}
