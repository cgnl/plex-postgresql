use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};

#[test]
fn cxa_demangle_known_types_if_available() {
    unsafe {
        let sym = libc::dlsym(libc::RTLD_DEFAULT, c"__cxa_demangle".as_ptr());
        if sym.is_null() {
            return;
        }
        let demangle: unsafe extern "C" fn(
            *const c_char,
            *mut c_char,
            *mut libc::size_t,
            *mut c_int,
        ) -> *mut c_char = std::mem::transmute(sym);

        let mut status: c_int = 0;
        let name = CString::new("_ZN5boost8bad_castE").unwrap();
        let demangled = demangle(
            name.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut status,
        );
        if status == 0 && !demangled.is_null() {
            let s = CStr::from_ptr(demangled).to_string_lossy().to_string();
            libc::free(demangled as *mut libc::c_void);
            assert!(s.contains("bad_cast"));
        }

        let mut status2: c_int = 0;
        let name2 = CString::new("_ZNSt13runtime_errorD1Ev").unwrap();
        let demangled2 = demangle(
            name2.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut status2,
        );
        if status2 == 0 && !demangled2.is_null() {
            let s = CStr::from_ptr(demangled2).to_string_lossy().to_string();
            libc::free(demangled2 as *mut libc::c_void);
            assert!(s.contains("runtime_error") || s.contains("exception"));
        }
    }
}

#[test]
fn cxa_demangle_invalid_name_returns_error_if_available() {
    unsafe {
        let sym = libc::dlsym(libc::RTLD_DEFAULT, c"__cxa_demangle".as_ptr());
        if sym.is_null() {
            return;
        }
        let demangle: unsafe extern "C" fn(
            *const c_char,
            *mut c_char,
            *mut libc::size_t,
            *mut c_int,
        ) -> *mut c_char = std::mem::transmute(sym);

        let mut status: c_int = 0;
        let name = CString::new("not_a_mangled_name").unwrap();
        let demangled = demangle(
            name.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut status,
        );
        if !demangled.is_null() {
            libc::free(demangled as *mut libc::c_void);
        }
        assert!(status == -2 || demangled.is_null());
    }
}
