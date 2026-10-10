use super::*;

// ═════════════════════════════════════════════════════════════════════════
// NEW TESTS: Pool State Machine (Stap 3)
// ═════════════════════════════════════════════════════════════════════════

#[test]
fn pool_slot_initial_state_is_free() {
    let slot = PoolSlot::new();
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_FREE);
    assert!(slot.conn.load(Ordering::Relaxed).is_null());
    assert_eq!(slot.owner_thread.load(Ordering::Relaxed), 0);
    assert_eq!(slot.generation.load(Ordering::Relaxed), 0);
}

#[test]
fn pool_slot_claim_free_succeeds() {
    let slot = PoolSlot::new();
    assert!(slot.try_claim_free());
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_RESERVED);
}

#[test]
fn pool_slot_claim_free_fails_when_reserved() {
    let slot = PoolSlot::new();
    assert!(slot.try_claim_free());
    // Second claim must fail
    assert!(!slot.try_claim_free());
}

#[test]
fn pool_slot_claim_free_fails_when_ready() {
    let slot = PoolSlot::new();
    slot.state.store(SLOT_READY, Ordering::Relaxed);
    assert!(!slot.try_claim_free());
}

#[test]
fn pool_slot_mark_ready_after_reserve() {
    let slot = PoolSlot::new();
    assert!(slot.try_claim_free());
    slot.mark_ready();
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_READY);
}

#[test]
fn pool_slot_mark_error_after_reserve() {
    let slot = PoolSlot::new();
    assert!(slot.try_claim_free());
    slot.mark_error();
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_ERROR);
}

#[test]
fn pool_slot_begin_reconnect_from_ready() {
    let slot = PoolSlot::new();
    slot.state.store(SLOT_READY, Ordering::Relaxed);
    assert!(slot.try_begin_reconnect());
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_RECONNECTING);
}

#[test]
fn pool_slot_begin_reconnect_fails_from_free() {
    let slot = PoolSlot::new();
    assert!(!slot.try_begin_reconnect());
}

#[test]
fn pool_slot_reclaim_error_succeeds() {
    let slot = PoolSlot::new();
    slot.state.store(SLOT_ERROR, Ordering::Relaxed);
    assert!(slot.try_reclaim_error());
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_RESERVED);
}

#[test]
fn pool_slot_reclaim_error_fails_from_ready() {
    let slot = PoolSlot::new();
    slot.state.store(SLOT_READY, Ordering::Relaxed);
    assert!(!slot.try_reclaim_error());
}

#[test]
fn pool_slot_reclaim_zombie_from_ready() {
    let slot = PoolSlot::new();
    slot.state.store(SLOT_READY, Ordering::Relaxed);
    let dead = super::super::threading::DEAD_BIT | 1;
    slot.owner_thread.store(dead, Ordering::Relaxed);
    assert!(slot.try_reserve_zombie(dead));
    slot.release();
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_FREE);
}

#[test]
fn pool_slot_reclaim_zombie_fails_from_free() {
    let slot = PoolSlot::new();
    let dead = super::super::threading::DEAD_BIT | 1;
    slot.owner_thread.store(dead, Ordering::Relaxed);
    assert!(!slot.try_reserve_zombie(dead));
    assert_eq!(slot.owner_thread.load(Ordering::Acquire), dead);
}

#[test]
fn pool_slot_release_clears_owner_and_state() {
    let slot = PoolSlot::new();
    slot.state.store(SLOT_READY, Ordering::Relaxed);
    slot.owner_thread.store(12345, Ordering::Relaxed);
    slot.release();
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_FREE);
    assert_eq!(slot.owner_thread.load(Ordering::Relaxed), 0);
}

#[test]
fn pool_slot_full_lifecycle() {
    // FREE → RESERVED → READY → RECONNECTING → READY → FREE
    let slot = PoolSlot::new();
    assert!(slot.try_claim_free()); // FREE → RESERVED
    slot.mark_ready(); // RESERVED → READY
    assert!(slot.try_begin_reconnect()); // READY → RECONNECTING
    slot.mark_ready(); // RECONNECTING → READY
    slot.release(); // READY → FREE
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_FREE);
}

#[test]
fn pool_slot_error_recovery_lifecycle() {
    // FREE → RESERVED → ERROR → RESERVED → READY → FREE
    let slot = PoolSlot::new();
    assert!(slot.try_claim_free()); // FREE → RESERVED
    slot.mark_error(); // RESERVED → ERROR
    assert!(slot.try_reclaim_error()); // ERROR → RESERVED
    slot.mark_ready(); // RESERVED → READY
    slot.release(); // READY → FREE
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_FREE);
}

#[test]
fn pool_slot_concurrent_claim_only_one_wins() {
    use std::sync::Arc;
    let slot = Arc::new(PoolSlot::new());
    let mut handles = vec![];
    let wins = Arc::new(AtomicU32::new(0));

    for _ in 0..10 {
        let s = Arc::clone(&slot);
        let w = Arc::clone(&wins);
        handles.push(std::thread::spawn(move || {
            if s.try_claim_free() {
                w.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(wins.load(Ordering::Relaxed), 1);
}
