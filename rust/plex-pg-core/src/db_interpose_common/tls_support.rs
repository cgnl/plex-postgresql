use std::cell::{Cell, UnsafeCell};
use std::mem::size_of;
use std::os::raw::{c_char, c_int, c_long, c_void};
use std::ptr;
use std::sync::OnceLock;

#[repr(C)]
struct TlsState {
    in_interpose_call: c_int,
    prepare_v2_depth: c_int,
    in_resolve_tables: c_int,
    value_type_calls: c_long,
    column_type_calls: c_long,
    last_query: *const c_char,
}

// pthread keys, including zero, are valid only when key_create succeeded.
static TLS_KEY: OnceLock<Option<libc::pthread_key_t>> = OnceLock::new();

thread_local! {
    // Allocation/key exhaustion must not silently share mutable recursion,
    // query and counter state between threads. Constant initialization needs
    // no allocator or destructor and works for the surviving thread after fork.
    static TLS_FALLBACK_ACTIVE: Cell<bool> = const { Cell::new(false) };
    static TLS_FALLBACK: UnsafeCell<TlsState> = const { UnsafeCell::new(TlsState {
        in_interpose_call: 0,
        prepare_v2_depth: 0,
        in_resolve_tables: 0,
        value_type_calls: 0,
        column_type_calls: 0,
        last_query: ptr::null(),
    }) };
}

fn fallback_state() -> *mut TlsState {
    // Once returned, this address must remain valid as this thread's state,
    // even if a temporary allocation/registration failure subsequently clears.
    TLS_FALLBACK_ACTIVE.with(|active| active.set(true));
    TLS_FALLBACK.with(UnsafeCell::get)
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    static mut __stderrp: *mut libc::FILE;
}

#[cfg(not(target_os = "macos"))]
unsafe extern "C" {
    static mut stderr: *mut libc::FILE;
}

#[inline]
pub(crate) unsafe fn stderr_ptr() -> *mut libc::FILE {
    #[cfg(target_os = "macos")]
    {
        __stderrp
    }
    #[cfg(not(target_os = "macos"))]
    {
        stderr
    }
}

unsafe extern "C" fn tls_destructor(ptr: *mut c_void) {
    if !ptr.is_null() {
        libc::free(ptr);
        #[cfg(test)]
        tests::DEALLOCATIONS.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }
}

fn tls_key() -> Option<libc::pthread_key_t> {
    *TLS_KEY.get_or_init(|| unsafe {
        #[cfg(test)]
        if tests::FAILURE.load(std::sync::atomic::Ordering::Acquire) == 1 {
            return None;
        }
        let mut key: libc::pthread_key_t = 0;
        if libc::pthread_key_create(&mut key, Some(tls_destructor)) == 0 {
            Some(key)
        } else {
            None
        }
    })
}

unsafe fn tls_state() -> *mut TlsState {
    if TLS_FALLBACK_ACTIVE.with(Cell::get) {
        return fallback_state();
    }
    let Some(key) = tls_key() else {
        return fallback_state();
    };
    let ptr_val = libc::pthread_getspecific(key) as *mut TlsState;
    if !ptr_val.is_null() {
        return ptr_val;
    }
    #[cfg(test)]
    if tests::FAILURE.load(std::sync::atomic::Ordering::Acquire) == 2 {
        return fallback_state();
    }
    let new = libc::calloc(1, size_of::<TlsState>()) as *mut TlsState;
    if new.is_null() {
        return fallback_state();
    }
    #[cfg(test)]
    tests::ALLOCATIONS.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    #[cfg(test)]
    let fail_registration = tests::FAILURE.load(std::sync::atomic::Ordering::Acquire) == 3;
    #[cfg(not(test))]
    let fail_registration = false;
    if fail_registration || libc::pthread_setspecific(key, new as *mut c_void) != 0 {
        // Registration failure otherwise leaks a state on every access and
        // changes addresses within one thread. Keep using that thread's TLS.
        tls_destructor(new.cast());
        return fallback_state();
    }
    new
}

