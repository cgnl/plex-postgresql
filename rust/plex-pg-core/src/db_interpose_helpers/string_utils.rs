use std::ffi::CStr;
use std::os::raw::c_char;

#[inline]
pub(super) fn cstr_to_str<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(ptr).to_str().ok() }
}

#[inline]
pub(crate) unsafe fn cstr_to_str_or_empty<'a>(ptr: *const c_char) -> &'a str {
    cstr_to_str(ptr).unwrap_or("")
}

#[inline]
pub(super) fn has_boundary(bytes: &[u8], idx: usize) -> bool {
    if idx >= bytes.len() {
        return true;
    }
    let b = bytes[idx];
    b == b'(' || b.is_ascii_whitespace()
}

#[inline]
pub(super) fn starts_with_icase(bytes: &[u8], pat: &[u8]) -> bool {
    if bytes.len() < pat.len() {
        return false;
    }
    bytes[..pat.len()].eq_ignore_ascii_case(pat)
}

pub(super) fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

pub(super) fn push_capped(buf: &mut Vec<u8>, out_cap: usize, bytes: &[u8]) {
    if out_cap == 0 || bytes.is_empty() {
        return;
    }
    let remaining = out_cap.saturating_sub(buf.len());
    if remaining == 0 {
        return;
    }
    let take = remaining.min(bytes.len());
    buf.extend_from_slice(&bytes[..take]);
}

pub(super) fn starts_with_ascii_icase_at(haystack: &[u8], at: usize, pat: &[u8]) -> bool {
    if haystack.len() < at + pat.len() {
        return false;
    }
    haystack[at..at + pat.len()].eq_ignore_ascii_case(pat)
}

pub(super) fn contains_ascii_icase(haystack: &[u8], pat: &[u8]) -> bool {
    if pat.is_empty() || haystack.len() < pat.len() {
        return false;
    }
    haystack
        .windows(pat.len())
        .any(|w| w.eq_ignore_ascii_case(pat))
}

pub(super) fn find_ascii_icase_from(haystack: &[u8], start: usize, pat: &[u8]) -> Option<usize> {
    if pat.is_empty() || haystack.len() < pat.len() || start >= haystack.len() {
        return None;
    }
    let mut i = start;
    while i + pat.len() <= haystack.len() {
        if haystack[i..i + pat.len()].eq_ignore_ascii_case(pat) {
            return Some(i);
        }
        i += 1;
    }
    None
}

pub(super) fn write_i64_to_buf(out: *mut c_char, out_len: usize, val: i64) -> bool {
    if out.is_null() || out_len == 0 {
        return false;
    }
    use std::io::Write;
    let buf = unsafe { std::slice::from_raw_parts_mut(out.cast::<u8>(), out_len) };
    let mut cursor = std::io::Cursor::new(buf);
    let _ = write!(cursor, "{}\0", val);
    true
}

pub(super) fn write_i32_to_buf(out: *mut c_char, out_len: usize, val: i32) -> bool {
    if out.is_null() || out_len == 0 {
        return false;
    }
    use std::io::Write;
    let buf = unsafe { std::slice::from_raw_parts_mut(out.cast::<u8>(), out_len) };
    let mut cursor = std::io::Cursor::new(buf);
    let _ = write!(cursor, "{}\0", val);
    true
}

pub(super) fn is_prev_numeric_boundary(prev: u8) -> bool {
    matches!(
        prev,
        b'=' | b'>' | b'<' | b' ' | b'(' | b',' | b'+' | b'-' | b'*' | b'/' | b'%'
    )
}

pub(super) fn is_next_numeric_boundary(bytes: &[u8], i: usize) -> bool {
    if i >= bytes.len() {
        return true;
    }
    let b = bytes[i];
    if matches!(
        b,
        b' ' | b')' | b',' | b';' | b'>' | b'<' | b'=' | b'+' | b'-' | b'*' | b'/'
    ) {
        return true;
    }
    starts_with_ascii_icase_at(bytes, i, b" AND")
        || starts_with_ascii_icase_at(bytes, i, b" OR")
        || starts_with_ascii_icase_at(bytes, i, b" ORDER")
        || starts_with_ascii_icase_at(bytes, i, b" LIMIT")
        || starts_with_ascii_icase_at(bytes, i, b" GROUP")
}

pub(super) fn find_ascii_icase(haystack: &[u8], pat: &[u8]) -> Option<usize> {
    find_ascii_icase_from(haystack, 0, pat)
}

pub(super) fn find_closing_paren(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b')' {
            return Some(i);
        }
        i += 1;
    }
    None
}

pub(super) fn split_csv_simple(section: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = section.as_bytes();
    let mut i = 0usize;
    let mut start = 0usize;
    let mut in_single = false;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            in_single = !in_single;
        } else if bytes[i] == b',' && !in_single {
            out.push(section[start..i].trim());
            start = i + 1;
        }
        i += 1;
    }
    if start <= section.len() {
        out.push(section[start..].trim());
    }
    out
}

pub(super) fn normalize_ident_token(t: &str) -> &str {
    let t = t.trim();
    let t = t.strip_prefix('"').unwrap_or(t);
    let t = t.strip_prefix('`').unwrap_or(t);
    let t = t.strip_suffix('"').unwrap_or(t);
    t.strip_suffix('`').unwrap_or(t)
}

pub(super) fn write_buf(out: *mut c_char, out_len: usize, value: Option<&str>) {
    if out.is_null() || out_len == 0 {
        return;
    }
    unsafe {
        *out = 0;
    }
    let Some(value) = value else {
        return;
    };
    let bytes = value.as_bytes();
    let n = bytes.len().min(out_len - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.cast::<u8>(), n);
        *out.add(n) = 0;
    }
}
