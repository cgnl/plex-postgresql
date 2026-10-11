use super::*;
use crate::log_debug_lazy;
use crate::log_info_lazy;

struct StackState {
    size: usize,
    used: isize,
    remaining: isize,
}

unsafe fn note_prepare_phase(db: *mut sqlite3, z_sql: *const c_char) {
    pg_exception_note_phase(
        b"prepare_v2\0".as_ptr() as *const c_char,
        z_sql,
        ptr::null_mut::<sqlite3_stmt>(),
        db,
    );
    if !z_sql.is_null() {
        pg_exception_note_query(z_sql);
    }
}

unsafe fn maybe_short_circuit_query_loop(
    db: *mut sqlite3,
    z_sql: *const c_char,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> Option<c_int> {
    let _ = db;
    if !detect_query_loop(z_sql) {
        return None;
    }

    if let Some(prepare) = shim_sqlite3_prepare_v2 {
        let rc = prepare(
            db,
            b"SELECT 1 WHERE 0\0".as_ptr() as *const c_char,
            -1,
            pp_stmt,
            pz_tail,
        );
        if rc == SQLITE_OK && !pp_stmt.is_null() && !(*pp_stmt).is_null() {
            pg_note_stmt_prepare(*pp_stmt, b"SELECT 1 WHERE 0\0".as_ptr() as *const c_char);
        }
        return Some(rc);
    }

    if !pp_stmt.is_null() {
        *pp_stmt = ptr::null_mut();
    }
    if !pz_tail.is_null() {
        *pz_tail = ptr::null();
    }
    Some(SQLITE_OK)
}

fn current_stack_state() -> StackState {
    let self_thread = unsafe { libc::pthread_self() };
    let local_var: u8 = 0;
    let current_stack = (&local_var as *const u8) as isize;

    #[cfg(target_os = "macos")]
    {
        let stack_addr = unsafe { libc::pthread_get_stackaddr_np(self_thread) as *mut c_void };
        let stack_size = unsafe { libc::pthread_get_stacksize_np(self_thread) };
        let stack_used = (stack_addr as isize).wrapping_sub(current_stack).abs();
        StackState {
            size: stack_size,
            used: stack_used,
            remaining: stack_size as isize - stack_used,
        }
    }

    #[cfg(not(target_os = "macos"))]
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        let mut stack_size: usize = 0;
        let mut stack_bottom: *mut c_void = ptr::null_mut();
        if libc::pthread_getattr_np(self_thread, &mut attr) == 0 {
            libc::pthread_attr_getstack(&attr, &mut stack_bottom, &mut stack_size);
            libc::pthread_attr_destroy(&mut attr);
        }

        let stack_addr = if stack_bottom.is_null() {
            ptr::null_mut()
        } else {
            (stack_bottom as *mut u8).add(stack_size) as *mut c_void
        };
        let mut stack_used = (stack_addr as isize).wrapping_sub(current_stack).abs();

        if !stack_bottom.is_null() && !stack_addr.is_null() {
            let cur = current_stack as usize;
            let bottom = stack_bottom as usize;
            let top = stack_addr as usize;
            if cur < bottom || cur > top {
                log_error(&format!(
                    "STACK CALCULATION ERROR: current={:p} not in [{:p}, {:p}]",
                    current_stack as *const c_void, stack_bottom, stack_addr
                ));
                stack_size = 8 * 1024 * 1024;
                stack_used = 0;
            }
        }

        StackState {
            size: stack_size,
            used: stack_used,
            remaining: stack_size as isize - stack_used,
        }
    }
}

fn maybe_log_query_shape(z_sql: *const c_char, bytes: &[u8], skip_complex_processing: c_int) {
    if bytes.contains(&b'`') {
        log_debug_lazy!(
            "BACKTICK_QUERY: skip_complex={} len={} sql={}",
            skip_complex_processing,
            bytes.len(),
            cstr_prefix(z_sql, 200, "NULL")
        );
    }

    if skip_complex_processing == 0
        && starts_with_ascii_icase(bytes, b"INSERT")
        && contains_ascii_icase(bytes, b"metadata_items")
    {
        log_info_lazy!(
            "PREPARE_V2 INSERT metadata_items: {}",
            cstr_prefix(z_sql, 300, "NULL")
        );
        if contains_ascii_icase(bytes, b"icu_root") {
            log_info("PREPARE_V2 has icu_root - will clean!");
        }
    }
}

