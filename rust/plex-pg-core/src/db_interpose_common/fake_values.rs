use std::os::raw::c_uint;
use std::ptr;
use std::sync::Mutex;

pub(crate) const MAX_FAKE_VALUES: usize = 4096;
pub(crate) const PG_FAKE_VALUE_MAGIC: u32 = 0x50475641;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct PgFakeValue {
    pub magic: u32,
    pub pg_stmt: *mut libc::c_void,
    pub col_idx: libc::c_int,
    pub row_idx: libc::c_int,
    pub owner_thread: libc::pthread_t,
}

pub(crate) struct FakeValueState {
    pub(crate) pool: [PgFakeValue; MAX_FAKE_VALUES],
    pub(crate) next: c_uint,
}

// Safety: PgFakeValue contains *mut c_void which is !Send, but
// the pool is only accessed under the Mutex lock and the raw pointers
// are used as opaque handles that are valid across threads.
unsafe impl Send for FakeValueState {}

pub(crate) static FAKE_VALUES: Mutex<FakeValueState> = Mutex::new(FakeValueState {
    pool: [PgFakeValue {
        magic: 0,
        pg_stmt: ptr::null_mut(),
        col_idx: 0,
        row_idx: 0,
        owner_thread: 0 as libc::pthread_t,
    }; MAX_FAKE_VALUES],
    next: 0,
});

pub fn rust_pg_check_fake_value(p_val: *mut crate::ffi_types::sqlite3_value) -> *mut PgFakeValue {
    if p_val.is_null() {
        return ptr::null_mut();
    }
    let guard = FAKE_VALUES.lock().unwrap_or_else(|e| e.into_inner());
    let pool_ptr = guard.pool.as_ptr();
    let pool_start = pool_ptr as usize;
    let pool_end = unsafe { pool_ptr.add(MAX_FAKE_VALUES) } as usize;
    let ptr_val = p_val as usize;
    if ptr_val >= pool_start && ptr_val < pool_end {
        let fake = p_val as *mut PgFakeValue;
        if unsafe { (&*fake).magic } == PG_FAKE_VALUE_MAGIC {
            return fake;
        }
    }
    ptr::null_mut()
}
