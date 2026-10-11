use std::collections::HashSet;
use std::os::raw::c_void;
use std::sync::atomic::{
    AtomicI64, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};
use std::sync::{Mutex, OnceLock};

use crate::ffi_types::PgConnection;
use crate::sync_utils::mutex_lock;

use super::registry::{ConnectionRegistry, DbToPool};
use super::tls_cache::tls_pool_cache_clear;
use crate::log_info_lazy;

pub(crate) const SLOT_FREE: u8 = 0;
pub(crate) const SLOT_RESERVED: u8 = 1;
pub(crate) const SLOT_READY: u8 = 2;
pub(crate) const SLOT_RECONNECTING: u8 = 3;
pub(crate) const SLOT_ERROR: u8 = 4;

pub(crate) const POOL_SIZE_DEFAULT: usize = 50;

/// A single slot in the connection pool.
/// All fields are atomic for lock-free CAS-based state transitions.
pub(crate) struct PoolSlot {
    /// Opaque pointer to C-allocated pg_connection_t (null = no connection)
    pub conn: AtomicPtr<c_void>,
    /// Logical TLS owner token (0 = unowned, high bit = retired owner)
    pub owner_thread: AtomicU64,
    /// Unix timestamp of last use
    pub last_used: AtomicI64,
    /// State machine: SLOT_FREE=0, SLOT_RESERVED=1, SLOT_READY=2, etc.
    pub state: AtomicU8,
    /// Monotonically increasing generation counter (detects stale TLS refs)
    pub generation: AtomicU32,
}

impl PoolSlot {
    pub fn new() -> Self {
        Self {
            conn: AtomicPtr::new(std::ptr::null_mut()),
            owner_thread: AtomicU64::new(0),
            last_used: AtomicI64::new(0),
            state: AtomicU8::new(SLOT_FREE),
            generation: AtomicU32::new(0),
        }
    }

