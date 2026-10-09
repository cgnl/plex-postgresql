use super::*;
use crate::log_debug_lazy;
use crate::log_info_lazy;

const WORKER_STACK_SIZE: usize = 8 * 1024 * 1024;

// A request remains owned by its caller even when pthread_cond_wait releases
// worker_mutex. Lifecycle operations share this admission lock so they cannot
// reset or stop the worker while a caller is waiting for its result.
static mut WORKER_ADMISSION: libc::pthread_mutex_t = libc::PTHREAD_MUTEX_INITIALIZER;
static WORKER_OWNER_PID: AtomicI32 = AtomicI32::new(0);

thread_local! {
    static IN_WORKER_CALL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct WorkerAdmissionGuard(PthreadMutexGuard);

impl WorkerAdmissionGuard {
    unsafe fn lock() -> Self {
        // Callback reentry on the submitting thread must fail as well: it
        // cannot reacquire admission while its outer call awaits the worker.
        IN_WORKER_CALL.with(|inside| inside.set(true));
        Self(PthreadMutexGuard::lock(ptr::addr_of_mut!(WORKER_ADMISSION)))
    }
}

impl Drop for WorkerAdmissionGuard {
    fn drop(&mut self) {
        unsafe {
            self.0.unlock();
        }
        IN_WORKER_CALL.with(|inside| inside.set(false));
    }
}

fn worker_access_allowed() -> bool {
    if SHIM_PASSTHROUGH_ONLY.load(Ordering::Acquire) != 0 {
        return false;
    }
    let pid = unsafe { libc::getpid() };
    let owner = WORKER_OWNER_PID.load(Ordering::Acquire);
    // Never acquire inherited locks in a fork child, including exported helper
    // calls that bypass the normal SQLite passthrough entry point.
    if owner != 0 && owner != pid {
        return false;
    }
    if owner == 0 {
        let owner = WORKER_OWNER_PID
            .compare_exchange(0, pid, Ordering::AcqRel, Ordering::Acquire)
            .unwrap_or_else(|existing| existing);
        if owner != 0 && owner != pid {
            return false;
        }
    }
    !IN_WORKER_CALL.with(std::cell::Cell::get)
}

unsafe fn prepare_worker_request(stmt: *mut *mut sqlite3_stmt, tail: *mut *const c_char) -> c_int {
    #[cfg(test)]
    if tests::USE_SQLITE.load(Ordering::Acquire) {
        return tests::prepare_sqlite(stmt, tail);
    }
    crate::db_interpose_prepare::rust_my_sqlite3_prepare_v2_internal(
        worker_request.db,
        worker_request.z_sql,
        worker_request.n_byte,
        stmt,
        tail,
        1,
    )
}

extern "C" fn worker_thread_func(_arg: *mut c_void) -> *mut c_void {
    IN_WORKER_CALL.with(|inside| inside.set(true));
    #[cfg(test)]
    if tests::USE_SQLITE.load(Ordering::Acquire) {
        tests::STARTS.fetch_add(1, Ordering::AcqRel);
    }
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
                let rc = prepare_worker_request(&mut stmt, &mut tail);

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
    if !worker_access_allowed() {
        return -1;
    }
    unsafe {
        let _admission = WorkerAdmissionGuard::lock();
        worker_init_admitted()
    }
}

// Caller holds WORKER_ADMISSION. The request mutex also protects worker_running
// against the worker thread, while admission serializes all lifecycle callers.
unsafe fn worker_init_admitted() -> c_int {
    let _worker = PthreadMutexGuard::lock(ptr::addr_of_mut!(worker_mutex));
    if worker_running != 0 {
        return 0;
    }
    {
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
    if !worker_access_allowed() {
        return;
    }
    unsafe {
        let _admission = WorkerAdmissionGuard::lock();
        let mut worker_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(worker_mutex));
        if worker_running == 0 {
            return;
        }
        worker_request.type_ = WORK_SHUTDOWN;
        worker_request.work_ready = 1;
        worker_running = 0;
        libc::pthread_cond_signal(ptr::addr_of_mut!(worker_cond_request));
        worker_guard.unlock();

        libc::pthread_join(worker_thread, ptr::null_mut());
        worker_thread = 0 as libc::pthread_t;
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
    // Reentrant worker calls must not wait on the caller currently awaiting us.
    // Normal prepare recursion uses from_worker=1 and does not delegate again.
    if !worker_access_allowed() {
        unsafe {
            if !pp_stmt.is_null() {
                *pp_stmt = ptr::null_mut();
            }
            if !pz_tail.is_null() {
                *pz_tail = ptr::null();
            }
        }
        return SQLITE_ERROR;
    }
    #[cfg(test)]
    tests::note_admission_attempt();
    unsafe {
        let _admission = WorkerAdmissionGuard::lock();
        if worker_init_admitted() != 0 {
            log_error("WORKER: Not running, cannot delegate");
            return SQLITE_ERROR;
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

        #[cfg(test)]
        if tests::pause_first_caller() {
            // Model a caller delayed while cond_wait has released its mutex.
            // The admission lock must still protect this caller's slot.
            worker_guard.unlock();
            tests::wait_for_caller_release();
            worker_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(worker_mutex));
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::ffi::SQLITE_OK;
    use std::ffi::CStr;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::time::{Duration, Instant};

    pub(super) static USE_SQLITE: AtomicBool = AtomicBool::new(false);
    static PAUSE: AtomicBool = AtomicBool::new(false);
    static PAUSED: AtomicBool = AtomicBool::new(false);
    static RELEASE: AtomicBool = AtomicBool::new(false);
    static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static STARTS: AtomicUsize = AtomicUsize::new(0);
    static PREPARES: AtomicUsize = AtomicUsize::new(0);
    static FINISHES: AtomicUsize = AtomicUsize::new(0);

    fn await_condition(condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "worker regression deadline exceeded"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    pub(super) fn note_admission_attempt() {
        if USE_SQLITE.load(Ordering::Acquire) {
            ATTEMPTS.fetch_add(1, Ordering::AcqRel);
        }
    }

    pub(super) fn pause_first_caller() -> bool {
        PAUSE.swap(false, Ordering::AcqRel)
    }

    pub(super) fn wait_for_caller_release() {
        PAUSED.store(true, Ordering::Release);
        await_condition(|| RELEASE.load(Ordering::Acquire));
    }

    pub(super) unsafe fn prepare_sqlite(
        stmt: *mut *mut sqlite3_stmt,
        tail: *mut *const c_char,
    ) -> c_int {
        // An accidental nested delegate/init from the worker must fail rather
        // than recursively waiting on its own outstanding request.
        assert_eq!(rust_worker_init(), -1);
        let mut nested = ptr::null_mut();
        assert_eq!(
            rust_delegate_prepare_to_worker(
                worker_request.db,
                worker_request.z_sql,
                -1,
                &mut nested,
                ptr::null_mut(),
            ),
            SQLITE_ERROR
        );
        assert!(nested.is_null());
        let result = rusqlite::ffi::sqlite3_prepare_v2(
            worker_request.db.cast(),
            worker_request.z_sql,
            worker_request.n_byte,
            stmt.cast(),
            tail,
        );
        PREPARES.fetch_add(1, Ordering::AcqRel);
        result
    }

    fn caller(marker: i64, iterations: usize) {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(&format!(
            "CREATE TABLE owner(value); INSERT INTO owner VALUES ({marker});"
        ))
        .unwrap();
        for _ in 0..iterations {
            let sql = std::ffi::CString::new(format!(
                "SELECT value + {marker} FROM owner; SELECT {marker};"
            ))
            .unwrap();
            let mut stmt = ptr::null_mut();
            let mut tail = ptr::null();
            unsafe {
                assert_eq!(
                    rust_delegate_prepare_to_worker(
                        db.handle().cast(),
                        sql.as_ptr(),
                        -1,
                        &mut stmt,
                        &mut tail
                    ),
                    SQLITE_OK
                );
                assert!(!stmt.is_null());
                assert!(
                    tail >= sql.as_ptr() && tail < sql.as_ptr().add(sql.as_bytes_with_nul().len())
                );
                assert_eq!(
                    CStr::from_ptr(tail).to_str().unwrap(),
                    format!(" SELECT {marker};")
                );
                assert_eq!(rusqlite::ffi::sqlite3_db_handle(stmt.cast()), db.handle());
                assert_eq!(
                    rusqlite::ffi::sqlite3_step(stmt.cast()),
                    rusqlite::ffi::SQLITE_ROW
                );
                assert_eq!(
                    rusqlite::ffi::sqlite3_column_int64(stmt.cast(), 0),
                    marker * 2
                );
                assert_eq!(
                    rusqlite::ffi::sqlite3_step(stmt.cast()),
                    rusqlite::ffi::SQLITE_DONE
                );
                // Each matching statement is finalized once by its own caller.
                assert_eq!(rusqlite::ffi::sqlite3_finalize(stmt.cast()), SQLITE_OK);
            }
            FINISHES.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn failing_caller(iterations: usize) {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE owner(value);").unwrap();
        let sql = std::ffi::CString::new("SELECT absent303 FROM owner; SELECT 303;").unwrap();
        for _ in 0..iterations {
            let mut stmt = ptr::null_mut();
            let mut tail = ptr::null();
            unsafe {
                assert_eq!(
                    rust_delegate_prepare_to_worker(
                        db.handle().cast(),
                        sql.as_ptr(),
                        -1,
                        &mut stmt,
                        &mut tail
                    ),
                    SQLITE_ERROR
                );
                assert!(
                    stmt.is_null(),
                    "failed caller received another caller's statement"
                );
                assert!(
                    tail >= sql.as_ptr() && tail < sql.as_ptr().add(sql.as_bytes_with_nul().len())
                );
                assert_eq!(CStr::from_ptr(tail).to_str().unwrap(), " SELECT 303;");
            }
        }
    }

    unsafe fn fork_with_inherited_locks() {
        let _admission = PthreadMutexGuard::lock(ptr::addr_of_mut!(WORKER_ADMISSION));
        let _request = PthreadMutexGuard::lock(ptr::addr_of_mut!(worker_mutex));
        let child = libc::fork();
        assert!(child >= 0);
        if child == 0 {
            // No allocation, logger, inherited mutex or worker restart in child.
            let init = rust_worker_init();
            let mut stmt = ptr::null_mut();
            let delegated = rust_delegate_prepare_to_worker(
                ptr::null_mut(),
                ptr::null(),
                -1,
                &mut stmt,
                ptr::null_mut(),
            );
            rust_worker_cleanup();
            #[cfg(target_os = "linux")]
            {
                fast_mark_fork_child_passthrough();
                if rust_worker_init() != -1 || SHIM_PASSTHROUGH_ONLY.load(Ordering::Acquire) != 1 {
                    libc::_exit(2);
                }
                rust_worker_cleanup();
            }
            libc::_exit(
                if init == -1 && delegated == SQLITE_ERROR && stmt.is_null() {
                    0
                } else {
                    1
                },
            );
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut status = 0;
        loop {
            let waited = libc::waitpid(child, &mut status, libc::WNOHANG);
            if waited == child {
                assert!(libc::WIFEXITED(status));
                assert_eq!(libc::WEXITSTATUS(status), 0);
                break;
            }
            assert_eq!(waited, 0);
            if Instant::now() >= deadline {
                libc::kill(child, libc::SIGKILL);
                libc::waitpid(child, &mut status, 0);
                panic!("fork child waited on an inherited worker lock");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn regression_child() {
        USE_SQLITE.store(true, Ordering::Release);
        let initializers: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(rust_worker_init))
            .collect();
        for initializer in initializers {
            assert_eq!(initializer.join().unwrap(), 0);
        }
        await_condition(|| STARTS.load(Ordering::Acquire) != 0);
        assert_eq!(
            STARTS.load(Ordering::Acquire),
            1,
            "simultaneous init created duplicate workers"
        );
        PAUSE.store(true, Ordering::Release);
        let first = std::thread::spawn(|| caller(101, 1));
        await_condition(|| PAUSED.load(Ordering::Acquire) && PREPARES.load(Ordering::Acquire) == 1);
        let second = std::thread::spawn(|| caller(202, 1));
        await_condition(|| ATTEMPTS.load(Ordering::Acquire) == 2);
        let cleanup_finished = std::sync::Arc::new(AtomicBool::new(false));
        let cleanup_flag = cleanup_finished.clone();
        let cleanup = std::thread::spawn(move || {
            rust_worker_cleanup();
            cleanup_flag.store(true, Ordering::Release);
        });
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            PREPARES.load(Ordering::Acquire),
            1,
            "second caller overwrote outstanding request"
        );
        assert!(
            !cleanup_finished.load(Ordering::Acquire),
            "cleanup overtook outstanding caller"
        );
        RELEASE.store(true, Ordering::Release);
        first.join().unwrap();
        second.join().unwrap();
        cleanup.join().unwrap();
        // Repeat deferred init and cleanup while callers use distinct handles.
        let callers: Vec<_> = (1..=4)
            .map(|id| std::thread::spawn(move || caller(id * 1000, 40)))
            .collect();
        let failures = std::thread::spawn(|| failing_caller(40));
        let cleaner = std::thread::spawn(|| {
            for _ in 0..20 {
                rust_worker_cleanup();
                assert_eq!(rust_worker_init(), 0);
            }
        });
        for caller in callers {
            caller.join().unwrap();
        }
        cleaner.join().unwrap();
        failures.join().unwrap();
        assert_eq!(PREPARES.load(Ordering::Acquire), 202);
        assert_eq!(FINISHES.load(Ordering::Acquire), 162);
        unsafe {
            fork_with_inherited_locks();
        }
        rust_worker_cleanup();
        rust_worker_cleanup();
    }

    #[test]
    fn worker_request_ownership_and_lifecycle() {
        const CHILD_ENV: &str = "PLEX_WORKER_REGRESSION_CHILD";
        if std::env::var_os(CHILD_ENV).is_some() {
            regression_child();
            return;
        }
        // Isolate exported global worker state from other concurrently running
        // unit tests, and contain deadlocks/crashes behind a process deadline.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "db_interpose_common::worker_runtime::tests::worker_request_ownership_and_lifecycle", "--nocapture"])
            .env(CHILD_ENV, "1")
            .spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "worker regression child failed: {status}");
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("worker regression subprocess exceeded deadline");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
