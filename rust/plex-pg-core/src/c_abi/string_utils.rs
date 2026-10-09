use std::ffi::CStr;
use std::os::raw::c_char;

#[no_mangle]
pub extern "C" fn safe_strcasestr(haystack: *const c_char, needle: *const c_char) -> *mut c_char {
    if haystack.is_null() || needle.is_null() {
        return std::ptr::null_mut();
    }
    // Safety: caller guarantees valid NUL-terminated strings
    let needle_bytes = unsafe { CStr::from_ptr(needle) }.to_bytes();
    if needle_bytes.is_empty() {
        return haystack as *mut c_char;
    }

    let needle_len = needle_bytes.len();
    let mut p = haystack;
    unsafe {
        while *p != 0 {
            if libc::strncasecmp(p, needle, needle_len) == 0 {
                return p as *mut c_char;
            }
            p = p.add(1);
        }
    }
    std::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn str_replace_nocase(
    str_ptr: *const c_char,
    old_ptr: *const c_char,
    new_ptr: *const c_char,
) -> *mut c_char {
    if str_ptr.is_null() || old_ptr.is_null() || new_ptr.is_null() {
        return std::ptr::null_mut();
    }

    // Safety: caller guarantees valid NUL-terminated strings
    let old_len = unsafe { CStr::from_ptr(old_ptr) }.to_bytes().len();
    if old_len == 0 {
        return unsafe { libc::strdup(str_ptr) };
    }

    let mut out = Vec::new();
    let mut p = str_ptr;
    loop {
        let match_ptr = safe_strcasestr(p, old_ptr);
        if match_ptr.is_null() {
            // Safety: p points to a valid NUL-terminated C string
            let tail = unsafe { CStr::from_ptr(p) }.to_bytes();
            if !tail.is_empty() {
                out.extend_from_slice(tail);
            }
            break;
        }

        let prefix_len = unsafe { match_ptr.offset_from(p) as usize };
        if prefix_len > 0 {
            let prefix = unsafe { std::slice::from_raw_parts(p.cast::<u8>(), prefix_len) };
            out.extend_from_slice(prefix);
        }

        // Safety: new_ptr is a valid NUL-terminated C string
        let new_bytes = unsafe { CStr::from_ptr(new_ptr) }.to_bytes();
        if !new_bytes.is_empty() {
            out.extend_from_slice(new_bytes);
        }

        p = unsafe { match_ptr.add(old_len) };
    }

    out.push(0);
    unsafe {
        let buf = libc::malloc(out.len()) as *mut u8;
        if buf.is_null() {
            return std::ptr::null_mut();
        }
        std::ptr::copy_nonoverlapping(out.as_ptr(), buf, out.len());
        buf as *mut c_char
    }
}
