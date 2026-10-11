#[cfg(target_os = "linux")]
use super::{linux_process_name_is_primary, linux_process_name_requires_passthrough};
use super::{
    rust_common_handle_exception, rust_common_load_sqlite_symbols, rust_get_exception_tracker,
    rust_reset_exception_tracking, rust_simple_str_replace, tls_column_type_calls_ptr,
    tls_last_query_ptr, tls_value_type_calls_ptr, total_exception_count,
};
use libc::{c_void, RTLD_DEFAULT, RTLD_LAZY};
use std::ffi::{CStr, CString};
use std::sync::atomic::Ordering;
use std::sync::Mutex;

// These tests reset process-wide counters and inspect returned tracker pointers.
// Hold the test lock across reset, lookup and assertions so another test cannot
// reset or overwrite the tracker while a pointer is being inspected.
static EXCEPTION_TEST_LOCK: Mutex<()> = Mutex::new(());

fn call_replace(input: Option<&str>, old: Option<&str>, new_str: Option<&str>) -> Option<String> {
    let input_cs = input.map(|s| CString::new(s).unwrap());
    let old_cs = old.map(|s| CString::new(s).unwrap());
    let new_cs = new_str.map(|s| CString::new(s).unwrap());

    let ptr = rust_simple_str_replace(
        input_cs.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
        old_cs.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
        new_cs.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
    );

    if ptr.is_null() {
        return None;
    }

    let out = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe {
        libc::free(ptr as *mut c_void);
    }
    Some(out)
}

#[test]
fn common_helpers_simple_str_replace_null_str_returns_none() {
    assert!(call_replace(None, Some("old"), Some("new")).is_none());
}

#[test]
fn common_helpers_simple_str_replace_null_old_returns_none() {
    assert!(call_replace(Some("hello"), None, Some("new")).is_none());
}

#[test]
fn common_helpers_simple_str_replace_null_new_returns_none() {
    assert!(call_replace(Some("hello"), Some("old"), None).is_none());
}

#[test]
fn common_helpers_simple_str_replace_no_match_returns_none() {
    assert!(call_replace(Some("hello world"), Some("xyz"), Some("abc")).is_none());
}

#[test]
fn common_helpers_simple_str_replace_basic_replace() {
    assert_eq!(
        call_replace(Some("hello world"), Some("world"), Some("earth")),
        Some("hello earth".to_string())
    );
}

#[test]
fn common_helpers_simple_str_replace_at_start() {
    assert_eq!(
        call_replace(Some("hello world"), Some("hello"), Some("goodbye")),
        Some("goodbye world".to_string())
    );
}

#[test]
fn common_helpers_simple_str_replace_at_end() {
    assert_eq!(
        call_replace(Some("hello world"), Some("world"), Some("!")),
        Some("hello !".to_string())
    );
}

#[test]
fn common_helpers_simple_str_replace_shorter_with_longer() {
    assert_eq!(
        call_replace(Some("ab"), Some("a"), Some("xyz")),
        Some("xyzb".to_string())
    );
}

#[test]
fn common_helpers_simple_str_replace_longer_with_shorter() {
    assert_eq!(
        call_replace(Some("hello world"), Some("hello"), Some("hi")),
        Some("hi world".to_string())
    );
}

#[test]
fn common_helpers_simple_str_replace_delete_segment() {
    assert_eq!(
        call_replace(Some("hello world"), Some("hello "), Some("")),
        Some("world".to_string())
    );
}

#[test]
fn common_helpers_simple_str_replace_empty_old_prepends() {
    assert_eq!(
        call_replace(Some("hello"), Some(""), Some("X")),
        Some("Xhello".to_string())
    );
}

#[test]
fn common_helpers_simple_str_replace_first_occurrence_only() {
    assert_eq!(
        call_replace(Some("aaa"), Some("a"), Some("b")),
        Some("baa".to_string())
    );
}

#[test]
fn common_helpers_simple_str_replace_sql_transform() {
    assert_eq!(
        call_replace(
            Some("INSERT OR REPLACE INTO tags"),
            Some("INSERT OR REPLACE INTO"),
            Some("INSERT INTO")
        ),
        Some("INSERT INTO tags".to_string())
    );
}

#[test]
fn exception_tracker_increments_for_same_type() {
    let _guard = EXCEPTION_TEST_LOCK.lock().unwrap();
    rust_reset_exception_tracking();
    let name = CString::new("TestException").unwrap();

    let t1 = rust_get_exception_tracker(name.as_ptr());
    assert!(!t1.is_null());
    assert_eq!(unsafe { (*t1).count }, 1);

    let t2 = rust_get_exception_tracker(name.as_ptr());
    assert!(!t2.is_null());
    assert_eq!(unsafe { (*t2).count }, 2);
}

