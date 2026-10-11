use super::{normalize_sql_literals_impl, rewrite_server_library_uri_bytes};
use std::ffi::CStr;
use std::os::raw::{c_char, c_int};

#[repr(C)]
pub struct RustNormalizedSql {
    pub normalized_sql: *mut c_char,
    pub param_values: *mut *mut c_char,
    pub param_count: c_int,
}

pub fn rust_free_cstring(ptr: *mut c_char) {
    if ptr.is_null() {
        return;
    }
    unsafe {
        let _ = std::ffi::CString::from_raw(ptr);
    }
}

pub fn rust_validate_utf8(ptr: *const c_char, len: usize) -> i32 {
    if ptr.is_null() {
        return 0;
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
    i32::from(std::str::from_utf8(bytes).is_ok())
}

pub fn rust_rewrite_server_library_uri(
    input: *const c_char,
    out: *mut c_char,
    out_len: usize,
) -> i32 {
    if input.is_null() || out.is_null() || out_len < 16 {
        return 0;
    }

    let input_bytes = unsafe { CStr::from_ptr(input).to_bytes() };
    let out_cap = out_len.saturating_sub(1);
    let Some(out_buf) = rewrite_server_library_uri_bytes(input_bytes, out_cap) else {
        return 0;
    };

    let n = out_buf.len().min(out_cap);
    unsafe {
        if n > 0 {
            std::ptr::copy_nonoverlapping(out_buf.as_ptr(), out.cast::<u8>(), n);
        }
        *out.add(n) = 0;
    }

    1
}

pub fn rust_normalize_sql_literals(sql: *const c_char) -> *mut RustNormalizedSql {
    if sql.is_null() {
        return std::ptr::null_mut();
    }
    let raw = match unsafe { CStr::from_ptr(sql) }.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let Some((normalized_sql, params)) = normalize_sql_literals_impl(raw) else {
        return std::ptr::null_mut();
    };

    let normalized_sql = match std::ffi::CString::new(normalized_sql) {
        Ok(s) => s.into_raw(),
        Err(_) => return std::ptr::null_mut(),
    };

    let mut param_ptrs: Vec<*mut c_char> = Vec::with_capacity(params.len());
    for p in params {
        match std::ffi::CString::new(p) {
            Ok(s) => param_ptrs.push(s.into_raw()),
            Err(_) => {
                for ptr in param_ptrs {
                    if !ptr.is_null() {
                        unsafe {
                            let _ = std::ffi::CString::from_raw(ptr);
                        }
                    }
                }
                unsafe {
                    let _ = std::ffi::CString::from_raw(normalized_sql);
                }
                return std::ptr::null_mut();
            }
        }
    }

    let mut boxed_params = param_ptrs.into_boxed_slice();
    let param_values = boxed_params.as_mut_ptr();
    let param_count = boxed_params.len() as c_int;
    std::mem::forget(boxed_params);

    Box::into_raw(Box::new(RustNormalizedSql {
        normalized_sql,
        param_values,
        param_count,
    }))
}

pub fn rust_free_normalized_sql(n: *mut RustNormalizedSql) {
    if n.is_null() {
        return;
    }

    let n = unsafe { Box::from_raw(n) };
    if !n.normalized_sql.is_null() {
        unsafe {
            let _ = std::ffi::CString::from_raw(n.normalized_sql);
        }
    }

    if !n.param_values.is_null() && n.param_count > 0 {
        let len = n.param_count as usize;
        let slice_ptr = std::ptr::slice_from_raw_parts_mut(n.param_values, len);
        let params = unsafe { Box::from_raw(slice_ptr) };
        for p in params.iter().copied() {
            if !p.is_null() {
                unsafe {
                    let _ = std::ffi::CString::from_raw(p);
                }
            }
        }
    }
}
