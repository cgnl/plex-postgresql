use super::*;

struct CachedTextState {
    col_name: *const c_char,
    oid: u32,
    source_value: *const c_char,
}

struct LiveTextState {
    row: c_int,
    col_name: *const c_char,
    _oid: u32,
    oid_u: c_uint,
}

fn empty_text_buffer() -> *const c_uchar {
    let buf_idx = next_text_buffer_index();
    let mut out_ptr: *const c_uchar = ptr::null();
    COLUMN_TEXT_BUFFERS.with(|bufs| {
        let mut bufs = bufs.borrow_mut();
        let buf = &mut bufs[buf_idx];
        buf[0] = 0;
        out_ptr = buf.as_ptr();
    });
    out_ptr
}

unsafe fn load_cached_text_state(pg_stmt: &mut PgStmt, idx: c_int) -> Option<CachedTextState> {
    let cached = &*pg_stmt.cached_result;
    let row = pg_stmt.current_row;
    if idx < 0 || idx >= cached.num_cols || row < 0 || row >= cached.num_rows {
        return None;
    }

    let crow = &*cached.rows.add(row as usize);
    if *crow.is_null.add(idx as usize) != 0 {
        return None;
    }

    let source_value = *crow.values.add(idx as usize);
    if source_value.is_null() {
        return None;
    }

    let col_name = if !cached.col_names.is_null() {
        *cached.col_names.add(idx as usize)
    } else {
        ptr::null()
    };
    let oid = if !cached.col_types.is_null() {
        *cached.col_types.add(idx as usize)
    } else {
        0
    };

    Some(CachedTextState {
        col_name,
        oid,
        source_value,
    })
}

/// Write cached text output into a thread-local buffer.
/// SAFETY: Must be called while stmt mutex is held. Does NOT call log_debug/log_error
/// to avoid deadlock with the LOGGER mutex.
unsafe fn write_cached_text_output(
    pg_stmt: &mut PgStmt,
    _idx: c_int,
    state: &CachedTextState,
) -> *const c_uchar {
    let str_len = libc::strlen(state.source_value) as usize;
    let buf_idx = next_text_buffer_index();
    let mut out_ptr: *const c_uchar = ptr::null();
    COLUMN_TEXT_BUFFERS.with(|bufs| {
        let mut bufs = bufs.borrow_mut();
        let buf = &mut bufs[buf_idx];
        let transform_rc = crate::db_interpose_helpers::rust_column_text_transform(
            state.col_name,
            state.oid as c_uint,
            pg_stmt.pg_sql,
            state.source_value,
            str_len,
            buf.as_mut_ptr() as *mut c_char,
            TEXT_BUFFER_SIZE,
        );
        if transform_rc == -1 || transform_rc == 1 {
            out_ptr = buf.as_ptr();
            return;
        }

        let copy_len = str_len.min(TEXT_BUFFER_SIZE - 1);
        if copy_len > 0 {
            ptr::copy_nonoverlapping(state.source_value as *const u8, buf.as_mut_ptr(), copy_len);
        }
        buf[copy_len] = 0;
        out_ptr = buf.as_ptr();
    });
    out_ptr
}

unsafe fn load_live_text_state(pg_stmt: &mut PgStmt, idx: c_int) -> Option<LiveTextState> {
    if pg_stmt.result.is_null() || idx < 0 || idx >= pg_stmt.num_cols {
        return None;
    }

    let row = pg_stmt.current_row;
    if row < 0 || row >= pg_stmt.num_rows {
        return None;
    }

    let mut is_null = 0;
    let mut oid_u: c_uint = 0;
    let mut sqlite_type = SQLITE_NULL;
    crate::db_interpose_helpers::rust_pg_result_type_info(
        helpers_result_ptr(pg_stmt.result),
        row,
        idx,
        &mut oid_u as *mut c_uint,
        &mut is_null as *mut c_int,
        &mut sqlite_type as *mut c_int,
    );
    if is_null != 0 {
        return None;
    }

    let col_name = crate::db_interpose_helpers::rust_pg_result_col_name(
        helpers_result_ptr(pg_stmt.result),
        idx,
    );

    Some(LiveTextState {
        row,
        col_name,
        _oid: oid_u as u32,
        oid_u,
    })
}

