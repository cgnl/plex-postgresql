use super::*;
use crate::db_interpose_common::{
    CRASH_LAST_COLUMN, CRASH_LAST_COLUMN_LEN, CRASH_LAST_COLUMN_MAX_LEN, CRASH_LAST_COLUMN_SEQ,
};
use crate::log_debug_lazy;

struct CachedTypeState {
    row: c_int,
    col_name: *const c_char,
    oid: u32,
    value_ptr: *const c_char,
    is_null: bool,
}

struct LiveTypeState {
    row: c_int,
    col_name: *const c_char,
    oid: u32,
    sqlite_type: c_int,
    is_null: bool,
    value_buf: [c_char; 128],
    value_len: c_int,
}

impl LiveTypeState {
    fn decltype_guess(&self) -> &'static str {
        match self.oid {
            16 | 21 | 23 | 26 => "INTEGER",
            20 | 1114 | 1184 => "dt_integer(8)",
            700 | 701 | 1700 => "REAL",
            17 => "BLOB",
            _ => "TEXT",
        }
    }
}

unsafe fn bump_column_type_counters() {
    GLOBAL_COLUMN_TYPE_CALLS.fetch_add(1, Ordering::Relaxed);
    let tls_calls = tls_column_type_calls_ptr();
    *tls_calls = (*tls_calls).wrapping_add(1);
}

unsafe fn column_type_debug_sql(pg_stmt: *mut PgStmt) -> *const c_char {
    if pg_stmt.is_null() {
        return ptr::null();
    }
    let s = &*pg_stmt;
    if !s.pg_sql.is_null() {
        s.pg_sql
    } else {
        s.sql
    }
}

fn passthrough_column_type(p_stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    get_orig_sqlite3_column_type()
        .map(|f| unsafe { f(p_stmt, idx) })
        .unwrap_or(SQLITE_NULL)
}

fn sqlite_type_for_oid(oid: u32) -> c_int {
    pg_oid_to_sqlite_type_impl(oid)
}

unsafe fn load_cached_type_state(pg_stmt: &mut PgStmt, idx: c_int) -> Option<CachedTypeState> {
    let cached = &*pg_stmt.cached_result;
    let row = pg_stmt.current_row;
    if idx < 0 || idx >= cached.num_cols || row < 0 || row >= cached.num_rows {
        return None;
    }

    let crow = &*cached.rows.add(row as usize);
    let is_null = *crow.is_null.add(idx as usize) != 0;
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
    let value_ptr = if !crow.values.is_null() {
        *crow.values.add(idx as usize)
    } else {
        ptr::null()
    };

    Some(CachedTypeState {
        row,
        col_name,
        oid,
        value_ptr,
        is_null,
    })
}

/// Logging context returned from resolve functions so callers can log
/// AFTER releasing the mutex (avoids ABBA deadlock with LOGGER mutex).
struct ColumnTypeLogCtx {
    idx: c_int,
    row: c_int,
    oid: u32,
    result: c_int,
    col_name: *const c_char,
    pg_sql: *const c_char,
    trace_col: bool,
    phase: &'static str,
    is_null: bool,
    out_of_bounds: bool,
    decltype_guess: &'static str,
}

unsafe fn resolve_cached_column_type(
    pg_stmt: &mut PgStmt,
    _p_stmt: *mut sqlite3_stmt,
    idx: c_int,
) -> (c_int, ColumnTypeLogCtx) {
    let mut ctx = ColumnTypeLogCtx {
        idx,
        row: pg_stmt.current_row,
        oid: 0,
        result: SQLITE_NULL,
        col_name: ptr::null(),
        pg_sql: pg_stmt.pg_sql,
        trace_col: false,
        phase: "cached",
        is_null: false,
        out_of_bounds: false,
        decltype_guess: "",
    };

    let Some(state) = load_cached_type_state(pg_stmt, idx) else {
        ctx.out_of_bounds = true;
        return (SQLITE_NULL, ctx);
    };
    ctx.row = state.row;
    ctx.oid = state.oid;
    ctx.col_name = state.col_name;

    let raw_pg_stmt = pg_stmt as *mut PgStmt;
    let trace_col = trace_badcast_should_log_col(raw_pg_stmt, idx, state.col_name);
    ctx.trace_col = trace_col;

    if state.is_null {
        ctx.is_null = true;
        return (SQLITE_NULL, ctx);
    }

    if !state.value_ptr.is_null() {
        let raw_val = pg_text_to_int64_impl(state.value_ptr);
        let mut masked = 0i64;
        if mask_collection_metadata_type(pg_stmt, state.col_name, raw_val, &mut masked) {
            let result = sqlite_type_for_oid(state.oid);
            ctx.result = result;
            return (result, ctx);
        }
    }

    let result = sqlite_type_for_oid(state.oid);
    ctx.result = result;
    (result, ctx)
}

