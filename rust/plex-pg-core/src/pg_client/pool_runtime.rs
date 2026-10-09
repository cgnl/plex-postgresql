use std::ffi::CString;
use std::os::raw::c_void;
use std::sync::atomic::Ordering;

use crate::db_interpose_conn_utils::log_error;
use crate::ffi_types::PgConnection;

use super::connection_helpers::conn_is_streaming_active_ptr;
use super::connection_lifecycle::{check_conn_ok, get_txn_status, reconnect_conn, reset_conn};
use super::session::{exec_simple, PQTRANS_INERROR, PQTRANS_INTRANS};
use super::threading::{current_thread_id, threads_equal};
use super::tls_cache::tls_pool_cache_clear;
use super::{log_info, pool, rust_stmt_cache_clear, SLOT_FREE, SLOT_READY};
use crate::log_info_lazy;

pub(super) fn pool_release_for_db_inner(db_handle: usize) {
    let pm = pool();
    let slot_opt = pm.db_to_pool.release(db_handle);

    if let Some(slot_idx) = slot_opt {
        let pool_size = pm.pool_size();
        if slot_idx < pool_size {
            let slot = &pm.slots[slot_idx];
            let current_thread = current_thread_id();
            let owner = slot.owner_thread.load(Ordering::Acquire);

            if threads_equal(owner, current_thread) {
                let state = slot.state.load(Ordering::Acquire);
                if state == SLOT_READY {
                    let conn = slot.conn.load(Ordering::Acquire);
                    if !conn.is_null() {
                        if conn_is_streaming_active_ptr(conn as *mut PgConnection) {
                            let scope = CString::new("POOL RELEASE").unwrap();
                            log_error(&format!(
                                "Pool: releasing slot {} while streaming_active=1, forcing cancel/drain",
                                slot_idx
                            ));
                            crate::db_interpose_conn_utils::rust_step_conn_cancel_and_drain(
                                conn as *mut PgConnection,
                                scope.as_ptr(),
                            );
                            // SAFETY: conn is non-null (checked above).
                            let conn_ref = unsafe { &*(conn as *mut PgConnection) };
                            conn_ref.streaming_active.store(0, Ordering::Release);
                        }

                        let txn = get_txn_status(conn);
                        if txn == PQTRANS_INTRANS || txn == PQTRANS_INERROR {
                            let cmd = c"ROLLBACK";
                            log_info_lazy!(
                                "Pool: slot {} has pending transaction (status={}), sending cleanup before release",
                                slot_idx, txn
                            );
                            let _ = exec_simple(conn, cmd.as_ptr());
                        }
                    }

                    slot.owner_thread.store(0, Ordering::Release);
                    slot.state.store(SLOT_FREE, Ordering::Release);
                    log_info_lazy!("Pool: releasing slot {} for db {:x}", slot_idx, db_handle);
                }
            }
        }
    }

    tls_pool_cache_clear();
}

pub(super) fn pool_check_health_inner(conn: *mut c_void) -> i32 {
    if conn.is_null() {
        return 0;
    }

    if check_conn_ok(conn) {
        return 0;
    }

    log_info("Pool: connection health check failed, resetting");

    let pm = pool();
    let current_thread = current_thread_id();
    let pool_size = pm.pool_size();

    for i in 0..pool_size {
        let slot = &pm.slots[i];
        if slot.conn.load(Ordering::Acquire) != conn {
            continue;
        }
        let owner = slot.owner_thread.load(Ordering::Acquire);
        if !threads_equal(owner, current_thread) {
            continue;
        }

        if !slot.try_begin_reconnect() {
            break;
        }

        rust_stmt_cache_clear(conn);

        let reset_ok = reset_conn(conn);
        if reset_ok {
            log_info_lazy!("Pool: connection reset successful for slot {}", i);
            slot.last_used.store(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
                Ordering::Release,
            );
            slot.mark_ready();
            return 1;
        }

        log_error(&format!(
            "Pool: PQreset failed for slot {}, trying fresh connection...",
            i
        ));
        let reconn_ok = reconnect_conn(conn);
        if reconn_ok {
            slot.last_used.store(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
                Ordering::Release,
            );
            log_info_lazy!(
                "Pool: fresh connection succeeded for slot {} (reconnected)",
                i
            );
            slot.mark_ready();
            return 1;
        } else {
            log_error(&format!(
                "Pool: fresh connection also failed for slot {}",
                i
            ));
            slot.mark_error();
            return 1;
        }
    }

    0
}