fn maybe_log_txn_route(
    z_sql: *const c_char,
    pg_conn: *mut PgConnection,
    is_read: bool,
    is_write: bool,
) {
    if !is_txn_control_sql(z_sql) {
        return;
    }

    let total = TXN_ROUTE_TOTAL.fetch_add(1, Ordering::Relaxed) + 1;
    let skip_now = crate::pg_config::pg_config_should_skip_sql(z_sql) != 0;
    if skip_now {
        TXN_ROUTE_SKIPPED.fetch_add(1, Ordering::Relaxed);
    }
    if !pg_conn.is_null() {
        let c = unsafe { &*pg_conn };
        if c.is_pg_active != 0
            && crate::db_interpose_helpers::rust_is_library_db_path(c.db_path.as_ptr()) != 0
            && (is_read || is_write)
            && !skip_now
        {
            TXN_ROUTE_PG.fetch_add(1, Ordering::Relaxed);
        }
    }

    log_info_lazy!(
        "TXN_ROUTE prepare: skip={} is_write={} is_read={} sql={}",
        skip_now as i32,
        is_write as i32,
        is_read as i32,
        cstr_prefix(z_sql, 220, "NULL")
    );

    if total == 1 || total.is_multiple_of(50) {
        let skipped = TXN_ROUTE_SKIPPED.load(Ordering::Relaxed);
        let routed_pg = TXN_ROUTE_PG.load(Ordering::Relaxed);
        log_info_lazy!(
            "TXN_ROUTE stats: total={} skipped={} pg_routed={}",
            total,
            skipped,
            routed_pg
        );
    }
}

unsafe fn clear_connection_error_state(db: *mut sqlite3) {
    let pg_conn_for_clear = crate::pg_client::rust_pg_find_connection(db);
    if !pg_conn_for_clear.is_null() {
        let c = &mut *pg_conn_for_clear;
        c.last_error_code = SQLITE_OK;
        c.last_error[0] = 0;
    }
}