/// Write live (non-cached) text output into a thread-local buffer.
/// SAFETY: Must be called while stmt mutex is held. Does NOT call log_debug/log_error
/// to avoid deadlock with the LOGGER mutex.
unsafe fn write_live_text_output(
    pg_stmt: &mut PgStmt,
    idx: c_int,
    state: &LiveTextState,
) -> *const c_uchar {
    let buf_idx = next_text_buffer_index();
    let mut out_ptr: *const c_uchar = ptr::null();
    let mut preview = [0u8; 128];
    let mut source_len: usize = 0;
    let mut transform_rc: c_int = 0;
    COLUMN_TEXT_BUFFERS.with(|bufs| {
        let mut bufs = bufs.borrow_mut();
        let buf = &mut bufs[buf_idx];
        transform_rc = crate::db_interpose_helpers::rust_pg_result_text_transform_copy(
            helpers_result_ptr(pg_stmt.result),
            state.row,
            idx,
            state.col_name,
            state.oid_u,
            pg_stmt.pg_sql,
            0,
            buf.as_mut_ptr() as *mut c_char,
            TEXT_BUFFER_SIZE,
            preview.as_mut_ptr() as *mut c_char,
            preview.len(),
            &mut source_len as *mut usize,
        );
        out_ptr = buf.as_ptr();
    });

    if transform_rc == -2 {
        // PQgetvalue returned NULL for a non-NULL cell — likely a stale PGresult
        // race condition. Match the C shim behavior: return empty string instead
        // of NULL to prevent std::string(nullptr) → basic_string exception.
        return b"\0".as_ptr();
    }
    out_ptr
}

pub(super) fn column_text_impl(p_stmt: *mut sqlite3_stmt, idx: c_int) -> *const c_uchar {
    let dbg_stmt = pg_find_any_stmt(p_stmt);
    let dbg_sql = if !dbg_stmt.is_null() {
        let ds = unsafe { &*dbg_stmt };
        if !ds.pg_sql.is_null() {
            ds.pg_sql
        } else {
            ds.sql
        }
    } else {
        ptr::null()
    };
    let dbg_db = get_orig_sqlite3_db_handle()
        .map(|f| unsafe { f(p_stmt) })
        .unwrap_or(ptr::null_mut());
    unsafe {
        pg_exception_note_phase(
            b"column_text\0".as_ptr() as *const c_char,
            dbg_sql,
            p_stmt,
            dbg_db,
        );
    }

    validate_type_consistency(p_stmt, idx, "column_text");

    if dbg_stmt.is_null() || unsafe { (&*dbg_stmt).is_pg == 0 } {
        return get_orig_sqlite3_column_text()
            .map(|f| unsafe { f(p_stmt, idx) })
            .unwrap_or(ptr::null());
    }

    let pg_stmt = unsafe { &mut *dbg_stmt };

    // Hold mutex only for data extraction — no logging inside this block
    // to avoid ABBA deadlock between stmt mutex and LOGGER mutex.
    {
        let _guard = unsafe { PgStmt::lock_mutex(dbg_stmt) };

        if !pg_stmt.cached_result.is_null() {
            let cached = unsafe { &*pg_stmt.cached_result };
            let row = pg_stmt.current_row;
            if idx < 0 || idx >= cached.num_cols || row < 0 || row >= cached.num_rows {
                ptr::null()
            } else {
                match unsafe { load_cached_text_state(pg_stmt, idx) } {
                    Some(state) => unsafe { write_cached_text_output(pg_stmt, idx, &state) },
                    None => empty_text_buffer(),
                }
            }
        } else if pg_stmt.result.is_null() {
            ptr::null()
        } else if idx < 0 || idx >= pg_stmt.num_cols {
            ptr::null()
        } else {
            let row = pg_stmt.current_row;
            if row < 0 || row >= pg_stmt.num_rows {
                ptr::null()
            } else {
                match unsafe { load_live_text_state(pg_stmt, idx) } {
                    Some(state) => unsafe { write_live_text_output(pg_stmt, idx, &state) },
                    None => empty_text_buffer(),
                }
            }
        }
    }
    // Mutex released here.
}