pub(crate) fn tls_in_interpose_call_ptr() -> *mut c_int {
    unsafe { ptr::addr_of_mut!((*tls_state()).in_interpose_call) }
}

pub(crate) fn tls_prepare_v2_depth_ptr() -> *mut c_int {
    unsafe { ptr::addr_of_mut!((*tls_state()).prepare_v2_depth) }
}

pub(crate) fn tls_in_resolve_tables_ptr() -> *mut c_int {
    unsafe { ptr::addr_of_mut!((*tls_state()).in_resolve_tables) }
}

pub(crate) fn tls_value_type_calls_ptr() -> *mut c_long {
    unsafe { ptr::addr_of_mut!((*tls_state()).value_type_calls) }
}

pub(crate) fn tls_column_type_calls_ptr() -> *mut c_long {
    unsafe { ptr::addr_of_mut!((*tls_state()).column_type_calls) }
}

pub(crate) fn tls_last_query_ptr() -> *mut *const c_char {
    unsafe { ptr::addr_of_mut!((*tls_state()).last_query) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

    pub(super) static FAILURE: AtomicUsize = AtomicUsize::new(0);
    pub(super) static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static DEALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

    unsafe fn assign(state: *mut TlsState, marker: c_int) {
        (*state).in_interpose_call = marker;
        (*state).prepare_v2_depth = marker + 1;
        (*state).in_resolve_tables = marker + 2;
        (*state).value_type_calls = (marker + 3).into();
        (*state).column_type_calls = (marker + 4).into();
        (*state).last_query = if marker == 100 {
            c"main query".as_ptr()
        } else {
            c"other query".as_ptr()
        };
    }

    unsafe fn check(state: *mut TlsState, marker: c_int) {
        assert_eq!((*state).in_interpose_call, marker);
        assert_eq!((*state).prepare_v2_depth, marker + 1);
        assert_eq!((*state).in_resolve_tables, marker + 2);
        assert_eq!((*state).value_type_calls, c_long::from(marker + 3));
        assert_eq!((*state).column_type_calls, c_long::from(marker + 4));
        assert_eq!(
            (*state).last_query,
            if marker == 100 {
                c"main query".as_ptr()
            } else {
                c"other query".as_ptr()
            }
        );
        assert_eq!(
            tls_in_interpose_call_ptr(),
            ptr::addr_of_mut!((*state).in_interpose_call)
        );
        assert_eq!(
            tls_prepare_v2_depth_ptr(),
            ptr::addr_of_mut!((*state).prepare_v2_depth)
        );
        assert_eq!(
            tls_in_resolve_tables_ptr(),
            ptr::addr_of_mut!((*state).in_resolve_tables)
        );
        assert_eq!(
            tls_value_type_calls_ptr(),
            ptr::addr_of_mut!((*state).value_type_calls)
        );
        assert_eq!(
            tls_column_type_calls_ptr(),
            ptr::addr_of_mut!((*state).column_type_calls)
        );
        assert_eq!(tls_last_query_ptr(), ptr::addr_of_mut!((*state).last_query));
    }

    unsafe fn fork_surviving_thread(state: *mut TlsState) {
        let child = libc::fork();
        assert!(child >= 0);
        if child == 0 {
            let inherited = tls_state();
            if inherited != state || (*inherited).prepare_v2_depth != 101 {
                libc::_exit(1);
            }
            (*inherited).prepare_v2_depth = 999;
            libc::_exit(0);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut status = 0;
        loop {
            let result = libc::waitpid(child, &mut status, libc::WNOHANG);
            if result == child {
                assert!(libc::WIFEXITED(status));
                assert_eq!(libc::WEXITSTATUS(status), 0);
                break;
            }
            assert_eq!(result, 0);
            if Instant::now() >= deadline {
                libc::kill(child, libc::SIGKILL);
                libc::waitpid(child, &mut status, 0);
                panic!("TLS access blocked after fork");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        check(state, 100);
    }

    fn isolated_case(mode: &str) {
        unsafe {
            let mut reserved = 0;
            assert_eq!(libc::pthread_key_create(&mut reserved, None), 0);
            #[cfg(target_os = "linux")]
            assert_eq!(
                reserved, 0,
                "fresh Linux process must exercise the first pthread key"
            );
            if mode != "nonzero" {
                assert_eq!(libc::pthread_key_delete(reserved), 0);
            }
            FAILURE.store(
                match mode {
                    "key-failure" => 1,
                    "allocation-failure" => 2,
                    "registration-failure" => 3,
                    _ => 0,
                },
                Ordering::Release,
            );
            let key = tls_key();
            if mode == "key-failure" {
                assert!(key.is_none());
            } else {
                assert!(key.is_some());
                if mode == "nonzero" {
                    assert_ne!(key.unwrap(), reserved);
                }
                #[cfg(target_os = "linux")]
                if mode == "zero" {
                    assert_eq!(key, Some(0));
                }
            }
            let main = tls_state();
            assert_eq!((*main).prepare_v2_depth, 0);
            assign(main, 100);
            let barrier = Arc::new(Barrier::new(9));
            let threads: Vec<_> = (1..=8)
                .map(|marker| {
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        let own = tls_state();
                        assert_eq!((*own).prepare_v2_depth, 0);
                        assign(own, marker);
                        barrier.wait();
                        for _ in 0..20 {
                            check(own, marker);
                        }
                        own as usize
                    })
                })
                .collect();
            barrier.wait();
            check(main, 100);
            let mut addresses = vec![main as usize];
            for thread in threads {
                addresses.push(thread.join().unwrap());
            }
            addresses.sort_unstable();
            addresses.dedup();
            assert_eq!(
                addresses.len(),
                9,
                "TLS storage must belong to individual threads"
            );
            check(main, 100);
            match mode {
                "zero" | "nonzero" => {
                    assert_eq!(ALLOCATIONS.load(Ordering::Acquire), 9);
                    assert_eq!(
                        DEALLOCATIONS.load(Ordering::Acquire),
                        8,
                        "thread exit must free each allocated state once"
                    );
                }
                "key-failure" | "allocation-failure" => {
                    assert_eq!(ALLOCATIONS.load(Ordering::Acquire), 0)
                }
                "registration-failure" => {
                    assert_eq!(ALLOCATIONS.load(Ordering::Acquire), 9);
                    assert_eq!(
                        ALLOCATIONS.load(Ordering::Acquire),
                        DEALLOCATIONS.load(Ordering::Acquire),
                        "failed registration must free every rejected allocation"
                    );
                }
                _ => panic!("unknown test case"),
            }
            // Recovery from temporary failure cannot move the state behind
            // pointers already handed to guards in this thread.
            FAILURE.store(0, Ordering::Release);
            check(main, 100);
            fork_surviving_thread(main);
            if mode == "nonzero" {
                assert_eq!(libc::pthread_key_delete(reserved), 0);
            }
        }
    }

    #[test]
    fn pthread_key_zero_and_failure_paths_are_thread_local() {
        const ENV: &str = "PLEX_TLS_REGRESSION_CASE";
        if let Ok(mode) = std::env::var(ENV) {
            isolated_case(&mode);
            return;
        }
        for mode in [
            "zero",
            "nonzero",
            "key-failure",
            "allocation-failure",
            "registration-failure",
        ] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "db_interpose_common::tls_support::tests::pthread_key_zero_and_failure_paths_are_thread_local", "--nocapture"])
                .env(ENV, mode).spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(
                        status.success(),
                        "isolated TLS case {mode} failed: {status}"
                    );
                    break;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("isolated TLS case {mode} exceeded deadline");
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}