unsafe fn load_live_type_state(pg_stmt: &mut PgStmt, idx: c_int) -> Option<LiveTypeState> {
    // NOTE: all logging removed from this function because it is called
    // while pg_stmt.mutex is held; logging is done by the caller after
    // releasing the mutex.
    if pg_stmt.result.is_null() {
        return None;
    }
    if idx < 0 || idx >= pg_stmt.num_cols {
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
    let col_name = crate::db_interpose_helpers::rust_pg_result_col_name(
        helpers_result_ptr(pg_stmt.result),
        idx,
    );

    let mut state = LiveTypeState {
        row,
        col_name,
        oid: oid_u as u32,
        sqlite_type,
        is_null: is_null != 0,
        value_buf: [0; 128],
        value_len: -1,
    };

    if !state.is_null {
        state.value_len = crate::db_interpose_helpers::rust_pg_result_text_copy(
            helpers_result_ptr(pg_stmt.result),
            row,
            idx,
            state.value_buf.as_mut_ptr(),
            state.value_buf.len(),
        );
    }

    Some(state)
}

unsafe fn resolve_live_column_type(
    pg_stmt: &mut PgStmt,
    _p_stmt: *mut sqlite3_stmt,
    idx: c_int,
) -> (c_int, ColumnTypeLogCtx) {
    let mut ctx = ColumnTypeLogCtx {
        idx,
        row: pg_stmt.current_row,
        oid: 0,
        result: SQLITE_NULL,
        col_name: ptr::null(),
        pg_sql: pg_stmt.pg_sql,
        trace_col: false,
        phase: "live",
        is_null: false,
        out_of_bounds: false,
        decltype_guess: "",
    };

    if pg_stmt.metadata_only_result != 0 && !pg_stmt.result.is_null() {
        if idx < 0 || idx >= pg_stmt.num_cols {
            ctx.out_of_bounds = true;
            return (SQLITE_NULL, ctx);
        }

        let oid = crate::db_interpose_helpers::rust_pg_result_col_oid(
            helpers_result_ptr(pg_stmt.result),
            idx,
        );
        let col_name = crate::db_interpose_helpers::rust_pg_result_col_name(
            helpers_result_ptr(pg_stmt.result),
            idx,
        );
        ctx.phase = "metadata";
        ctx.oid = oid;
        ctx.col_name = col_name;
        ctx.trace_col = trace_badcast_should_log_col(pg_stmt as *mut PgStmt, idx, col_name);
        ctx.result = sqlite_type_for_oid(oid);
        ctx.decltype_guess = match ctx.result {
            SQLITE_INTEGER => "INTEGER",
            SQLITE_FLOAT => "REAL",
            SQLITE_BLOB => "BLOB",
            _ => "TEXT",
        };
        return (ctx.result, ctx);
    }

    let Some(state) = load_live_type_state(pg_stmt, idx) else {
        ctx.out_of_bounds = true;
        return (SQLITE_NULL, ctx);
    };
    ctx.row = state.row;
    ctx.oid = state.oid;
    ctx.col_name = state.col_name;

    let raw_pg_stmt = pg_stmt as *mut PgStmt;
    let trace_col = trace_badcast_should_log_col(raw_pg_stmt, idx, state.col_name);
    ctx.trace_col = trace_col;
    // --- seqlock: begin CRASH_LAST_COLUMN write ---
    {
        let c_seq = CRASH_LAST_COLUMN_SEQ.load(Ordering::Relaxed);
        CRASH_LAST_COLUMN_SEQ.store(c_seq.wrapping_add(1), Ordering::Release);
        let clen = if !state.col_name.is_null() && *state.col_name != 0 {
            let mut wrote = libc::snprintf(
                ptr::addr_of_mut!(CRASH_LAST_COLUMN) as *mut c_char,
                CRASH_LAST_COLUMN_MAX_LEN,
                b"%.63s\0".as_ptr() as *const c_char,
                state.col_name,
            );
            if wrote < 0 {
                wrote = 0;
            }
            if wrote >= CRASH_LAST_COLUMN_MAX_LEN as c_int {
                wrote = CRASH_LAST_COLUMN_MAX_LEN as c_int - 1;
            }
            wrote
        } else {
            CRASH_LAST_COLUMN[0] = 0;
            0
        };
        CRASH_LAST_COLUMN_LEN.store(clen, Ordering::SeqCst);
        CRASH_LAST_COLUMN_SEQ.store(c_seq.wrapping_add(2), Ordering::Release);
    }
    // --- seqlock: end CRASH_LAST_COLUMN write ---

    if state.is_null {
        ctx.is_null = true;
        return (SQLITE_NULL, ctx);
    }

    if state.value_len >= 0 {
        let raw_val = pg_text_to_int64_impl(state.value_buf.as_ptr());
        let mut masked = 0i64;
        if mask_collection_metadata_type(pg_stmt, state.col_name, raw_val, &mut masked) {
            // Return the column's actual type (not SQLITE_NULL) to prevent
            // holder/type mismatch → bad_cast. The mask sets the value to 0,
            // which column_int will return. SOCI's typed holder stays valid.
            let result = state.sqlite_type;
            ctx.result = result;
            return (result, ctx);
        }
    }

    let result = state.sqlite_type;
    ctx.result = result;
    ctx.decltype_guess = state.decltype_guess();
    (result, ctx)
}

pub(super) fn column_type_impl(p_stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    unsafe { bump_column_type_counters() };

    log_debug_lazy!("COLUMN_TYPE: stmt={:p} idx={}", p_stmt, idx);
    let raw_pg_stmt = pg_find_any_stmt(p_stmt);
    let dbg_sql = unsafe { column_type_debug_sql(raw_pg_stmt) };
    let dbg_db = get_orig_sqlite3_db_handle()
        .map(|f| unsafe { f(p_stmt) })
        .unwrap_or(ptr::null_mut());
    unsafe {
        pg_exception_note_phase(
            b"column_type\0".as_ptr() as *const c_char,
            dbg_sql,
            p_stmt,
            dbg_db,
        );
    }

    if !raw_pg_stmt.is_null() && unsafe { (&*raw_pg_stmt).is_pg != 0 } {
        let pg_stmt = unsafe { &mut *raw_pg_stmt };
        let needs_metadata = pg_stmt.result.is_null()
            && pg_stmt.cached_result.is_null()
            && !pg_stmt.pg_sql.is_null();
        if needs_metadata {
            ensure_pg_result_for_metadata(raw_pg_stmt);
        }
        unsafe {
            let tls_query = tls_last_query_ptr();
            *tls_query = pg_stmt.pg_sql;
        }

        let (result, ctx) = {
            let _guard = unsafe { PgStmt::lock_mutex(raw_pg_stmt) };
            if !pg_stmt.cached_result.is_null() {
                unsafe { resolve_cached_column_type(pg_stmt, p_stmt, idx) }
            } else {
                unsafe { resolve_live_column_type(pg_stmt, p_stmt, idx) }
            }
        };
        column_type_emit_log(raw_pg_stmt, p_stmt, &ctx);
        result
    } else {
        passthrough_column_type(p_stmt, idx)
    }
}

/// Emit all diagnostic / trace logging for a column_type call.
/// Must be called OUTSIDE any pg_stmt mutex scope.
fn column_type_emit_log(pg_stmt: *mut PgStmt, p_stmt: *mut sqlite3_stmt, ctx: &ColumnTypeLogCtx) {
    if ctx.out_of_bounds {
        log_debug_lazy!(
            "COLUMN_TYPE_VERBOSE: idx={} row={} -> SQLITE_NULL ({}, out of bounds)",
            ctx.idx,
            ctx.row,
            ctx.phase
        );
        return;
    }
    if ctx.is_null {
        log_debug_lazy!(
            "COLUMN_TYPE: idx={} col='{}' is NULL, returning {} ({})",
            ctx.idx,
            cstr_to_string_or(ctx.col_name, "?"),
            sqlite_type_name(ctx.result),
            ctx.phase
        );
        if ctx.trace_col {
            trace_badcast_log_ctx(
                pg_stmt,
                p_stmt,
                ctx.idx,
                "column_type",
                ctx.phase,
                ctx.row,
                1,
                ctx.oid,
                ctx.col_name,
            );
            log_debug_lazy!(
                "TRACE_BADCAST: column_type idx={} col='{}' row={} oid={} is_null=1 -> {} sql={}",
                ctx.idx,
                cstr_to_string_or(ctx.col_name, "?"),
                ctx.row,
                ctx.oid,
                sqlite_type_name(ctx.result),
                cstr_prefix(ctx.pg_sql, 200, "?")
            );
        }
        return;
    }
    if ctx.trace_col {
        trace_badcast_log_ctx(
            pg_stmt,
            p_stmt,
            ctx.idx,
            "column_type",
            ctx.phase,
            ctx.row,
            0,
            ctx.oid,
            ctx.col_name,
        );
        log_debug_lazy!(
            "TRACE_BADCAST: column_type ({}) idx={} col='{}' row={} oid={} is_null=0 -> {} (guess_decltype='{}') sql={}",
            ctx.phase,
            ctx.idx,
            cstr_to_string_or(ctx.col_name, "?"),
            ctx.row,
            ctx.oid,
            sqlite_type_name(ctx.result),
            ctx.decltype_guess,
            cstr_prefix(ctx.pg_sql, 200, "?")
        );
    }
    log_debug_lazy!(
        "COLUMN_TYPE: idx={} col='{}' row={} OID={} -> {} (decltype='{}', {})",
        ctx.idx,
        cstr_to_string_or(ctx.col_name, "?"),
        ctx.row,
        ctx.oid,
        sqlite_type_name(ctx.result),
        ctx.decltype_guess,
        ctx.phase
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_oid_mapping_keeps_timestamp_columns_integer() {
        assert_eq!(sqlite_type_for_oid(1114), SQLITE_INTEGER);
        assert_eq!(sqlite_type_for_oid(1184), SQLITE_INTEGER);
    }
}
