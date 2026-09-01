use std::os::raw::{c_char, c_int, c_void};

use crate::db_interpose_common::{
    pg_exception_get_last_column, pg_exception_get_last_query, stderr_ptr,
};

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod impl_unix {
    use super::*;
    use std::ffi::CStr;
    use std::ptr;

    type TerminateHandler = extern "C" fn();
    use crate::db_interpose_common::{CxaDemangleFn, CXA_DEMANGLE_FN};

    type CxaSetTerminateFn = unsafe extern "C" fn(TerminateHandler) -> TerminateHandler;
    type CxaGetExceptionPtrFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
    type CxaCurrentExceptionTypeFn = unsafe extern "C" fn() -> *mut c_void;
    type CxaCurrentPrimaryExceptionFn = unsafe extern "C" fn() -> *mut c_void;
    type CxaDecrementExceptionRefcountFn = unsafe extern "C" fn(*mut c_void);
    type DynamicCastFn =
        unsafe extern "C" fn(*const c_void, *const c_void, *const c_void, isize) -> *mut c_void;

    static mut PREV_TERMINATE: Option<TerminateHandler> = None;
    static mut STD_EXCEPTION_TINFO: *const c_void = ptr::null();
    static mut CXA_SET_TERMINATE: Option<CxaSetTerminateFn> = None;
    static mut CXA_GET_EXCEPTION_PTR: Option<CxaGetExceptionPtrFn> = None;
    static mut CXA_CURRENT_EXCEPTION_TYPE: Option<CxaCurrentExceptionTypeFn> = None;
    static mut CXA_CURRENT_PRIMARY_EXCEPTION: Option<CxaCurrentPrimaryExceptionFn> = None;
    static mut CXA_DECREMENT_EXCEPTION_REFCOUNT: Option<CxaDecrementExceptionRefcountFn> = None;
    static mut DYNAMIC_CAST: Option<DynamicCastFn> = None;

    #[inline]
    #[allow(dead_code)]
    unsafe fn read_option<T: Copy>(slot: *const Option<T>) -> Option<T> {
        ptr::read(slot)
    }

    unsafe fn ensure_cxa_demangle() -> Option<CxaDemangleFn> {
        let opt = CXA_DEMANGLE_FN.get_or_init(|| {
            let sym = load_sym_raw(b"__cxa_demangle\0");
            if !sym.is_null() {
                Some(std::mem::transmute::<*mut c_void, CxaDemangleFn>(sym))
            } else {
                None
            }
        });
        *opt
    }

    #[cfg(target_os = "macos")]
    const ABI_LIBS: [&[u8]; 1] = [b"libc++abi.dylib\0"];

    #[cfg(target_os = "linux")]
    const ABI_LIBS: [&[u8]; 8] = [
        b"/usr/lib/plexmediaserver/lib/libc++.so.2\0",
        b"/usr/lib/plexmediaserver/lib/libc++abi.so.1\0",
        b"libc++abi.so.1\0",
        b"libc++abi.so\0",
        b"libc++.so.2\0",
        b"libc++.so\0",
        b"libstdc++.so.6\0",
        b"libstdc++.so\0",
    ];

    unsafe fn load_sym_raw(sym: &[u8]) -> *mut c_void {
        let ptr = libc::dlsym(libc::RTLD_DEFAULT, sym.as_ptr() as *const c_char);
        if !ptr.is_null() {
            return ptr;
        }

        for lib in ABI_LIBS.iter() {
            let handle = libc::dlopen(
                lib.as_ptr() as *const c_char,
                libc::RTLD_NOW | libc::RTLD_LOCAL,
            );
            if handle.is_null() {
                continue;
            }
            let sym_ptr = libc::dlsym(handle, sym.as_ptr() as *const c_char);
            if !sym_ptr.is_null() {
                return sym_ptr;
            }
        }

        ptr::null_mut()
    }

    unsafe fn cxa_set_terminate() -> Option<CxaSetTerminateFn> {
        let slot = ptr::addr_of_mut!(CXA_SET_TERMINATE);
        let mut current = ptr::read(slot);
        if current.is_none() {
            let sym = load_sym_raw(b"__cxa_set_terminate\0");
            if !sym.is_null() {
                ptr::write(
                    slot,
                    Some(std::mem::transmute::<*mut c_void, CxaSetTerminateFn>(sym)),
                );
                current = ptr::read(slot);
            }
        }
        current
    }

    unsafe fn cxa_get_exception_ptr() -> Option<CxaGetExceptionPtrFn> {
        let slot = ptr::addr_of_mut!(CXA_GET_EXCEPTION_PTR);
        let mut current = ptr::read(slot);
        if current.is_none() {
            let sym = load_sym_raw(b"__cxa_get_exception_ptr\0");
            if !sym.is_null() {
                ptr::write(
                    slot,
                    Some(std::mem::transmute::<*mut c_void, CxaGetExceptionPtrFn>(
                        sym,
                    )),
                );
                current = ptr::read(slot);
            }
        }
        current
    }

    unsafe fn cxa_current_exception_type() -> Option<CxaCurrentExceptionTypeFn> {
        let slot = ptr::addr_of_mut!(CXA_CURRENT_EXCEPTION_TYPE);
        let mut current = ptr::read(slot);
        if current.is_none() {
            let sym = load_sym_raw(b"__cxa_current_exception_type\0");
            if !sym.is_null() {
                ptr::write(
                    slot,
                    Some(std::mem::transmute::<*mut c_void, CxaCurrentExceptionTypeFn>(sym)),
                );
                current = ptr::read(slot);
            }
        }
        current
    }

    unsafe fn cxa_current_primary_exception() -> Option<CxaCurrentPrimaryExceptionFn> {
        let slot = ptr::addr_of_mut!(CXA_CURRENT_PRIMARY_EXCEPTION);
        let mut current = ptr::read(slot);
        if current.is_none() {
            let sym = load_sym_raw(b"__cxa_current_primary_exception\0");
            if !sym.is_null() {
                ptr::write(
                    slot,
                    Some(std::mem::transmute::<
                        *mut c_void,
                        CxaCurrentPrimaryExceptionFn,
                    >(sym)),
                );
                current = ptr::read(slot);
            }
        }
        current
    }

    unsafe fn cxa_decrement_exception_refcount() -> Option<CxaDecrementExceptionRefcountFn> {
        let slot = ptr::addr_of_mut!(CXA_DECREMENT_EXCEPTION_REFCOUNT);
        let mut current = ptr::read(slot);
        if current.is_none() {
            let sym = load_sym_raw(b"__cxa_decrement_exception_refcount\0");
            if !sym.is_null() {
                ptr::write(
                    slot,
                    Some(std::mem::transmute::<
                        *mut c_void,
                        CxaDecrementExceptionRefcountFn,
                    >(sym)),
                );
                current = ptr::read(slot);
            }
        }
        current
    }

    unsafe fn dynamic_cast_fn() -> Option<DynamicCastFn> {
        let slot = ptr::addr_of_mut!(DYNAMIC_CAST);
        let mut current = ptr::read(slot);
        if current.is_none() {
            let sym = load_sym_raw(b"__dynamic_cast\0");
            if !sym.is_null() {
                ptr::write(
                    slot,
                    Some(std::mem::transmute::<*mut c_void, DynamicCastFn>(sym)),
                );
                current = ptr::read(slot);
            }
        }
        current
    }

    unsafe fn std_exception_tinfo() -> *const c_void {
        let mut current = ptr::read(ptr::addr_of!(STD_EXCEPTION_TINFO));
        if current.is_null() {
            let sym = load_sym_raw(b"_ZTISt9exception\0");
            if !sym.is_null() {
                current = sym as *const c_void;
            } else {
                let sym = load_sym_raw(b"_ZTISt3__19exception\0");
                if !sym.is_null() {
                    current = sym as *const c_void;
                }
            }
            if !current.is_null() {
                ptr::write(ptr::addr_of_mut!(STD_EXCEPTION_TINFO), current);
            }
        }
        current
    }

    // Assumes Itanium C++ ABI: vptr followed by `const char*` name.
    unsafe fn typeinfo_name(tinfo: *const c_void) -> *const c_char {
        if tinfo.is_null() {
            return ptr::null();
        }
        let fields = tinfo as *const *const c_void;
        *fields.add(1) as *const c_char
    }

    fn env_truthy(name: &[u8]) -> bool {
        crate::env_utils::env_truthy(name)
    }

    unsafe fn is_exception_like(type_name: *const c_char) -> bool {
        if type_name.is_null() {
            return false;
        }
        let raw = CStr::from_ptr(type_name).to_string_lossy();
        let mut name = raw.to_string();
        if let Some(demangle) = ensure_cxa_demangle() {
            let mut status: c_int = 0;
            let demangled = demangle(type_name, ptr::null_mut(), ptr::null_mut(), &mut status);
            if !demangled.is_null() {
                name = CStr::from_ptr(demangled).to_string_lossy().to_string();
                libc::free(demangled as *mut c_void);
            }
        }
        let lower = name.to_ascii_lowercase();
        lower.contains("exception") || lower.contains("runtime_error")
    }

    unsafe fn demangle_type_name(type_name: *const c_char) -> (*const c_char, *mut c_char) {
        let mut demangled: *mut c_char = ptr::null_mut();
        if let Some(demangle) = ensure_cxa_demangle() {
            if !type_name.is_null() {
                let mut status: c_int = 0;
                demangled = demangle(type_name, ptr::null_mut(), ptr::null_mut(), &mut status);
            }
        }
        let readable = if !demangled.is_null() {
            demangled
        } else {
            type_name
        };
        (readable, demangled)
    }

    extern "C" fn rust_terminate_logger() {
        unsafe {
            let mut type_name: *const c_char = b"unknown\0".as_ptr() as *const c_char;
            let mut what_buf: [c_char; 257] = [0; 257];
            let mut demangled: *mut c_char = ptr::null_mut();

            let tinfo = cxa_current_exception_type()
                .map(|f| f())
                .unwrap_or(ptr::null_mut());
            if !tinfo.is_null() {
                let name = typeinfo_name(tinfo);
                if !name.is_null() {
                    type_name = name;
                }
                let (readable, dem) = demangle_type_name(type_name);
                type_name = readable;
                demangled = dem;
            }

            if let Some(get_primary) = cxa_current_primary_exception() {
                let primary = get_primary();
                if !primary.is_null() && !tinfo.is_null() {
                    let _ = pg_exception_extract_what(
                        primary,
                        tinfo,
                        what_buf.as_mut_ptr(),
                        what_buf.len(),
                    );
                    if let Some(dec) = cxa_decrement_exception_refcount() {
                        dec(primary);
                    }
                }
            }

            let _ = libc::fprintf(
                stderr_ptr(),
                b"[EXC_TERMINATE] type=%s what=%s\n\0".as_ptr() as *const c_char,
                type_name,
                what_buf.as_ptr(),
            );

            let last_query = pg_exception_get_last_query();
            let last_column = pg_exception_get_last_column();
            if !last_query.is_null() && *last_query != 0 {
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[EXC_TERMINATE] last_query=%.220s\n\0".as_ptr() as *const c_char,
                    last_query,
                );
            }
            if !last_column.is_null() && *last_column != 0 {
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[EXC_TERMINATE] last_column=%.220s\n\0".as_ptr() as *const c_char,
                    last_column,
                );
            }
            let _ = libc::fflush(stderr_ptr());

            if !demangled.is_null() {
                libc::free(demangled as *mut c_void);
            }

            if let Some(prev) = ptr::read(ptr::addr_of!(PREV_TERMINATE)) {
                prev();
            }
            libc::abort();
        }
    }

    #[no_mangle]
    pub extern "C" fn pg_exception_install_terminate_logger() {
        unsafe {
            if let Some(set_term) = cxa_set_terminate() {
                let prev = set_term(rust_terminate_logger);
                ptr::write(ptr::addr_of_mut!(PREV_TERMINATE), Some(prev));
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[EXC_TERMINATE] __cxa_set_terminate installed\n\0".as_ptr() as *const c_char,
                );
            } else {
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[EXC_TERMINATE] WARNING: __cxa_set_terminate not found\n\0".as_ptr()
                        as *const c_char,
                );
            }
            let _ = libc::fflush(stderr_ptr());
        }
    }

    #[no_mangle]
    pub extern "C" fn pg_exception_extract_what(
        thrown_exception: *mut c_void,
        tinfo: *mut c_void,
        out_buf: *mut c_char,
        out_buf_len: usize,
    ) -> c_int {
        if out_buf.is_null() || out_buf_len == 0 {
            return 0;
        }
        unsafe {
            *out_buf = 0;
        }

        if thrown_exception.is_null() || tinfo.is_null() {
            return 0;
        }

        unsafe {
            let mut exception_obj = if env_truthy(b"PLEX_PG_EXCEPTION_ASSUME_OBJECT\0") {
                thrown_exception
            } else {
                let get_ptr = match cxa_get_exception_ptr() {
                    Some(f) => f,
                    None => {
                        return 0;
                    }
                };
                get_ptr(thrown_exception)
            };
            if exception_obj.is_null() {
                exception_obj = thrown_exception;
            }

            let dst_type = std_exception_tinfo();
            if dst_type.is_null() {
                return 0;
            }

            let mut as_std = if env_truthy(b"PLEX_PG_EXCEPTION_ASSUME_OBJECT\0") {
                exception_obj as *mut c_void
            } else {
                let dyn_cast = match dynamic_cast_fn() {
                    Some(f) => f,
                    None => {
                        return 0;
                    }
                };

                dyn_cast(
                    exception_obj as *const c_void,
                    tinfo as *const c_void,
                    dst_type,
                    -1,
                )
            };

            if as_std.is_null() {
                if tinfo as *const c_void == dst_type
                    || is_exception_like(typeinfo_name(tinfo as *const c_void))
                {
                    as_std = exception_obj as *mut c_void;
                } else {
                    return 0;
                }
            }

            let vtable = *(as_std as *const *const *const c_void);
            if vtable.is_null() {
                return 0;
            }
            let what_ptr = *vtable.add(2);
            if what_ptr.is_null() {
                return 0;
            }

            let what_fn: extern "C" fn(*const c_void) -> *const c_char = std::mem::transmute::<
                *const c_void,
                extern "C" fn(*const c_void) -> *const c_char,
            >(what_ptr);
            let msg = what_fn(as_std as *const c_void);
            if msg.is_null() || *msg == 0 {
                return 0;
            }

            let _ = libc::snprintf(out_buf, out_buf_len, b"%s\0".as_ptr() as *const c_char, msg);
            if *out_buf != 0 {
                1
            } else {
                0
            }
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub use impl_unix::{pg_exception_extract_what, pg_exception_install_terminate_logger};

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
#[no_mangle]
pub extern "C" fn pg_exception_install_terminate_logger() {}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
#[no_mangle]
pub extern "C" fn pg_exception_extract_what(
    _thrown_exception: *mut c_void,
    _tinfo: *mut c_void,
    out_buf: *mut c_char,
    out_buf_len: usize,
) -> c_int {
    if out_buf.is_null() || out_buf_len == 0 {
        return 0;
    }
    unsafe {
        *out_buf = 0;
    }
    0
}