    /// CAS: FREE → RESERVED. Returns true on success.
    pub fn try_claim_free(&self) -> bool {
        self.state
            .compare_exchange(
                SLOT_FREE,
                SLOT_RESERVED,
                Ordering::SeqCst,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    /// CAS: READY → RECONNECTING. Returns true on success.
    pub fn try_begin_reconnect(&self) -> bool {
        self.state
            .compare_exchange(
                SLOT_READY,
                SLOT_RECONNECTING,
                Ordering::SeqCst,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    /// CAS: ERROR → RESERVED. Returns true on success.
    pub fn try_reclaim_error(&self) -> bool {
        self.state
            .compare_exchange(
                SLOT_ERROR,
                SLOT_RESERVED,
                Ordering::SeqCst,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    /// Reserve precisely the observed retired owner before touching its connection.
    pub fn try_reserve_zombie(&self, dead_owner: u64) -> bool {
        use super::threading::{owner_is_dead, DEAD_BIT};
        if !owner_is_dead(dead_owner)
            || self
                .owner_thread
                .compare_exchange(dead_owner, DEAD_BIT, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return false;
        }
        if self
            .state
            .compare_exchange(
                SLOT_READY,
                SLOT_RESERVED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            let _ = self.owner_thread.compare_exchange(
                DEAD_BIT,
                dead_owner,
                Ordering::Release,
                Ordering::Relaxed,
            );
            return false;
        }
        true
    }

    /// A streaming connection remains owned by its retired thread until drained.
    pub fn restore_zombie(&self, dead_owner: u64) {
        self.owner_thread.store(dead_owner, Ordering::Release);
        self.state.store(SLOT_READY, Ordering::Release);
    }

    /// Set slot to READY (after successful connection creation).
    pub fn mark_ready(&self) {
        self.state.store(SLOT_READY, Ordering::Release);
    }

    /// Set slot to ERROR (after failed connection/reconnect).
    pub fn mark_error(&self) {
        self.state.store(SLOT_ERROR, Ordering::Release);
    }

    /// Release slot back to FREE (on close or cleanup).
    pub fn release(&self) {
        self.owner_thread.store(0, Ordering::Release);
        self.state.store(SLOT_FREE, Ordering::Release);
    }
}

/// Central pool manager holding all connection pool state.
pub(crate) struct PoolManager {
    pub slots: Vec<PoolSlot>,
    pub configured_size: AtomicUsize,
    pub configured_max_size: AtomicUsize,
    pub idle_timeout_secs: AtomicU32,
    pub library_db_path: Mutex<Option<String>>,
    pub last_reap_time: AtomicI64,
    pub init_pid: AtomicU32,
    pub registry: ConnectionRegistry,
    pub db_to_pool: DbToPool,
    pub global_metadata_id: AtomicI64,
    pub global_last_insert_rowid: AtomicI64,
    pub live_pool_conns: Mutex<HashSet<usize>>,
}

impl PoolManager {
    pub fn new(pool_size: usize, pool_max: usize) -> Self {
        // Allocate slots up to runtime max so auto-grow can expand
        // without reallocation. Only `configured_size` limits active use.
        let effective_max = pool_max.max(1);
        let mut slots = Vec::with_capacity(effective_max);
        for _ in 0..effective_max {
            slots.push(PoolSlot::new());
        }
        let effective_size = pool_size.clamp(1, effective_max);
        Self {
            slots,
            configured_size: AtomicUsize::new(effective_size),
            configured_max_size: AtomicUsize::new(effective_max),
            idle_timeout_secs: AtomicU32::new(300),
            library_db_path: Mutex::new(None),
            last_reap_time: AtomicI64::new(0),
            init_pid: AtomicU32::new(std::process::id()),
            registry: ConnectionRegistry::new(),
            db_to_pool: DbToPool::new(),
            global_metadata_id: AtomicI64::new(0),
            global_last_insert_rowid: AtomicI64::new(0),
            live_pool_conns: Mutex::new(HashSet::new()),
        }
    }

    /// TLS teardown only touches initialized atomics: no locks, allocation or libpq.
    pub fn retire_thread_owner(&self, token: u64) {
        use super::threading::DEAD_BIT;
        if token == 0 || token >= DEAD_BIT {
            return;
        }
        for slot in &self.slots {
            let _ = slot.owner_thread.compare_exchange(
                token,
                token | DEAD_BIT,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }

    /// Get configured pool size.
    pub fn pool_size(&self) -> usize {
        self.configured_size
            .load(Ordering::Relaxed)
            .min(self.pool_max())
    }

    /// Get configured maximum pool size (runtime cap).
    pub fn pool_max(&self) -> usize {
        self.configured_max_size.load(Ordering::Relaxed)
    }

    fn find_connection_slot(&self, conn_ptr: *const c_void) -> Option<(usize, u8)> {
        if conn_ptr.is_null() {
            return None;
        }

        for (i, slot) in self.slots.iter().enumerate() {
            if slot.conn.load(Ordering::Acquire) == conn_ptr as *mut c_void {
                return Some((i, slot.state.load(Ordering::Acquire)));
            }
        }

        None
    }

    /// Check if a connection pointer is in any READY pool slot.
    pub fn validate_connection(&self, conn_ptr: *const c_void) -> bool {
        matches!(self.find_connection_slot(conn_ptr), Some((_, SLOT_READY)))
    }

    /// Best-effort release of a stale streaming flag for a tracked pool connection.
    ///
    /// This is intentionally more permissive than `validate_connection`: it scans all
    /// allocated slots and does not require the slot to be READY, because cleanup can
    /// race with slot state transitions even while the connection pointer is still live
    /// in the pool.
    pub fn clear_streaming_active(&self, conn_ptr: *const c_void) -> bool {
        if conn_ptr.is_null() {
            return false;
        }

        if let Some((i, state)) = self.find_connection_slot(conn_ptr) {
            unsafe {
                (*(conn_ptr as *mut PgConnection))
                    .streaming_active
                    .store(0, Ordering::Release);
            }

            log_info_lazy!(
                "Pool: cleared stale streaming_active flag for slot {} (state={})",
                i,
                state
            );
            return true;
        }

        false
    }

    pub fn note_live_pool_connection(&self, conn_ptr: *const c_void) {
        if conn_ptr.is_null() {
            return;
        }
        mutex_lock(&self.live_pool_conns).insert(conn_ptr as usize);
    }

    pub fn forget_live_pool_connection(&self, conn_ptr: *const c_void) {
        if conn_ptr.is_null() {
            return;
        }
        mutex_lock(&self.live_pool_conns).remove(&(conn_ptr as usize));
    }

    pub fn is_live_pool_connection(&self, conn_ptr: *const c_void) -> bool {
        if conn_ptr.is_null() {
            return false;
        }
        mutex_lock(&self.live_pool_conns).contains(&(conn_ptr as usize))
    }

    /// Check whether a connection pointer is one we still track, either as a
    /// live pool connection or as a registered non-pooled handle connection.
    pub fn is_tracked_connection(&self, conn_ptr: *const c_void) -> bool {
        if conn_ptr.is_null() {
            return false;
        }

        self.is_live_pool_connection(conn_ptr) || self.registry.contains_conn(conn_ptr as usize)
    }

    /// Update last_used timestamp for a connection in the pool.
    pub fn touch_connection(&self, conn_ptr: *const c_void, now: i64) {
        let size = self.pool_size();
        for i in 0..size {
            let slot = &self.slots[i];
            if slot.conn.load(Ordering::Acquire) == conn_ptr as *mut c_void {
                slot.last_used.store(now, Ordering::Release);
                return;
            }
        }
    }

    /// Reset all pool state for child process after fork.
    /// Does NOT close connections (they belong to the parent).
    pub fn reset_for_child(&self) {
        let size = self.pool_size();
        for i in 0..size {
            let slot = &self.slots[i];
            slot.conn.store(std::ptr::null_mut(), Ordering::Release);
            slot.owner_thread.store(0, Ordering::Release);
            slot.last_used.store(0, Ordering::Release);
            slot.state.store(SLOT_FREE, Ordering::Release);
            slot.generation.fetch_add(1, Ordering::SeqCst);
        }
        self.registry.clear();
        self.db_to_pool.clear();
        self.init_pid.store(std::process::id(), Ordering::Release);
        tls_pool_cache_clear();
    }

    /// Scan pool for idle connections past the timeout.
    /// Returns a vec of (slot_index, conn_ptr) pairs that should be destroyed
    /// by calling C-side PQfinish. The slot's generation is bumped and state
    /// set to FREE before returning, so no other thread can use the conn.
    pub fn reap_idle(&self, now: i64) -> Vec<(usize, *mut c_void)> {
        let timeout = self.idle_timeout_secs.load(Ordering::Relaxed) as i64;
        let size = self.pool_size();
        let mut to_destroy = Vec::new();

        for i in 0..size {
            let slot = &self.slots[i];
            let state = slot.state.load(Ordering::Acquire);

            if state != SLOT_FREE {
                continue;
            }
            let conn = slot.conn.load(Ordering::Acquire);
            if conn.is_null() {
                continue;
            }
            let last_used = slot.last_used.load(Ordering::Acquire);
            if now - last_used < timeout {
                continue;
            }

            if !slot.try_claim_free() {
                continue;
            }

            slot.generation.fetch_add(1, Ordering::SeqCst);

            let conn = slot.conn.swap(std::ptr::null_mut(), Ordering::SeqCst);

            slot.owner_thread.store(0, Ordering::Release);
            slot.state.store(SLOT_FREE, Ordering::Release);

            if !conn.is_null() {
                to_destroy.push((i, conn));
            }
        }

        to_destroy
    }
}

pub(crate) static POOL: OnceLock<PoolManager> = OnceLock::new();

/// Get or initialize the global pool manager.
pub(crate) fn pool() -> &'static PoolManager {
    POOL.get_or_init(|| PoolManager::new(POOL_SIZE_DEFAULT, POOL_SIZE_DEFAULT))
}
