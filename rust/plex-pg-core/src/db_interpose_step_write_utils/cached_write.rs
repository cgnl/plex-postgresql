use super::*;
use crate::log_debug_lazy;

#[no_mangle]
pub extern "C" fn rust_step_cached_write_should_noop(
    base_conn: *mut PgConnection,
    sql: *const c_char,
    out_exec_conn: *mut *mut PgConnection,
) -> c_int {
    unsafe {
        let exec_conn = rust_step_pick_thread_connection(base_conn);
        if !out_exec_conn.is_null() {
            *out_exec_conn = exec_conn;
        }
        crate::db_interpose_txn_utils::rust_txn_terminator_should_noop(
            exec_conn,
            sql,
            std::ptr::null_mut(),
        )
    }
}

#[no_mangle]
pub extern "C" fn rust_step_pg_write_should_noop(
    exec_conn: *mut PgConnection,
    pg_sql: *const c_char,
    txn_state_out: *mut c_int,
) -> c_int {
    crate::db_interpose_txn_utils::rust_txn_terminator_should_noop(exec_conn, pg_sql, txn_state_out)
}

#[no_mangle]
pub extern "C" fn rust_step_cached_write_build_exec_sql(
    orig_sql: *const c_char,
    translated_sql: *const c_char,
    exec_sql_out: *mut *const c_char,
) -> *mut c_char {
    unsafe {
        if !exec_sql_out.is_null() {
            *exec_sql_out = translated_sql;
        }
        if translated_sql.is_null() {
            return std::ptr::null_mut();
        }

        let owned = crate::pg_statement::rust_convert_metadata_settings_upsert(translated_sql);
        if !owned.is_null() {
            if !exec_sql_out.is_null() {
                *exec_sql_out = owned;
            }
            return owned;
        }

        let orig_bytes = cstr_bytes(orig_sql);
        let translated_bytes = cstr_bytes(translated_sql);

        if !orig_bytes.is_empty()
            && starts_with_icase_bytes(orig_bytes, b"INSERT")
            && contains_icase_bytes(translated_bytes, b"schema_migrations")
            && !contains_icase_bytes(translated_bytes, b"ON CONFLICT")
        {
            let base = cstr_to_string_or(translated_sql, "");
            let sql = format!("{base} ON CONFLICT DO NOTHING");
            let ptr = malloc_cstring(&sql);
            if !exec_sql_out.is_null() {
                *exec_sql_out = ptr;
            }
            return ptr;
        }

        if !orig_bytes.is_empty()
            && starts_with_icase_bytes(orig_bytes, b"INSERT")
            && !contains_bytes(translated_bytes, b"RETURNING")
            && !contains_icase_bytes(translated_bytes, b"schema_migrations")
        {
            let base = cstr_to_string_or(translated_sql, "");
            let sql = format!("{base} RETURNING id");
            let ptr = malloc_cstring(&sql);
            if !exec_sql_out.is_null() {
                *exec_sql_out = ptr;
            }
            return ptr;
        }

        std::ptr::null_mut()
    }
}

