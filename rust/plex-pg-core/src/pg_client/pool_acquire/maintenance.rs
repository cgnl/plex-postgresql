use std::sync::atomic::Ordering;

use crate::ffi_types::PgConnection;

use super::super::connection_helpers::conn_is_streaming_active_ptr;
use super::super::connection_lifecycle::destroy_pool_connection;
use super::super::threading::owner_is_dead;
use super::super::SLOT_READY;
use super::shared::AcquireCtx;
use crate::log_info_lazy;

pub(super) fn reclaim_zombies_and_reap(ctx: &AcquireCtx<'_>) {
    let idle_timeout = ctx.pm.idle_timeout_secs.load(Ordering::Relaxed) as i64;

    for i in 0..ctx.pool_size {
        let slot = &ctx.pm.slots[i];
        let state = slot.state.load(Ordering::Acquire);
        if state != SLOT_READY {
            continue;
        }
        let last_used = slot.last_used.load(Ordering::Acquire);
        if ctx.now - last_used <= idle_timeout {
            continue;
        }

        let owner = slot.owner_thread.load(Ordering::Acquire);
        if !owner_is_dead(owner) || !slot.try_reserve_zombie(owner) {
            continue;
        }

        let conn = slot.conn.load(Ordering::Acquire);
        if !conn.is_null() && conn_is_streaming_active_ptr(conn as *mut PgConnection) {
            slot.restore_zombie(owner);
            log_info_lazy!(
                "Pool PHASE 0: slot {} owner dead but streaming_active, skipping reclaim",
                i
            );
            continue;
        }

        slot.release();
        log_info_lazy!(
            "Pool PHASE 0: Freed zombie slot {} (owner thread dead, idle {} sec)",
            i,
            ctx.now - last_used
        );
    }

    let last_reap = ctx.pm.last_reap_time.load(Ordering::Relaxed);
    if ctx.now - last_reap < 60 {
        return;
    }
    if ctx
        .pm
        .last_reap_time
        .compare_exchange(last_reap, ctx.now, Ordering::SeqCst, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    log_info_lazy!(
        "Pool reaper: running (last run {} seconds ago)",
        ctx.now - last_reap
    );
    let to_destroy = ctx.pm.reap_idle(ctx.now);
    for (_slot_idx, conn_ptr) in to_destroy {
        destroy_pool_connection(conn_ptr);
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::pool_state::{PoolManager, SLOT_FREE};
    use super::super::super::threading::DEAD_BIT;
    use super::*;

    #[test]
    fn maintenance_keeps_live_and_streaming_owners_and_reclaims_only_idle_dead() {
        let pm = PoolManager::new(4, 4);
        let now = 1000;
        pm.last_reap_time.store(now, Ordering::Relaxed);
        // A zeroed fixture is never passed to libpq or mutex functions; only
        // its initialized atomic streaming flag is accessed by maintenance.
        let streaming: PgConnection = unsafe { std::mem::zeroed() };
        streaming.streaming_active.store(1, Ordering::Release);
        for (slot, owner) in pm
            .slots
            .iter()
            .zip([7, DEAD_BIT | 8, DEAD_BIT | 9, DEAD_BIT | 10])
        {
            slot.mark_ready();
            slot.owner_thread.store(owner, Ordering::Release);
            slot.last_used.store(0, Ordering::Release);
        }
        pm.slots[2]
            .conn
            .store(&streaming as *const _ as *mut _, Ordering::Release);
        pm.slots[3].last_used.store(now, Ordering::Release);
        let ctx = AcquireCtx {
            pm: &pm,
            current_thread: 11,
            now,
            pool_size: 4,
            db_path: std::ptr::null(),
            exclude_conn: std::ptr::null(),
        };
        reclaim_zombies_and_reap(&ctx);
        assert_eq!(pm.slots[0].owner_thread.load(Ordering::Acquire), 7);
        assert_eq!(pm.slots[0].state.load(Ordering::Acquire), SLOT_READY);
        assert_eq!(pm.slots[1].owner_thread.load(Ordering::Acquire), 0);
        assert_eq!(pm.slots[1].state.load(Ordering::Acquire), SLOT_FREE);
        assert_eq!(
            pm.slots[2].owner_thread.load(Ordering::Acquire),
            DEAD_BIT | 9
        );
        assert_eq!(pm.slots[2].state.load(Ordering::Acquire), SLOT_READY);
        assert_eq!(pm.slots[3].state.load(Ordering::Acquire), SLOT_READY);
        streaming.streaming_active.store(0, Ordering::Release);
        reclaim_zombies_and_reap(&ctx);
        assert_eq!(pm.slots[2].state.load(Ordering::Acquire), SLOT_FREE);
    }

    #[test]
    fn concurrent_reclaimers_preserve_streaming_and_new_live_ownership() {
        use std::sync::{Arc, Barrier};
        let pm = Arc::new(PoolManager::new(1, 1));
        let streaming: PgConnection = unsafe { std::mem::zeroed() };
        streaming.streaming_active.store(1, Ordering::Release);
        let slot = &pm.slots[0];
        let dead = DEAD_BIT | 101;
        slot.owner_thread.store(dead, Ordering::Release);
        slot.conn
            .store(&streaming as *const _ as *mut _, Ordering::Release);
        slot.mark_ready();
        pm.last_reap_time.store(1000, Ordering::Release);
        let barrier = Arc::new(Barrier::new(3));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let pm = Arc::clone(&pm);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let ctx = AcquireCtx {
                        pm: &pm,
                        current_thread: 102,
                        now: 1000,
                        pool_size: 1,
                        db_path: std::ptr::null(),
                        exclude_conn: std::ptr::null(),
                    };
                    barrier.wait();
                    for _ in 0..1000 {
                        reclaim_zombies_and_reap(&ctx);
                    }
                    barrier.wait();
                    barrier.wait();
                    for _ in 0..1000 {
                        reclaim_zombies_and_reap(&ctx);
                        std::thread::yield_now();
                    }
                })
            })
            .collect();
        barrier.wait();
        barrier.wait();
        assert_eq!(slot.owner_thread.load(Ordering::Acquire), dead);
        assert_eq!(slot.state.load(Ordering::Acquire), SLOT_READY);
        streaming.streaming_active.store(0, Ordering::Release);
        barrier.wait();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !slot.try_claim_free() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        let live = super::super::super::threading::current_thread_id();
        slot.owner_thread.store(live, Ordering::Release);
        slot.mark_ready();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(slot.owner_thread.load(Ordering::Acquire), live);
        assert_eq!(slot.state.load(Ordering::Acquire), SLOT_READY);
        assert!(!slot.try_reserve_zombie(dead));
    }
}
