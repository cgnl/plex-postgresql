use super::*;

struct CachedScalarState {
    _row: c_int,
    col_name: *const c_char,
    value_ptr: *const c_char,
}

struct LiveScalarState {
    _row: c_int,
    _oid: u32,
    col_name: *const c_char,
    is_null: bool,
    value_buf: [c_char; 128],
    value_valid: bool,
}

impl LiveScalarState {
    fn new(row: c_int, oid: u32, col_name: *const c_char, is_null: bool) -> Self {
        Self {
            _row: row,
            _oid: oid,
            col_name,
            is_null,
            value_buf: [0; 128],
            value_valid: false,
        }
    }
}

unsafe fn load_cached_scalar_state(pg_stmt: &mut PgStmt, idx: c_int) -> Option<CachedScalarState> {
    let cached = &*pg_stmt.cached_result;
    let row = pg_stmt.current_row;
    if idx < 0 || idx >= cached.num_cols || row < 0 || row >= cached.num_rows {
        return None;
    }
    let crow = &*cached.rows.add(row as usize);
    if *crow.is_null.add(idx as usize) != 0 {
        return None;
    }
    let value_ptr = *crow.values.add(idx as usize);
    if value_ptr.is_null() {
        return None;
    }
    let col_name = if !cached.col_names.is_null() {
        *cached.col_names.add(idx as usize)
    } else {
        ptr::null()
    };

    Some(CachedScalarState {
        _row: row,
        col_name,
        value_ptr,
    })
}

unsafe fn load_live_scalar_state(
    pg_stmt: &mut PgStmt,
    idx: c_int,
    _bounds_label: &str,
    _row_bounds_label: &str,
) -> Option<LiveScalarState> {
    if pg_stmt.result.is_null() {
        return None;
    }
    if idx < 0 || idx >= pg_stmt.num_cols {
        // NOTE: no log_debug here -- this runs under pg_stmt.mutex and
        // log_debug acquires the LOGGER mutex, creating an ABBA deadlock.
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
    let mut state = LiveScalarState::new(row, oid_u as u32, col_name, is_null != 0);

    if !state.is_null {
        let val_len = crate::db_interpose_helpers::rust_pg_result_text_copy(
            helpers_result_ptr(pg_stmt.result),
            row,
            idx,
            state.value_buf.as_mut_ptr(),
            state.value_buf.len(),
        );
        if val_len >= 0 {
            state.value_valid = true;
        }
    }

    Some(state)
}

pub(super) fn column_int_impl(p_stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    validate_type_consistency(p_stmt, idx, "column_int");
    let raw_pg_stmt = pg_find_any_stmt(p_stmt);

    if !raw_pg_stmt.is_null() && unsafe { (&*raw_pg_stmt).is_pg != 0 } {
        let pg_stmt = unsafe { &mut *raw_pg_stmt };
        let result_val;
        {
            let _guard = unsafe { PgStmt::lock_mutex(raw_pg_stmt) };

            if !pg_stmt.cached_result.is_null() {
                return unsafe {
                    if let Some(state) = load_cached_scalar_state(pg_stmt, idx) {
                        pg_text_to_int_impl(state.value_ptr)
                    } else {
                        0
                    }
                };
            }

            result_val = unsafe {
                let Some(state) =
                    load_live_scalar_state(pg_stmt, idx, "COL_INT_BOUNDS", "COL_INT_ROW_BOUNDS")
                else {
                    return 0;
                };

                let mut rv = 0;
                if !state.is_null && state.value_valid {
                    rv = pg_text_to_int_impl(state.value_buf.as_ptr());

                    let mut masked = 0i64;
                    if mask_collection_metadata_type(
                        pg_stmt,
                        state.col_name,
                        rv as i64,
                        &mut masked,
                    ) {
                        rv = masked as c_int;
                    }
                }
                rv
            };
        }
        // Guard is dropped -- safe to return without holding the mutex.
        return result_val;
    }

    get_orig_sqlite3_column_int()
        .map(|f| unsafe { f(p_stmt, idx) })
        .unwrap_or(0)
}

pub(super) fn column_int64_impl(p_stmt: *mut sqlite3_stmt, idx: c_int) -> i64 {
    validate_type_consistency(p_stmt, idx, "column_int64");
    let raw_pg_stmt = pg_find_any_stmt(p_stmt);

    if !raw_pg_stmt.is_null() && unsafe { (&*raw_pg_stmt).is_pg != 0 } {
        let pg_stmt = unsafe { &mut *raw_pg_stmt };
        let result_val;
        {
            let _guard = unsafe { PgStmt::lock_mutex(raw_pg_stmt) };

            if !pg_stmt.cached_result.is_null() {
                return unsafe {
                    if let Some(state) = load_cached_scalar_state(pg_stmt, idx) {
                        let mut rv = pg_text_to_int64_impl(state.value_ptr);
                        let mut masked = 0i64;
                        if mask_collection_metadata_type(pg_stmt, state.col_name, rv, &mut masked) {
                            rv = masked;
                        }
                        rv
                    } else {
                        0
                    }
                };
            }

            result_val = unsafe {
                let Some(state) = load_live_scalar_state(
                    pg_stmt,
                    idx,
                    "COL_INT64_BOUNDS",
                    "COL_INT64_ROW_BOUNDS",
                ) else {
                    return 0;
                };

                let mut rv: i64 = 0;
                if !state.is_null && state.value_valid {
                    rv = pg_text_to_int64_impl(state.value_buf.as_ptr());

                    let mut masked = 0i64;
                    if mask_collection_metadata_type(pg_stmt, state.col_name, rv, &mut masked) {
                        rv = masked;
                    }
                }
                rv
            };
        }
        // Guard is dropped -- safe to return without holding the mutex.
        return result_val;
    }

    get_orig_sqlite3_column_int64()
        .map(|f| unsafe { f(p_stmt, idx) })
        .unwrap_or(0)
}

pub(super) fn column_double_impl(p_stmt: *mut sqlite3_stmt, idx: c_int) -> f64 {
    validate_type_consistency(p_stmt, idx, "column_double");
    let raw_pg_stmt = pg_find_any_stmt(p_stmt);

    if !raw_pg_stmt.is_null() && unsafe { (&*raw_pg_stmt).is_pg != 0 } {
        let pg_stmt = unsafe { &mut *raw_pg_stmt };
        let result_val;
        {
            let _guard = unsafe { PgStmt::lock_mutex(raw_pg_stmt) };

            if !pg_stmt.cached_result.is_null() {
                return unsafe {
                    if let Some(state) = load_cached_scalar_state(pg_stmt, idx) {
                        pg_text_to_double_impl(state.value_ptr)
                    } else {
                        0.0
                    }
                };
            }

            result_val = unsafe {
                let Some(state) = load_live_scalar_state(
                    pg_stmt,
                    idx,
                    "COL_DOUBLE_BOUNDS",
                    "COL_DOUBLE_ROW_BOUNDS",
                ) else {
                    return 0.0;
                };

                if !state.is_null && state.value_valid {
                    pg_text_to_double_impl(state.value_buf.as_ptr())
                } else {
                    0.0
                }
            };
        }
        // Guard is dropped -- safe to return without holding the mutex.
        return result_val;
    }

    get_orig_sqlite3_column_double()
        .map(|f| unsafe { f(p_stmt, idx) })
        .unwrap_or(0.0)
}
