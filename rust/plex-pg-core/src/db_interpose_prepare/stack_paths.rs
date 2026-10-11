use super::*;
use crate::log_debug_lazy;
use crate::log_info_lazy;

unsafe fn prepare_shadow_stmt(
    db: *mut sqlite3,
    sql: *const c_char,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    let rc = if let Some(prepare) = shim_sqlite3_prepare_v2 {
        prepare(db, sql, -1, pp_stmt, pz_tail)
    } else {
        if !pp_stmt.is_null() {
            *pp_stmt = ptr::null_mut();
        }
        SQLITE_ERROR
    };

    if rc == SQLITE_OK && !pp_stmt.is_null() && !(*pp_stmt).is_null() {
        pg_note_stmt_prepare(*pp_stmt, sql);
    }

    rc
}

pub(super) unsafe fn maybe_delegate_prepare_to_worker(
    from_worker: c_int,
    stack_remaining: isize,
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
    depth_guard: &mut PrepareDepthGuard,
) -> Option<c_int> {
    let worker_active = std::ptr::read_volatile(std::ptr::addr_of!(worker_running)) != 0;
    if from_worker == 0
        && stack_remaining < WORKER_DELEGATION_THRESHOLD
        && worker_active
        && !should_bypass_worker_delegation(z_sql)
    {
        log_debug_lazy!(
            "WORKER DELEGATION: stack_remaining={} bytes < {}, delegating to 8MB worker",
            stack_remaining,
            WORKER_DELEGATION_THRESHOLD
        );
        depth_guard.decrement_now();
        return Some(delegate_prepare_to_worker(
            db, z_sql, n_byte, pp_stmt, pz_tail,
        ));
    }

    None
}

