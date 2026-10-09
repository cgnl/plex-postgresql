use crate::ffi_types::PgConnection;

use super::super::super::connection_helpers::{
    conn_is_pg_active_ptr, conn_is_streaming_active_ptr,
};
use super::super::super::connection_lifecycle::{get_txn_status, reconnect_conn, reset_conn};
use super::super::super::rust_stmt_cache_clear;
use super::super::super::session::{exec_simple, PQTRANS_INERROR, PQTRANS_INTRANS};
use super::super::shared::{AcquireCtx, AcquireDecision};
use super::common::{claim_slot_for_thread, mark_ready_and_cache_slot};
use crate::log_debug_lazy;
use crate::log_info_lazy;

pub(crate) fn phase2_reuse_existing(ctx: &AcquireCtx<'_>) -> AcquireDecision {
    for i in 0..ctx.pool_size {
        let slot = &ctx.pm.slots[i];
        if !slot.try_claim_free() {
            continue;
        }
        let conn = slot.conn.load(std::sync::atomic::Ordering::Acquire);
        if conn.is_null() || conn == ctx.exclude_conn as *mut _ {
            slot.release();
            continue;
        }
        if conn_is_streaming_active_ptr(conn as *mut PgConnection) {
            slot.release();
            continue;
        }

        claim_slot_for_thread(ctx, slot);

        let txn = get_txn_status(conn);
        if txn == PQTRANS_INTRANS || txn == PQTRANS_INERROR {
            let cmd = c"ROLLBACK";
            log_info_lazy!(
                "Pool PHASE 2: slot {} has pending transaction (status={}), sending cleanup before reset",
                i, txn
            );
            let _ = exec_simple(conn, cmd.as_ptr());
        }

        rust_stmt_cache_clear(conn);
        if reset_conn(conn) {
            log_debug_lazy!("Pool: reusing reset connection in slot {}", i);
            return mark_ready_and_cache_slot(i, slot, conn);
        }

        rust_stmt_cache_clear(conn);
        if reconnect_conn(conn) && conn_is_pg_active_ptr(conn as *mut PgConnection) {
            return mark_ready_and_cache_slot(i, slot, conn);
        }

        slot.mark_error();
    }

    AcquireDecision::Continue
}