#[no_mangle]
pub extern "C" fn rust_step_cached_write_execute_and_finalize(
    cached_io: *mut *mut PgStmt,
    p_stmt: *mut sqlite3_stmt,
    changes_conn: *mut PgConnection,
    exec_conn: *mut PgConnection,
    orig_sql: *const c_char,
    exec_sql: *const c_char,
    pg_conn_error_out: *mut c_int,
) -> c_int {
    unsafe {
        if !pg_conn_error_out.is_null() {
            *pg_conn_error_out = 0;
        }
        if exec_conn.is_null()
            || (&*exec_conn).conn.is_null()
            || orig_sql.is_null()
            || exec_sql.is_null()
        {
            if !pg_conn_error_out.is_null() {
                *pg_conn_error_out = 1;
            }
            return STEP_RESULT_ERROR;
        }

        if contains_bytes(cstr_bytes(orig_sql), b"play_queue_generators") {
            log_debug_lazy!(
                "CACHED INSERT play_queue_generators on thread {:p} conn {:p}",
                libc::pthread_self() as *mut c_void,
                exec_conn
            );
        }

        crate::pg_client::rust_pool_touch_connection(exec_conn as *const c_void);
        let ec = &mut *exec_conn;
        let mut conn_guard = PthreadMutexGuard::lock(&mut ec.mutex as *mut _);

        if ec.conn.is_null() {
            log_error("CACHED EXEC: conn became NULL after lock (TOCTOU race)");
            conn_guard.unlock();
            if !pg_conn_error_out.is_null() {
                *pg_conn_error_out = 1;
            }
            return STEP_RESULT_ERROR;
        }

        let scope = CString::new("CACHED EXEC").unwrap();
        crate::db_interpose_conn_utils::rust_step_conn_cancel_and_drain(exec_conn, scope.as_ptr());

        let sql_hash = crate::pg_client::rust_hash_sql(exec_sql);
        let stmt_name = format!("ce_{:x}", sql_hash);
        let mut stmt_name_buf = [0 as c_char; STMT_NAME_LEN];
        let bytes = stmt_name.as_bytes();
        let len = bytes.len().min(STMT_NAME_LEN.saturating_sub(1));
        for i in 0..len {
            stmt_name_buf[i] = bytes[i] as c_char;
        }
        stmt_name_buf[len] = 0;

        let mut cached_stmt_name: *const c_char = std::ptr::null();
        let res: *mut PGresult = if crate::pg_client::rust_stmt_cache_lookup(
            exec_conn as *mut c_void,
            sql_hash,
            &mut cached_stmt_name,
        ) != 0
        {
            crate::libpq_helpers::rust_pq_exec_prepared(
                ec.conn,
                cached_stmt_name,
                0,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                0,
            )
        } else {
            let prep_res = crate::libpq_helpers::rust_pq_prepare(
                ec.conn,
                stmt_name_buf.as_ptr(),
                exec_sql,
                0,
                std::ptr::null(),
            );
            if crate::libpq_helpers::rust_pq_result_status(prep_res) == PGRES_COMMAND_OK {
                crate::pg_client::rust_stmt_cache_add(
                    exec_conn as *mut c_void,
                    sql_hash,
                    stmt_name_buf.as_ptr(),
                    0,
                );
                crate::libpq_helpers::rust_pq_clear(prep_res);
                crate::libpq_helpers::rust_pq_exec_prepared(
                    ec.conn,
                    stmt_name_buf.as_ptr(),
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                )
            } else if is_duplicate_prepared_stmt(prep_res) {
                crate::pg_client::rust_stmt_cache_add(
                    exec_conn as *mut c_void,
                    sql_hash,
                    stmt_name_buf.as_ptr(),
                    0,
                );
                crate::libpq_helpers::rust_pq_clear(prep_res);
                crate::libpq_helpers::rust_pq_exec_prepared(
                    ec.conn,
                    stmt_name_buf.as_ptr(),
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                )
            } else {
                log_debug_lazy!(
                    "CACHED EXEC prepare failed, using PQexec: {}",
                    cstr_to_string_or(
                        crate::libpq_helpers::rust_pq_error_message(ec.conn),
                        "(null)"
                    )
                );
                crate::libpq_helpers::rust_pq_clear(prep_res);
                crate::libpq_helpers::rust_pq_exec(ec.conn, exec_sql)
            }
        };
        conn_guard.unlock();

        let status = crate::libpq_helpers::rust_pq_result_status(res);
        if status == PGRES_COMMAND_OK || status == PGRES_TUPLES_OK {
            if !changes_conn.is_null() {
                let cc = &mut *changes_conn;
                let cmd_tuples = crate::libpq_helpers::rust_pq_cmd_tuples(res);
                let tuples_ptr = if cmd_tuples.is_null() {
                    b"1\0".as_ptr() as *const c_char
                } else {
                    cmd_tuples
                };
                cc.last_changes = crate::db_interpose_helpers::rust_pg_text_to_int(tuples_ptr);
            }

            if starts_with_icase_bytes(cstr_bytes(orig_sql), b"INSERT")
                && status == PGRES_TUPLES_OK
                && crate::libpq_helpers::rust_pq_ntuples(res) > 0
            {
                let mut id_buf = [0 as c_char; 64];
                let mut id_str: *const c_char = std::ptr::null();
                if crate::db_interpose_helpers::rust_pg_result_text_copy(
                    res as *const crate::db_interpose_helpers::PGresult,
                    0,
                    0,
                    id_buf.as_mut_ptr(),
                    id_buf.len(),
                ) >= 0
                {
                    id_str = id_buf.as_ptr();
                }
                if !id_str.is_null() && !CStr::from_ptr(id_str).to_bytes().is_empty() {
                    // Mirror the non-cached write path (write_exec.rs): publish
                    // the returned id as `last_insert_rowid` on both the
                    // executing connection and the global atomic. Without this,
                    // a cached INSERT with `RETURNING id` produces a row on
                    // PG but `sqlite3_last_insert_rowid()` returns 0, which
                    // Plex's SOCI wrapper interprets as "insert failed" and
                    // throws DB::Exception from every async-task tick.
                    let rowid = crate::db_interpose_helpers::rust_pg_text_to_int64(id_str);
                    if rowid > 0 {
                        let ec = &mut *exec_conn;
                        ec.last_insert_rowid = rowid;
                        crate::pg_client::rust_set_global_last_insert_rowid(rowid);
                    }
                    let meta_id = crate::pg_statement::rust_extract_metadata_id(orig_sql);
                    if meta_id > 0 {
                        crate::pg_client::rust_set_global_metadata_id(meta_id);
                    }
                }
            }
        } else {
            let err = if !changes_conn.is_null() && !(&*changes_conn).conn.is_null() {
                crate::libpq_helpers::rust_pq_error_message((&*changes_conn).conn)
            } else {
                b"NULL connection\0".as_ptr() as *const c_char
            };
            log_sql_fallback(
                orig_sql,
                exec_sql,
                err,
                b"CACHED WRITE\0".as_ptr() as *const c_char,
            );
            if is_stale_prepared_stmt(res) {
                crate::pg_client::rust_stmt_cache_clear_local(exec_conn as *mut c_void);
                crate::libpq_helpers::rust_pq_clear(res);
                if !pg_conn_error_out.is_null() {
                    *pg_conn_error_out = 1;
                }
                return STEP_RESULT_ERROR;
            }
            crate::pg_client::rust_pool_check_health(exec_conn as *mut c_void);
        }

        if !res.is_null() {
            crate::libpq_helpers::rust_pq_clear(res);
        }

        let mut cached = if !cached_io.is_null() {
            *cached_io
        } else {
            std::ptr::null_mut()
        };
        if cached.is_null() {
            cached = crate::pg_statement::rust_stmt_create(exec_conn, orig_sql, p_stmt);
            if !cached.is_null() {
                let c = &mut *cached;
                c.is_pg = 1;
                c.is_cached = 1;
                c.write_executed = 1;
                crate::pg_statement::rust_cached_stmt_register(p_stmt as usize, cached as usize);
            }
            if !cached_io.is_null() {
                *cached_io = cached;
            }
        } else {
            let c = &mut *cached;
            c.write_executed = 1;
        }

        STEP_RESULT_DONE
    }
}