#[test]
fn exception_tracking_reset_clears_counts() {
    let _guard = EXCEPTION_TEST_LOCK.lock().unwrap();
    rust_reset_exception_tracking();
    let name = CString::new("ResetException").unwrap();
    let t1 = rust_get_exception_tracker(name.as_ptr());
    assert_eq!(unsafe { (*t1).count }, 1);

    rust_reset_exception_tracking();
    let t2 = rust_get_exception_tracker(name.as_ptr());
    assert_eq!(unsafe { (*t2).count }, 1);
}

#[test]
fn common_handle_exception_increments_total_count() {
    let _guard = EXCEPTION_TEST_LOCK.lock().unwrap();
    rust_reset_exception_tracking();
    unsafe {
        *tls_column_type_calls_ptr() = 1;
        *tls_value_type_calls_ptr() = 0;
        *tls_last_query_ptr() = std::ptr::null();
    }

    let mut in_handler = 0;
    let mut should_call_original = 0;
    let rc = rust_common_handle_exception(
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        &mut in_handler,
        &mut should_call_original,
    );
    assert_eq!(rc, 0);
    assert_eq!(should_call_original, 1);
    assert_eq!(total_exception_count.load(Ordering::SeqCst), 1);

    unsafe {
        *tls_column_type_calls_ptr() = 0;
    }
}

#[test]
fn common_load_sqlite_symbols_sets_pointers() {
    unsafe {
        super::orig_sqlite3_open = None;
        super::orig_sqlite3_prepare_v2 = None;
        super::orig_sqlite3_column_decltype = None;
    }

    rust_common_load_sqlite_symbols(std::ptr::null_mut());
    unsafe {
        let open = super::orig_sqlite3_open;
        let prepare = super::orig_sqlite3_prepare_v2;
        assert!(open.is_none());
        assert!(prepare.is_none());
    }

    let mut handle = std::ptr::null_mut();
    let names = if cfg!(target_os = "macos") {
        vec![
            CString::new("libsqlite3.dylib").unwrap(),
            CString::new("/usr/lib/libsqlite3.dylib").unwrap(),
        ]
    } else {
        vec![
            CString::new("libsqlite3.so.0").unwrap(),
            CString::new("libsqlite3.so").unwrap(),
        ]
    };
    for name in names {
        unsafe {
            handle = libc::dlopen(name.as_ptr(), RTLD_LAZY);
        }
        if !handle.is_null() {
            break;
        }
    }
    if handle.is_null() {
        handle = RTLD_DEFAULT;
    }

    unsafe {
        rust_common_load_sqlite_symbols(handle);
        let open = super::orig_sqlite3_open;
        let prepare = super::orig_sqlite3_prepare_v2;
        let decltype = super::orig_sqlite3_column_decltype;
        assert!(open.is_some());
        assert!(prepare.is_some());
        assert!(decltype.is_some());
    }

    if handle != RTLD_DEFAULT {
        unsafe {
            libc::dlclose(handle);
        }
    }
}

#[test]
fn tls_state_is_thread_local() {
    unsafe {
        *tls_column_type_calls_ptr() = 111;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        unsafe {
            *tls_column_type_calls_ptr() = 222;
        }
        let val = unsafe { *tls_column_type_calls_ptr() };
        tx.send(val).unwrap();
    })
    .join()
    .unwrap();

    let other = rx.recv().unwrap();
    assert_eq!(other, 222);
    let main_val = unsafe { *tls_column_type_calls_ptr() };
    assert_eq!(main_val, 111);
}

#[cfg(target_os = "linux")]
#[test]
fn linux_primary_process_names_stay_active() {
    assert!(linux_process_name_is_primary("Plex Media Server"));
    assert!(linux_process_name_is_primary("Plex Media Serv"));
    assert!(linux_process_name_is_primary("Plex Media Scanner"));
    assert!(!linux_process_name_requires_passthrough(
        "Plex Media Scanner"
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_helper_process_names_switch_to_passthrough() {
    assert!(linux_process_name_requires_passthrough("PMS CPM"));
    assert!(linux_process_name_requires_passthrough("PMS ReqHandler"));
    assert!(linux_process_name_requires_passthrough("PMS FileWatcher"));
    assert!(!linux_process_name_requires_passthrough(""));
}
