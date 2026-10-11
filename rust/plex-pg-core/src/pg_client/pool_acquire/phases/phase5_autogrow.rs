use std::sync::atomic::Ordering;

use crate::db_interpose_conn_utils::log_error;
use crate::ffi_types::PgConnection;

use super::super::super::connection_helpers::conn_is_pg_active_ptr;
use super::super::super::connection_lifecycle::create_pool_connection;
use super::super::shared::{AcquireCtx, AcquireDecision};
use super::common::{
    claim_slot_for_thread, mark_ready_and_cache_slot, release_failed_new_connection,
};

pub(crate) fn phase5_autogrow(ctx: &AcquireCtx<'_>) -> AcquireDecision {
    // Growth adds capacity; it cannot help while PostgreSQL is unreachable.
    if ctx.connect_failed.get() {
        return AcquireDecision::Continue;
    }
    let current_size = ctx.pm.configured_size.load(Ordering::Relaxed);
    let runtime_max = ctx.pm.pool_max();
    if current_size >= runtime_max {
        return AcquireDecision::Continue;
    }

    let new_size = current_size + 1;
    if ctx
        .pm
        .configured_size
        .compare_exchange(current_size, new_size, Ordering::SeqCst, Ordering::Relaxed)
        .is_err()
    {
        return AcquireDecision::Continue;
    }

    let idx = new_size - 1;
    if idx >= ctx.pm.slots.len() {
        return AcquireDecision::Continue;
    }

    let slot = &ctx.pm.slots[idx];
    if !slot.try_claim_free() {
        return AcquireDecision::Continue;
    }

    claim_slot_for_thread(ctx, slot);

    log_error(&format!(
        "Pool: auto-grew {} -> {} (thread needs slot)",
        current_size, new_size
    ));

    let new_conn = create_pool_connection(ctx.db_path);
    if !new_conn.is_null() && conn_is_pg_active_ptr(new_conn as *mut PgConnection) {
        slot.conn.store(new_conn, Ordering::Release);
        return mark_ready_and_cache_slot(idx, slot, new_conn);
    }

    log_error(&format!("Pool: auto-grow slot {} connection failed", idx));
    release_failed_new_connection(slot, new_conn);

    AcquireDecision::Continue
}
