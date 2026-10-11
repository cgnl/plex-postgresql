use super::*;

pub(super) fn column_count_impl(p_stmt: *mut sqlite3_stmt) -> c_int {
    let raw_pg_stmt = pg_find_any_stmt(p_stmt);
    if !raw_pg_stmt.is_null() && unsafe { (&*raw_pg_stmt).is_pg != 0 } {
        let _guard = unsafe { PgStmt::lock_mutex(raw_pg_stmt) };
        let pg_stmt = unsafe { &*raw_pg_stmt };
        if !pg_stmt.descriptor.is_null() {
            return crate::libpq_helpers::rust_pq_nfields(pg_stmt.descriptor);
        }
        if !pg_stmt.cached_result.is_null() {
            return unsafe { (*pg_stmt.cached_result).num_cols };
        }
        return pg_stmt.num_cols;
    }
    get_orig_sqlite3_column_count()
        .map(|f| unsafe { f(p_stmt) })
        .unwrap_or(0)
}

pub(super) fn column_name_impl(p_stmt: *mut sqlite3_stmt, idx: c_int) -> *const c_char {
    let raw_pg_stmt = pg_find_any_stmt(p_stmt);
    let mut result: *const c_char = ptr::null();
    let mut use_orig = true;

    if !raw_pg_stmt.is_null() && unsafe { (&*raw_pg_stmt).is_pg != 0 } {
        let _guard = unsafe { PgStmt::lock_mutex(raw_pg_stmt) };
        let pg_stmt = unsafe { &*raw_pg_stmt };
        if !pg_stmt.descriptor.is_null() {
            if idx >= 0 && idx < crate::libpq_helpers::rust_pq_nfields(pg_stmt.descriptor) {
                return crate::db_interpose_helpers::rust_pg_result_col_name(
                    helpers_result_ptr(pg_stmt.descriptor),
                    idx,
                );
            }
            return ptr::null();
        }
        if !pg_stmt.col_names.is_null() && idx >= 0 && idx < pg_stmt.num_col_names {
            result = unsafe { *pg_stmt.col_names.add(idx as usize) };
            use_orig = false;
        } else if !pg_stmt.result.is_null() && idx >= 0 && idx < pg_stmt.num_cols {
            result = crate::db_interpose_helpers::rust_pg_result_col_name(
                helpers_result_ptr(pg_stmt.result),
                idx,
            );
            use_orig = false;
        } else {
            use_orig = false;
        }
    }

    if use_orig {
        result = get_orig_sqlite3_column_name()
            .map(|f| unsafe { f(p_stmt, idx) })
            .unwrap_or(ptr::null());
    }
    result
}

pub(super) fn data_count_impl(p_stmt: *mut sqlite3_stmt) -> c_int {
    let raw_pg_stmt = pg_find_any_stmt(p_stmt);

    if !raw_pg_stmt.is_null() && unsafe { (&*raw_pg_stmt).is_pg != 0 } {
        let pg_stmt = unsafe { &mut *raw_pg_stmt };
        // Hold mutex only for data reads — no logging inside this block
        // to avoid ABBA deadlock between stmt mutex and LOGGER mutex.
        let _guard = unsafe { PgStmt::lock_mutex(raw_pg_stmt) };
        let count = if pg_stmt.current_row >= 0 && pg_stmt.current_row < pg_stmt.num_rows {
            pg_stmt.num_cols
        } else {
            0
        };
        return count;
    }

    get_orig_sqlite3_data_count()
        .map(|f| unsafe { f(p_stmt) })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C)]
    struct ResultAttribute {
        name: *mut c_char,
        table_id: u32,
        column_id: c_int,
        format: c_int,
        type_id: u32,
        type_len: c_int,
        type_modifier: c_int,
    }

    extern "C" {
        fn PQmakeEmptyPGresult(
            conn: *mut crate::libpq_helpers::PGconn,
            status: c_int,
        ) -> *mut PgResultLibpq;
        fn PQsetResultAttrs(
            result: *mut PgResultLibpq,
            count: c_int,
            attrs: *mut ResultAttribute,
        ) -> c_int;
    }

    #[test]
    fn prepared_descriptors_survive_reset_and_empty_results_without_backend() {
        let mut names = [
            CString::new("metadata_type").unwrap(),
            CString::new("count").unwrap(),
        ];
        let mut attrs: Vec<_> = names
            .iter_mut()
            .enumerate()
            .map(|(i, name)| ResultAttribute {
                name: name.as_ptr().cast_mut(),
                table_id: 0,
                column_id: 0,
                format: 0,
                type_id: if i == 0 { 23 } else { 20 },
                type_len: if i == 0 { 4 } else { 8 },
                type_modifier: -1,
            })
            .collect();
        let desc = unsafe { PQmakeEmptyPGresult(ptr::null_mut(), PGRES_COMMAND_OK) };
        assert!(!desc.is_null());
        assert_eq!(unsafe { PQsetResultAttrs(desc, 2, attrs.as_mut_ptr()) }, 1);
        let stmt = Box::into_raw(Box::new(PgStmt::new()));
        let shadow = stmt.cast::<sqlite3_stmt>();
        unsafe {
            (*stmt).is_pg = 2;
            (*stmt).descriptor = desc;
            (*stmt).num_cols = 2;
            (*stmt).current_row = -1;
        }
        crate::pg_statement::rust_stmt_register(shadow as usize, stmt as usize);
        let name = column_name_impl(shadow, 1);
        assert_eq!(column_count_impl(shadow), 2);
        assert_eq!(data_count_impl(shadow), 0);
        assert_eq!(unsafe { CStr::from_ptr(name) }.to_bytes(), b"count");
        assert!(column_name_impl(shadow, 2).is_null());
        for _ in 0..2 {
            crate::pg_statement::rust_stmt_clear_result(stmt);
            assert_eq!(column_count_impl(shadow), 2);
            assert_eq!(column_name_impl(shadow, 1), name);
            assert_eq!(data_count_impl(shadow), 0);
            // No live connection is needed, including after PostgreSQL loss.
            assert!(ensure_pg_result_for_metadata(stmt));
        }
        crate::pg_statement::rust_stmt_unregister(shadow as usize);
        crate::pg_statement::rust_stmt_free(stmt);
    }
}
