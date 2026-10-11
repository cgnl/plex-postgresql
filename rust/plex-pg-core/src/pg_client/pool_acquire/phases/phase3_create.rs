use std::sync::atomic::Ordering;

use crate::db_interpose_conn_utils::log_error;
use crate::ffi_types::PgConnection;

use super::super::super::connection_helpers::conn_is_pg_active_ptr;
use super::super::super::connection_lifecycle::create_pool_connection;
use super::super::shared::{AcquireCtx, AcquireDecision};
use super::common::{
    claim_slot_for_thread, mark_ready_and_cache_slot, release_failed_new_connection,
};
use crate::log_debug_lazy;
use crate::log_info_lazy;

pub(crate) fn phase3_create_empty(ctx: &AcquireCtx<'_>) -> AcquireDecision {
    for i in 0..ctx.pool_size {
        let slot = &ctx.pm.slots[i];
        if ctx.connect_failed.get() {
            break;
        }
        if !slot.conn.load(Ordering::Acquire).is_null() {
            continue;
        }
        if !slot.try_claim_free() {
            continue;
        }

        claim_slot_for_thread(ctx, slot);

        log_debug_lazy!("Pool: claimed empty slot {} for thread", i);

        let new_conn = create_pool_connection(ctx.db_path);
        if !new_conn.is_null() && conn_is_pg_active_ptr(new_conn as *mut PgConnection) {
            slot.conn.store(new_conn, Ordering::Release);
            log_info_lazy!("Pool: created new connection in slot {}", i);
            return mark_ready_and_cache_slot(i, slot, new_conn);
        }

        log_error(&format!("Pool: failed to create connection for slot {}", i));
        release_failed_new_connection(slot, new_conn);
        ctx.connect_failed.set(true);
    }

    AcquireDecision::Continue
}