pub(super) fn prepare_v2_internal_impl(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
    from_worker: c_int,
) -> c_int {
    unsafe {
        note_prepare_phase(db, z_sql);
    }

    if trace_prepare_sql_ok(z_sql) {
        log_debug_lazy!("TRACE_PREPARE_SQL: {}", cstr_prefix(z_sql, 700, "NULL"));
    }

    // Only check shadow SQLite schema for non-PG databases.
    // For PG-routed DDL, PG is authoritative — skip shadow checks entirely.
    // This prevents crashes when shadow SQLite lacks tables that PG has.
    unsafe {
        let pg_conn_check = crate::pg_client::rust_pg_find_connection(db);
        let is_pg_active = !pg_conn_check.is_null() && (&*pg_conn_check).is_pg_active != 0;
        if !is_pg_active {
            if let Some(rc) = maybe_skip_alter_table_add(db, z_sql, pp_stmt, pz_tail) {
                return rc;
            }
        }
    }

    if let Some(rc) = unsafe { maybe_short_circuit_query_loop(db, z_sql, pp_stmt, pz_tail) } {
        return rc;
    }

    let mut depth_guard = unsafe { PrepareDepthGuard::enter() };
    unsafe {
        let depth = *tls_prepare_v2_depth_ptr();
        if depth > 50 {
            log_error(&format!(
                "RECURSION LIMIT: prepare_v2 called {} times (depth={})!",
                depth, depth
            ));
            log_error("  This indicates infinite recursion - ABORTING to prevent crash");
            log_error(&format!("  Query: {}", cstr_prefix(z_sql, 200, "NULL")));
            if !pp_stmt.is_null() {
                *pp_stmt = ptr::null_mut();
            }
            if !pz_tail.is_null() {
                *pz_tail = ptr::null();
            }
            return SQLITE_ERROR;
        }
    }

    let stack = current_stack_state();
    let bytes = if z_sql.is_null() {
        &[][..]
    } else {
        unsafe { CStr::from_ptr(z_sql).to_bytes() }
    };
    unsafe { log_stack_info(stack.size as isize, stack.used, stack.remaining) };

    if let Some(rc) = unsafe {
        maybe_delegate_prepare_to_worker(
            from_worker,
            stack.remaining,
            db,
            z_sql,
            n_byte,
            pp_stmt,
            pz_tail,
            &mut depth_guard,
        )
    } {
        return rc;
    }

    if let Some(rc) =
        unsafe { maybe_handle_ondeck_low_stack(stack.remaining, db, z_sql, pp_stmt, pz_tail) }
    {
        return rc;
    }

    if let Some(rc) = unsafe {
        maybe_handle_low_stack_prepare_path(
            from_worker,
            stack.remaining,
            stack.used,
            stack.size,
            db,
            z_sql,
            bytes,
            pp_stmt,
            pz_tail,
        )
    } {
        return rc;
    }

    let mut skip_complex_processing = 0;
    if from_worker == 0 && stack.remaining < 64_000 {
        skip_complex_processing = 1;
        log_info_lazy!(
            "STACK CAUTION: stack_used={}/{} bytes, remaining={} - skipping complex processing",
            stack.used,
            stack.size,
            stack.remaining
        );
    }

    if z_sql.is_null() {
        log_error("prepare_v2 called with NULL SQL");
        return if let Some(prepare) = unsafe { shim_sqlite3_prepare_v2 } {
            unsafe { prepare(db, z_sql, n_byte, pp_stmt, pz_tail) }
        } else {
            if !pp_stmt.is_null() {
                unsafe { *pp_stmt = ptr::null_mut() };
            }
            SQLITE_ERROR
        };
    }

    maybe_log_query_shape(z_sql, bytes, skip_complex_processing);

    let pg_conn = if skip_complex_processing != 0 {
        ptr::null_mut()
    } else {
        crate::pg_client::rust_pg_find_connection(db)
    };

    let is_write = crate::pg_config::pg_config_is_write_operation(z_sql) != 0;
    let is_read = crate::pg_config::pg_config_is_read_operation(z_sql) != 0;

    maybe_log_txn_route(z_sql, pg_conn, is_read, is_write);

    if contains_icase_ptr(z_sql, "plugins") {
        log_info_lazy!(
            "SKIP_DEBUG plugins query skip={} sql={}",
            crate::pg_config::pg_config_should_skip_sql(z_sql) != 0,
            cstr_prefix(z_sql, 220, "NULL")
        );
    }

    let sql_str = unsafe { CStr::from_ptr(z_sql) }.to_str().unwrap_or("");

    // SQLite passthrough (fts3_tokenizer, icu_load_collation, sqlite_master, etc.)
    // must use real SQLite prepare. They configure SQLite's engine or query metadata.
    let is_passthrough = crate::pg_config::is_sqlite_passthrough_str(sql_str);

    // DDL must never hit shadow SQLite — always use dummy shadow + PG.
    // Defense-in-depth: if pg_conn is null (low-stack skip_complex_processing),
    // look up the connection directly so DDL still routes to PG.
    let is_ddl = crate::pg_config::is_ddl_str(sql_str);
    let effective_pg_conn = if is_ddl && pg_conn.is_null() {
        crate::pg_client::rust_pg_find_connection(db)
    } else {
        pg_conn
    };
    let is_ddl_on_pg = is_ddl
        && !effective_pg_conn.is_null()
        && unsafe { (&*effective_pg_conn).is_pg_active } != 0;

    let use_dummy_shadow = !is_passthrough
        && (is_ddl_on_pg || unsafe { should_use_dummy_shadow(pg_conn, z_sql, is_read, is_write) });

    let mut pre_trans: SqlTranslation = unsafe { std::mem::zeroed() };
    let mut have_pre_trans = false;

    let rc = if use_dummy_shadow {
        let dummy = unsafe { prepare_dummy_shadow_stmt(db, z_sql, bytes, pp_stmt, pz_tail) };
        pre_trans = dummy.pre_trans;
        have_pre_trans = dummy.have_pre_trans;
        if dummy.rc != SQLITE_OK {
            return dummy.rc;
        }
        dummy.rc
    } else {
        let rc = unsafe {
            prepare_real_sqlite_stmt(skip_complex_processing, db, z_sql, n_byte, pp_stmt, pz_tail)
        };
        if rc != SQLITE_OK {
            return rc;
        }
        rc
    };

    let registered = unsafe {
        clear_connection_error_state(db);
        maybe_register_pg_stmt(
            effective_pg_conn,
            z_sql,
            bytes,
            pp_stmt,
            is_read,
            is_write,
            &mut pre_trans,
            &mut have_pre_trans,
        )
    };

    if have_pre_trans {
        unsafe { sql_translation_free(&mut pre_trans as *mut SqlTranslation) };
    }

    if !registered {
        return SQLITE_ERROR;
    }
    rc
}
