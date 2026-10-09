use super::*;
use crate::log_debug_lazy;
use crate::log_info_lazy;

const WORKER_STACK_SIZE: usize = 8 * 1024 * 1024;

extern "C" fn worker_thread_func(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        log_info_lazy!(
            "WORKER: Thread started with {} MB stack",
            WORKER_STACK_SIZE / (1024 * 1024)
        );

        loop {
            let mut worker_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(worker_mutex));

            while worker_request.work_ready == 0 && worker_running != 0 {
                libc::pthread_cond_wait(
                    ptr::addr_of_mut!(worker_cond_request),
                    worker_guard.mutex_ptr(),
                );
            }

            if worker_running == 0 {
                worker_guard.unlock();
                break;
            }

            worker_request.work_ready = 0;

            if worker_request.type_ == WORK_SHUTDOWN {
                worker_request.work_done = 1;
                libc::pthread_cond_signal(ptr::addr_of_mut!(worker_cond_response));
                worker_guard.unlock();
                break;
            }

            if worker_request.type_ == WORK_PREPARE_V2 {
                let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
                let mut tail: *const c_char = ptr::null();
                let rc = crate::db_interpose_prepare::rust_my_sqlite3_prepare_v2_internal(
                    worker_request.db,
                    worker_request.z_sql,
                    worker_request.n_byte,
                    &mut stmt,
                    &mut tail,
                    1,
                );

                worker_request.stmt = stmt;
                worker_request.tail = tail;
                worker_request.result = rc;
            }

            worker_request.work_done = 1;
            libc::pthread_cond_signal(ptr::addr_of_mut!(worker_cond_response));
            worker_guard.unlock();
        }

        log_info("WORKER: Thread exiting");
        ptr::null_mut()
    }
}

pub fn rust_worker_init() -> c_int {
    unsafe {
        let mut attr = std::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
        if libc::pthread_attr_init(attr.as_mut_ptr()) != 0 {
            log_error("WORKER: Failed to init thread attributes");
            return -1;
        }
        let mut attr = attr.assume_init();

        if libc::pthread_attr_setstacksize(&mut attr as *mut _, WORKER_STACK_SIZE) != 0 {
            log_error("WORKER: Failed to set stack size");
            libc::pthread_attr_destroy(&mut attr as *mut _);
            return -1;
        }

        worker_running = 1;
        worker_request = EMPTY_WORKER_REQUEST;

        if libc::pthread_create(
            ptr::addr_of_mut!(worker_thread),
            &attr as *const _,
            worker_thread_func,
            ptr::null_mut(),
        ) != 0
        {
            log_error("WORKER: Failed to create thread");
            worker_running = 0;
            libc::pthread_attr_destroy(&mut attr as *mut _);
            return -1;
        }

        libc::pthread_attr_destroy(&mut attr as *mut _);
        log_info_lazy!(
            "WORKER: Initialized with {} MB stack",
            WORKER_STACK_SIZE / (1024 * 1024)
        );
    }

    0
}

#[cfg(target_os = "linux")]
pub(crate) unsafe fn fast_mark_fork_child_passthrough() {
    SHIM_PASSTHROUGH_ONLY.store(1, Ordering::Release);
    shim_init_pid = libc::getpid();
    CRASH_LAST_COLUMN_LEN.store(0, Ordering::SeqCst);
    GLOBAL_VALUE_TYPE_CALLS.store(0, Ordering::Relaxed);
    GLOBAL_COLUMN_TYPE_CALLS.store(0, Ordering::Relaxed);
    worker_thread = 0 as libc::pthread_t;
    worker_running = 0;
    worker_request = EMPTY_WORKER_REQUEST;
    rust_reset_symbol_verification();
}

pub fn rust_worker_cleanup() {
    unsafe {
        if worker_running == 0 {
            return;
        }

        let mut worker_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(worker_mutex));
        worker_request.type_ = WORK_SHUTDOWN;
        worker_request.work_ready = 1;
        worker_running = 0;
        libc::pthread_cond_signal(ptr::addr_of_mut!(worker_cond_request));
        worker_guard.unlock();

        libc::pthread_join(worker_thread, ptr::null_mut());
    }

    log_info("WORKER: Cleaned up");
}

pub fn rust_delegate_prepare_to_worker(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    unsafe {
        if worker_running == 0 {
            log_info("WORKER: Reinitializing after fork or deferred startup");
            if rust_worker_init() != 0 {
                log_error("WORKER: Not running, cannot delegate");
                return SQLITE_ERROR;
            }
        }

        let preview = crate::db_interpose_conn_utils::cstr_prefix(z_sql, 100, "NULL");
        log_debug_lazy!("WORKER: Delegating query ({})", preview);

        let mut worker_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(worker_mutex));

        worker_request.type_ = WORK_PREPARE_V2;
        worker_request.db = db;
        worker_request.z_sql = z_sql;
        worker_request.n_byte = n_byte;
        worker_request.stmt = ptr::null_mut();
        worker_request.tail = ptr::null();
        worker_request.result = SQLITE_ERROR;
        worker_request.work_done = 0;
        worker_request.work_ready = 1;

        libc::pthread_cond_signal(ptr::addr_of_mut!(worker_cond_request));

        while worker_request.work_done == 0 {
            libc::pthread_cond_wait(
                ptr::addr_of_mut!(worker_cond_response),
                worker_guard.mutex_ptr(),
            );
        }

        if !pp_stmt.is_null() {
            *pp_stmt = worker_request.stmt;
        }
        if !pz_tail.is_null() {
            *pz_tail = worker_request.tail;
        }
        let result = worker_request.result;

        worker_guard.unlock();

        log_debug_lazy!("WORKER: Delegation complete, rc={}", result);
        result
    }
}
