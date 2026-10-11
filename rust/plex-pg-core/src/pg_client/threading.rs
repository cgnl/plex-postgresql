use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use super::pool_state::POOL;

// pthread_t may point into an unmapped thread stack after exit on musl.
// Pool ownership must never dereference a foreign or expired pthread handle.
pub(crate) const DEAD_BIT: u64 = 1 << 63;
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

fn allocate_token(counter: &AtomicU64) -> u64 {
    let mut next = counter.load(Ordering::Relaxed);
    loop {
        if next >= DEAD_BIT {
            return 0;
        }
        match counter.compare_exchange_weak(next, next + 1, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(token) => return token,
            Err(observed) => next = observed,
        }
    }
}

struct ThreadOwner {
    pid: Cell<u32>,
    token: Cell<u64>,
}

impl ThreadOwner {
    fn token_for_pid(&self, pid: u32) -> u64 {
        if self.pid.get() != pid {
            // The fork survivor receives a new identity. Never retire the
            // inherited parent's token against the child's pool.
            self.token.set(allocate_token(&NEXT_TOKEN));
            self.pid.set(pid);
        }
        self.token.get()
    }
}

impl Drop for ThreadOwner {
    fn drop(&mut self) {
        if self.pid.get() != std::process::id() {
            return;
        }
        if let Some(pm) = POOL.get() {
            pm.retire_thread_owner(self.token.get());
        }
    }
}

thread_local! {
    static THREAD_OWNER: ThreadOwner = ThreadOwner {
        pid: Cell::new(std::process::id()),
        token: Cell::new(allocate_token(&NEXT_TOKEN)),
    };
}

pub(super) fn current_thread_id() -> u64 {
    THREAD_OWNER
        .try_with(|owner| owner.token_for_pid(std::process::id()))
        .unwrap_or(0)
}

pub(super) fn threads_equal(a: u64, b: u64) -> bool {
    a != 0 && a < DEAD_BIT && a == b
}

pub(crate) fn owner_is_dead(owner: u64) -> bool {
    owner > DEAD_BIT
}

pub(super) fn sleep_ms(ms: i32) {
    if ms <= 0 {
        return;
    }
    unsafe {
        libc::usleep((ms as u32).saturating_mul(1000));
    }
}

#[cfg(test)]
mod tests {
    use super::super::pool_state::{pool, PoolManager, SLOT_READY, SLOT_RESERVED};
    use super::*;
    use std::sync::Arc;

    #[test]
    fn exhausted_tokens_never_wrap_or_become_dead_markers() {
        let counter = AtomicU64::new(DEAD_BIT - 1);
        assert_eq!(allocate_token(&counter), DEAD_BIT - 1);
        assert_eq!(allocate_token(&counter), 0);
        assert_eq!(allocate_token(&counter), 0);
    }

    #[test]
    fn pid_change_renews_without_retiring_parent_owner() {
        let original = current_thread_id();
        let owner = ThreadOwner {
            pid: Cell::new(0),
            token: Cell::new(original),
        };
        let renewed = owner.token_for_pid(std::process::id());
        assert_ne!(renewed, original);
        assert!(threads_equal(renewed, renewed));
        let pm = pool();
        let slot = &pm.slots[pm.slots.len() - 2];
        slot.owner_thread.store(original, Ordering::Release);
        let inherited = ThreadOwner {
            pid: Cell::new(0),
            token: Cell::new(original),
        };
        drop(inherited);
        assert_eq!(slot.owner_thread.load(Ordering::Acquire), original);
        drop(owner);
        assert_eq!(slot.owner_thread.load(Ordering::Acquire), original);
        slot.release();
    }

