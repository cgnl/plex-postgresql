use super::*;

fn text_decltype() -> *const c_char {
    DECLTYPE_TEXT.as_ptr() as *const c_char
}

fn passthrough_decltype(p_stmt: *mut sqlite3_stmt, idx: c_int) -> *const c_char {
    get_orig_sqlite3_column_decltype()
        .map(|f| unsafe { f(p_stmt, idx) })
        .unwrap_or(ptr::null())
}

/// Check whether we have no result or idx is out of bounds.
/// SAFETY: Must be called while stmt mutex is held. Does NOT log to avoid
/// deadlock with the LOGGER mutex.
fn no_result_decltype(pg_stmt: &mut PgStmt, idx: c_int) -> Option<*const c_char> {
    if pg_stmt.result.is_null() || idx < 0 || idx >= pg_stmt.num_cols {
        return Some(text_decltype());
    }
    None
}

/// Look up cached decltype for a column.
/// SAFETY: Must be called while stmt mutex is held. Does NOT log to avoid
/// deadlock with the LOGGER mutex.
unsafe fn lookup_cached_decltype(
    pg_stmt: &mut PgStmt,
    idx: c_int,
    col_name: *const c_char,
) -> *const c_char {
    let mut cached_type = lookup_sqlite_decltype(pg_stmt.conn, col_name);

    if cached_type.is_null() && idx >= 0 && (idx as usize) < pg_stmt.col_table_names.len() {
        let table_ptr = pg_stmt.col_table_names[idx as usize];
        if !table_ptr.is_null() {
            let table = CStr::from_ptr(table_ptr).to_string_lossy();
            let column = cstr_to_string_or(col_name, "");
            let mut cache_key = String::with_capacity(DECLTYPE_MAX_KEY_LEN);
            cache_key.push_str(&table);
            cache_key.push('_');
            cache_key.push_str(&column);
            cached_type = lookup_decltype_direct(pg_stmt.conn, &cache_key);
        }
    }

    cached_type
}

/// Return a previously cached decltype value.
/// SAFETY: Must be called while stmt mutex is held. Does NOT log to avoid
/// deadlock with the LOGGER mutex.
unsafe fn return_cached_decltype(cached_type: *const c_char) -> *const c_char {
    cached_type
}

/// Resolve special-case decltype overrides.
/// SAFETY: Must be called while stmt mutex is held. Does NOT log to avoid
/// deadlock with the LOGGER mutex.
unsafe fn resolve_special_case_decltype(
    pg_stmt: &mut PgStmt,
    idx: c_int,
    oid: u32,
    col_name: *const c_char,
) -> Option<*const c_char> {
    let table_oid = crate::db_interpose_helpers::rust_pg_result_col_table_oid(
        helpers_result_ptr(pg_stmt.result),
        idx,
    );
    let special_case =
        crate::pg_statement::rust_decltype_special_case(oid, col_name, pg_stmt.pg_sql, table_oid);

    if special_case == PG_DECLTYPE_CASE_DT_INTEGER_8 {
        return Some(DECLTYPE_DT_INTEGER_8.as_ptr() as *const c_char);
    }
    if special_case == PG_DECLTYPE_CASE_NULL {
        return Some(ptr::null());
    }
    None
}

/// Map a PostgreSQL OID to a SQLite decltype string.
/// SAFETY: Must be called while stmt mutex is held. Does NOT log to avoid
/// deadlock with the LOGGER mutex.
unsafe fn oid_decltype(oid: u32) -> *const c_char {
    crate::pg_statement::oid_to_sqlite_decltype(oid).as_ptr()
}

pub(super) fn column_decltype_impl(p_stmt: *mut sqlite3_stmt, idx: c_int) -> *const c_char {
    let raw_pg_stmt = pg_find_any_stmt(p_stmt);

    if raw_pg_stmt.is_null() || unsafe { (&*raw_pg_stmt).is_pg == 0 } {
        return passthrough_decltype(p_stmt, idx);
    }

    let pg_stmt = unsafe { &mut *raw_pg_stmt };

    // Call ensure_metadata_result BEFORE acquiring stmt mutex to avoid
    // ABBA deadlock (stmt mutex -> conn mutex).
    if pg_stmt.result.is_null() && pg_stmt.cached_result.is_null() && !pg_stmt.pg_sql.is_null() {
        ensure_pg_result_for_metadata(raw_pg_stmt);
    }

    // Hold mutex only for data reads — no logging inside this block
    // to avoid ABBA deadlock between stmt mutex and LOGGER mutex.
    let _guard = unsafe { PgStmt::lock_mutex(raw_pg_stmt) };

    if let Some(result) = no_result_decltype(pg_stmt, idx) {
        return result;
    }

    let col_name = crate::db_interpose_helpers::rust_pg_result_col_name(
        helpers_result_ptr(pg_stmt.result),
        idx,
    );

    let cached_type = unsafe { lookup_cached_decltype(pg_stmt, idx, col_name) };
    if !cached_type.is_null() {
        return unsafe { return_cached_decltype(cached_type) };
    }

    let oid = crate::db_interpose_helpers::rust_pg_result_col_oid(
        helpers_result_ptr(pg_stmt.result),
        idx,
    );

    if let Some(result) = unsafe { resolve_special_case_decltype(pg_stmt, idx, oid, col_name) } {
        return result;
    }

    unsafe { oid_decltype(oid) }
}
