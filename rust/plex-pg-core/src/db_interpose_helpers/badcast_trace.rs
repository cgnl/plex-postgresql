use super::{
    cstr_to_str_or_empty, getenv_nonempty, read_first_line_trimmed, trim_first_line, write_buf,
};
use std::ffi::CStr;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::raw::{c_char, c_int};

pub fn rust_env_truthy(value: *const c_char) -> c_int {
    let s = unsafe { cstr_to_str_or_empty(value) };
    if s.is_empty() {
        return 0;
    }
    matches!(s.as_bytes()[0], b'1' | b'y' | b'Y' | b't' | b'T') as c_int
}

pub fn rust_read_first_line_trim_to_buf(
    path: *const c_char,
    out: *mut c_char,
    out_len: usize,
) -> c_int {
    if path.is_null() || out.is_null() || out_len < 2 {
        return 0;
    }
    unsafe {
        *out = 0;
    }

    let path = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };

    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return 0,
    };
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return 0;
    }
    let Some(trimmed) = trim_first_line(&line) else {
        return 0;
    };

    let bytes = trimmed.as_bytes();
    let n = bytes.len().min(out_len - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.cast::<u8>(), n);
        *out.add(n) = 0;
    }
    1
}

pub fn rust_trace_prepare_sql_ok(_sql: *const c_char) -> c_int {
    1
}

pub fn rust_load_badcast_config(
    enabled_out: *mut c_int,
    idx_out: *mut c_char,
    idx_len: usize,
    thread_out: *mut c_char,
    thread_len: usize,
    sql_out: *mut c_char,
    sql_len: usize,
    col_out: *mut c_char,
    col_len: usize,
) -> c_int {
    let enabled = if let Some(v) = getenv_nonempty("PLEX_PG_TRACE_BADCAST") {
        i32::from(v != "0")
    } else if let Some(v) = getenv_nonempty("PLEX_PG_LOG_LEVEL") {
        i32::from(v.eq_ignore_ascii_case("ERROR"))
    } else {
        0
    };

    let idx = getenv_nonempty("PLEX_PG_TRACE_BADCAST_IDX")
        .or_else(|| read_first_line_trimmed("/tmp/plex_pg_trace_badcast_idx"));
    let thread = getenv_nonempty("PLEX_PG_TRACE_BADCAST_THREAD")
        .or_else(|| read_first_line_trimmed("/tmp/plex_pg_trace_badcast_thread"));
    let sql = getenv_nonempty("PLEX_PG_TRACE_BADCAST_SQL_CONTAINS")
        .or_else(|| read_first_line_trimmed("/tmp/plex_pg_trace_badcast_sql_contains"));
    let col = getenv_nonempty("PLEX_PG_TRACE_BADCAST_COL_CONTAINS")
        .or_else(|| read_first_line_trimmed("/tmp/plex_pg_trace_badcast_col_contains"));

    if !enabled_out.is_null() {
        unsafe {
            *enabled_out = enabled;
        }
    }
    write_buf(idx_out, idx_len, idx.as_deref());
    write_buf(thread_out, thread_len, thread.as_deref());
    write_buf(sql_out, sql_len, sql.as_deref());
    write_buf(col_out, col_len, col.as_deref());

    1
}