    #[test]
    fn fork_survivor_receives_a_new_token() {
        let parent_token = current_thread_id();
        let mut pipe = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            // No allocator or mutex use in the forked child.
            let child_token = current_thread_id();
            let valid = u8::from(child_token != 0 && child_token != parent_token);
            unsafe {
                libc::close(pipe[0]);
                libc::write(pipe[1], &valid as *const u8 as *const libc::c_void, 1);
                libc::_exit(0);
            }
        }
        let mut valid = 0_u8;
        let mut status = 0;
        unsafe {
            libc::close(pipe[1]);
            assert_eq!(
                libc::read(pipe[0], &mut valid as *mut u8 as *mut libc::c_void, 1),
                1
            );
            libc::close(pipe[0]);
            assert_eq!(libc::waitpid(child, &mut status, 0), child);
        }
        assert_eq!(status, 0);
        assert_eq!(valid, 1);
        assert_eq!(current_thread_id(), parent_token);
    }

    #[test]
    fn retirement_and_reservation_reject_new_live_owner_after_dead_observation() {
        let pm = PoolManager::new(1, 1);
        let slot = &pm.slots[0];
        slot.mark_ready();
        slot.owner_thread.store(17, Ordering::Release);
        pm.retire_thread_owner(17);
        let observed = slot.owner_thread.load(Ordering::Acquire);
        assert_eq!(observed, DEAD_BIT | 17);
        slot.owner_thread.store(18, Ordering::Release);
        assert!(!slot.try_reserve_zombie(observed));
        assert_eq!(slot.state.load(Ordering::Acquire), SLOT_READY);
        assert_eq!(slot.owner_thread.load(Ordering::Acquire), 18);
        assert!(!slot.try_reserve_zombie(18));
        pm.retire_thread_owner(18);
        assert!(slot.try_reserve_zombie(DEAD_BIT | 18));
        assert_eq!(slot.state.load(Ordering::Acquire), SLOT_RESERVED);
        slot.restore_zombie(DEAD_BIT | 18);
        assert_eq!(slot.owner_thread.load(Ordering::Acquire), DEAD_BIT | 18);
        assert_eq!(slot.state.load(Ordering::Acquire), SLOT_READY);
    }

    struct AfterOwnerDrop(Arc<AtomicU64>);
    impl Drop for AfterOwnerDrop {
        fn drop(&mut self) {
            self.0.store(current_thread_id(), Ordering::Release);
            let conn = super::super::pool_acquire::pool_get_connection_inner(std::ptr::null());
            assert!(conn.is_null());
        }
    }
    thread_local! {
        static AFTER_OWNER: Cell<Option<AfterOwnerDrop>> = const { Cell::new(None) };
    }

    struct ForeignArgs {
        token: AtomicU64,
        after_drop: Arc<AtomicU64>,
        slot_idx: usize,
        finished: AtomicU64,
    }
    extern "C" fn foreign_owner(arg: *mut libc::c_void) -> *mut libc::c_void {
        let args = unsafe { &*(arg as *const ForeignArgs) };
        AFTER_OWNER.with(|late| late.set(Some(AfterOwnerDrop(args.after_drop.clone()))));
        let token = current_thread_id();
        let slot = &pool().slots[args.slot_idx];
        slot.owner_thread.store(token, Ordering::Release);
        slot.mark_ready();
        args.token.store(token, Ordering::Release);
        args.finished.store(1, Ordering::Release);
        std::ptr::null_mut()
    }

    #[test]
    fn foreign_joined_and_detached_threads_retire_unique_tokens() {
        let pm = pool();
        let slot_idx = pm.slots.len() - 1;
        let mut seen_tokens = std::collections::HashSet::new();
        let mut seen_handles = std::collections::HashSet::new();
        let mut reused_handle = false;
        for detached in [false, true] {
            for _ in 0..256 {
                let args = Box::new(ForeignArgs {
                    token: AtomicU64::new(0),
                    after_drop: Arc::new(AtomicU64::new(u64::MAX)),
                    slot_idx,
                    finished: AtomicU64::new(0),
                });
                let mut thread = unsafe { std::mem::zeroed() };
                assert_eq!(
                    unsafe {
                        libc::pthread_create(
                            &mut thread,
                            std::ptr::null(),
                            foreign_owner,
                            &*args as *const ForeignArgs as *mut libc::c_void,
                        )
                    },
                    0
                );
                // Store only for observing handle reuse; never call a foreign
                // pthread API on this value after join/exit.
                reused_handle |= !seen_handles.insert(thread);
                if detached {
                    assert_eq!(unsafe { libc::pthread_detach(thread) }, 0);
                } else {
                    assert_eq!(
                        unsafe { libc::pthread_join(thread, std::ptr::null_mut()) },
                        0
                    );
                }
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while args.after_drop.load(Ordering::Acquire) == u64::MAX {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "TLS teardown timed out"
                    );
                    std::thread::yield_now();
                }
                assert_eq!(args.after_drop.load(Ordering::Acquire), 0);
                let token = args.token.load(Ordering::Acquire);
                assert!(token != 0 && token < DEAD_BIT);
                assert!(seen_tokens.insert(token));
                let slot = &pm.slots[slot_idx];
                assert_eq!(slot.owner_thread.load(Ordering::Acquire), token | DEAD_BIT);
                assert!(slot.try_reserve_zombie(token | DEAD_BIT));
                slot.release();
                // Detached callback no longer accesses args after publishing
                // finished, and AfterOwnerDrop owns its own Arc reference.
                assert_eq!(args.finished.load(Ordering::Acquire), 1);
            }
        }
        // Supported libc implementations recycle handles even though tokens
        // must never be recycled. This assertion is exercised on native musl.
        assert!(
            reused_handle,
            "fixture did not observe pthread handle reuse"
        );
    }
}