pub(super) unsafe fn maybe_handle_ondeck_low_stack(
    stack_remaining: isize,
    db: *mut sqlite3,
    z_sql: *const c_char,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> Option<c_int> {
    let is_ondeck_query = !z_sql.is_null()
        && ((contains_icase_ptr(z_sql, "metadata_item_settings")
            && contains_icase_ptr(z_sql, "metadata_items"))
            || (contains_icase_ptr(z_sql, "metadata_item_views")
                && contains_icase_ptr(z_sql, "grandparents"))
            || contains_icase_ptr(z_sql, "grandparentsSettings"));

    if !is_ondeck_query || stack_remaining >= 100_000 {
        return None;
    }

    log_info_lazy!(
        "STACK LOW OnDeck: {} bytes remaining - using PG fast path",
        stack_remaining
    );

    let pg_conn = crate::pg_client::rust_pg_find_connection(db);
    if !pg_conn.is_null() {
        let pc = &*pg_conn;
        if pc.is_pg_active != 0
            && !pc.conn.is_null()
            && crate::db_interpose_helpers::rust_is_library_db_path(pc.db_path.as_ptr()) != 0
        {
            let rc = prepare_shadow_stmt(db, c"SELECT 1".as_ptr(), pp_stmt, pz_tail);

            if rc == SQLITE_OK && !pp_stmt.is_null() && !(*pp_stmt).is_null() {
                let pg_stmt = pg_stmt_create(pg_conn, z_sql, *pp_stmt);
                if !pg_stmt.is_null() {
                    let s = &mut *pg_stmt;
                    s.is_pg = 2;
                    let mut trans = sql_translate(z_sql);
                    if trans.success != 0 && !trans.sql.is_null() {
                        let aliased =
                            crate::db_interpose_prepare_helpers::alias_collection_sync_aggregates(
                                &cstr_to_string_or(z_sql, ""),
                                &cstr_to_string_or(trans.sql, ""),
                            );
                        let pg_sql_src = match aliased {
                            Some(ref a) => CString::new(a.as_str()).ok(),
                            None => None,
                        };
                        let pg_sql_ptr = if let Some(cs) = pg_sql_src.as_ref() {
                            libc::strdup(cs.as_ptr())
                        } else {
                            libc::strdup(trans.sql)
                        };
                        s.pg_sql = pg_sql_ptr;
                        s.param_count = trans.param_count;
                        s.ensure_param_capacity(trans.param_count as usize);
                        trace_prepare_pgsql_if_enabled(z_sql, s.pg_sql);
                        log_info_lazy!(
                            "STACK LOW OnDeck: routed to PG: {}",
                            cstr_prefix(trans.sql, 100, "NULL")
                        );
                    }
                    sql_translation_free(&mut trans as *mut SqlTranslation);
                    if !describe_before_publication(pg_stmt, pp_stmt) {
                        return Some(SQLITE_ERROR);
                    }
                    pg_register_stmt(*pp_stmt, pg_stmt);
                }
            }
            return Some(rc);
        }
    }

    log_error("STACK CRITICAL OnDeck: no PG connection, returning empty");
    Some(prepare_shadow_stmt(
        db,
        c"SELECT 1 WHERE 0".as_ptr(),
        pp_stmt,
        pz_tail,
    ))
}

pub(super) unsafe fn maybe_handle_low_stack_prepare_path(
    from_worker: c_int,
    stack_remaining: isize,
    stack_used: isize,
    stack_size: usize,
    db: *mut sqlite3,
    z_sql: *const c_char,
    bytes: &[u8],
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> Option<c_int> {
    let stack_threshold = if from_worker != 0 { 32_000 } else { 64_000 };
    if stack_remaining >= stack_threshold {
        return None;
    }

    // SQLite passthrough (fts3_tokenizer, icu_load_collation, sqlite_master, etc.)
    // must go to real SQLite even on low stack — never route to PG, never reject.
    if !z_sql.is_null() {
        let sql_str = CStr::from_ptr(z_sql).to_str().unwrap_or("");
        if crate::pg_config::is_sqlite_passthrough_str(sql_str) {
            return None; // fall through to normal prepare_real_sqlite_stmt
        }
    }

    let pg_conn_check = crate::pg_client::rust_pg_find_connection(db);
    let is_pg_routable = if pg_conn_check.is_null() {
        false
    } else {
        let pcc = &*pg_conn_check;
        pcc.is_pg_active != 0
            && !pcc.conn.is_null()
            && !z_sql.is_null()
            && crate::db_interpose_helpers::rust_is_library_db_path(pcc.db_path.as_ptr()) != 0
            && (crate::pg_config::pg_config_is_read_operation(z_sql) != 0
                || crate::pg_config::pg_config_is_write_operation(z_sql) != 0)
    };

    if is_pg_routable {
        log_info_lazy!(
            "STACK LOW ({} bytes) but using PG path for: {}",
            stack_remaining,
            cstr_prefix(z_sql, 100, "NULL")
        );

        let rc = prepare_shadow_stmt(db, c"SELECT 1".as_ptr(), pp_stmt, pz_tail);

        if rc == SQLITE_OK && !pp_stmt.is_null() && !(*pp_stmt).is_null() {
            let pg_stmt = pg_stmt_create(pg_conn_check, z_sql, *pp_stmt);
            if !pg_stmt.is_null() {
                let s = &mut *pg_stmt;
                let is_write = crate::pg_config::pg_config_is_write_operation(z_sql) != 0;
                s.is_pg = if is_write { 1 } else { 2 };

                let mut trans = sql_translate(z_sql);
                if trans.success != 0 && !trans.sql.is_null() {
                    let aliased =
                        crate::db_interpose_prepare_helpers::alias_collection_sync_aggregates(
                            &cstr_to_string_or(z_sql, ""),
                            &cstr_to_string_or(trans.sql, ""),
                        );
                    let pg_sql_ptr = if let Some(a) = aliased {
                        let cs = CString::new(a).ok();
                        if let Some(cs) = cs.as_ref() {
                            libc::strdup(cs.as_ptr())
                        } else {
                            libc::strdup(trans.sql)
                        }
                    } else {
                        libc::strdup(trans.sql)
                    };

                    s.pg_sql = pg_sql_ptr;
                    s.param_count = trans.param_count;
                    s.ensure_param_capacity(trans.param_count as usize);
                    trace_prepare_pgsql_if_enabled(z_sql, s.pg_sql);

                    copy_param_names(pg_stmt, &trans);
                    apply_prepared_stmt_settings(pg_stmt);
                }
                sql_translation_free(&mut trans as *mut SqlTranslation);
                if !describe_before_publication(pg_stmt, pp_stmt) {
                    return Some(SQLITE_ERROR);
                }
                pg_register_stmt(*pp_stmt, pg_stmt);
            }
        }
        return Some(rc);
    }

    log_error(&format!(
        "STACK PROTECTION TRIGGERED: stack_used={}/{} bytes, remaining={} bytes",
        stack_used, stack_size, stack_remaining
    ));
    log_error(&format!(
        "  Query rejected (not a PG read): {}",
        cstr_prefix(z_sql, 200, "NULL")
    ));

    let pg_conn = crate::pg_client::rust_pg_find_connection(db);
    if !pg_conn.is_null() {
        let pc = &mut *pg_conn;
        pc.last_error_code = SQLITE_NOMEM;
        libc::snprintf(
            pc.last_error.as_mut_ptr(),
            pc.last_error.len(),
            c"Stack protection: insufficient stack space (remaining=%ld).".as_ptr(),
            stack_remaining as libc::c_long,
        );
    }

    if !pp_stmt.is_null() {
        *pp_stmt = ptr::null_mut();
    }
    if !pz_tail.is_null() {
        *pz_tail = ptr::null();
    }

    if starts_with_ascii_icase(bytes, b"SELECT") {
        log_debug_lazy!(
            "STACK PROTECTION select preview: {}",
            cstr_prefix(z_sql, 160, "NULL")
        );
    }

    Some(SQLITE_NOMEM)
}
