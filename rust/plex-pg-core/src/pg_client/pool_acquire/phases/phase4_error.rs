use std::sync::atomic::Ordering;

use crate::ffi_types::PgConnection;

use super::super::super::connection_helpers::conn_is_pg_active_ptr;
use super::super::super::connection_lifecycle::{create_pool_connection, destroy_pool_connection};
use super::super::shared::{AcquireCtx, AcquireDecision};
use super::common::{
    claim_slot_for_thread, mark_ready_and_cache_slot, release_failed_new_connection,
};
use crate::log_debug_lazy;
use crate::log_info_lazy;

pub(crate) fn phase4_reclaim_error(ctx: &AcquireCtx<'_>) -> AcquireDecision {
    for i in 0..ctx.pool_size {
        if ctx.connect_failed.get() {
            break;
        }
        let slot = &ctx.pm.slots[i];
        if !slot.try_reclaim_error() {
            continue;
        }

        claim_slot_for_thread(ctx, slot);

        let old_conn = slot.conn.swap(std::ptr::null_mut(), Ordering::SeqCst);
        if !old_conn.is_null() {
            destroy_pool_connection(old_conn);
        }

        log_debug_lazy!("Pool: reclaiming error slot {}", i);

        let new_conn = create_pool_connection(ctx.db_path);
        if !new_conn.is_null() && conn_is_pg_active_ptr(new_conn as *mut PgConnection) {
            slot.conn.store(new_conn, Ordering::Release);
            log_info_lazy!("Pool: recovered slot {} with new connection", i);
            return mark_ready_and_cache_slot(i, slot, new_conn);
        }

        release_failed_new_connection(slot, new_conn);
        ctx.connect_failed.set(true);
    }

    AcquireDecision::Continue
}
