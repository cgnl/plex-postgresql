use super::*;
use crate::log_debug_lazy;

#[no_mangle]
pub extern "C" fn rust_step_write_execute_and_finalize(
    pg_stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
    param_values: *const *const c_char,
    pg_conn_error_out: *mut c_int,
) -> c_int {
    unsafe {
        if !pg_conn_error_out.is_null() {
            *pg_conn_error_out = 0;
        }
        if !pg_stmt.is_null() && (*pg_stmt).write_executed < 0 {
            return STEP_RESULT_ERROR;
        }
        if pg_stmt.is_null() || exec_conn.is_null() || (&*exec_conn).conn.is_null() {
            if !pg_stmt.is_null() {
                let s = &mut *pg_stmt;
                s.write_executed = -1;
                crate::db_interpose_exec::pg_path::set_pg_last_error(
                    s.conn,
                    "PostgreSQL write connection unavailable",
                );
            }
            if !pg_conn_error_out.is_null() {
                *pg_conn_error_out = 1;
            }
            return STEP_RESULT_ERROR;
        }
        let stmt = &mut *pg_stmt;
        let ec = &mut *exec_conn;
        let handle_conn = if stmt.shadow_stmt.is_null() {
            stmt.conn
        } else {
            crate::pg_client::rust_pg_find_handle_connection(sqlite3_db_handle(stmt.shadow_stmt))
        };
        let mut conn_guard = PthreadMutexGuard::lock(&mut ec.mutex as *mut _);
        if ec.conn.is_null() {
            crate::db_interpose_exec::pg_path::set_pg_last_error(
                handle_conn,
                "PostgreSQL write connection unavailable",
            );
            stmt.write_executed = -1;
            return STEP_RESULT_ERROR;
        }

        if skip_stats_resources_update()
            && !stmt.pg_sql.is_null()
            && starts_with_icase_bytes(cstr_bytes(stmt.pg_sql), b"UPDATE")
            && contains_icase_bytes(cstr_bytes(stmt.pg_sql), b"statistics_resources")
        {
            log_error("STEP WRITE: skipping statistics_resources UPDATE via PLEX_PG_SKIP_STATS_RESOURCES_UPDATE");
            ec.last_changes = 0;
            crate::db_interpose_exec::pg_path::clear_pg_last_error(exec_conn);
            crate::db_interpose_exec::pg_path::copy_pg_outcome(handle_conn, exec_conn);
            conn_guard.unlock();
            stmt.write_executed = 1;
            return STEP_RESULT_DONE;
        }

        let res: *mut PGresult = if stmt.use_prepared != 0 && stmt.stmt_name[0] != 0 {
            let mut cached_name: *const c_char = std::ptr::null();
            let mut is_cached = crate::pg_client::rust_stmt_cache_lookup(
                exec_conn as *mut c_void,
                stmt.sql_hash,
                &mut cached_name,
            ) != 0;

            if !is_cached {
                let prep_res = crate::libpq_helpers::rust_pq_prepare(
                    ec.conn,
                    stmt.stmt_name.as_ptr(),
                    stmt.pg_sql,
                    stmt.param_count,
                    std::ptr::null(),
                );
                if crate::libpq_helpers::rust_pq_result_status(prep_res) == PGRES_COMMAND_OK {
                    crate::pg_client::rust_stmt_cache_add(
                        exec_conn as *mut c_void,
                        stmt.sql_hash,
                        stmt.stmt_name.as_ptr(),
                        stmt.param_count,
                    );
                    cached_name = stmt.stmt_name.as_ptr();
                    is_cached = true;
                } else if is_duplicate_prepared_stmt(prep_res) {
                    crate::pg_client::rust_stmt_cache_add(
                        exec_conn as *mut c_void,
                        stmt.sql_hash,
                        stmt.stmt_name.as_ptr(),
                        stmt.param_count,
                    );
                    cached_name = stmt.stmt_name.as_ptr();
                    is_cached = true;
                } else {
                    log_debug_lazy!(
                        "PQprepare (write) failed for {}: {}",
                        cstr_to_string_or(stmt.stmt_name.as_ptr(), ""),
                        cstr_to_string_or(
                            crate::libpq_helpers::rust_pq_error_message(ec.conn),
                            "(null)"
                        )
                    );
                }
                crate::libpq_helpers::rust_pq_clear(prep_res);
            }

            if is_cached && !cached_name.is_null() {
                crate::libpq_helpers::rust_pq_exec_prepared(
                    ec.conn,
                    cached_name,
                    stmt.param_count,
                    param_values,
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                )
            } else {
                crate::libpq_helpers::rust_pq_exec_params(
                    ec.conn,
                    stmt.pg_sql,
                    stmt.param_count,
                    std::ptr::null(),
                    param_values,
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                )
            }
        } else {
            crate::libpq_helpers::rust_pq_exec_params(
                ec.conn,
                stmt.pg_sql,
                stmt.param_count,
                std::ptr::null(),
                param_values,
                std::ptr::null(),
                std::ptr::null(),
                0,
            )
        };

        let status = crate::libpq_helpers::rust_pq_result_status(res);
        if status == PGRES_COMMAND_OK || status == PGRES_TUPLES_OK {
            crate::db_interpose_exec::pg_path::clear_pg_last_error(exec_conn);
            let cmd_tuples = crate::libpq_helpers::rust_pq_cmd_tuples(res);
            let tuples_ptr = if cmd_tuples.is_null() {
                b"1\0".as_ptr() as *const c_char
            } else {
                cmd_tuples
            };
            ec.last_changes = crate::db_interpose_helpers::rust_pg_text_to_int(tuples_ptr);

            if starts_with_icase_bytes(cstr_bytes(stmt.sql), b"INSERT")
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
                    let rowid = crate::db_interpose_helpers::rust_pg_text_to_int64(id_str);
                    if rowid > 0 {
                        ec.last_insert_rowid = rowid;
                        if !handle_conn.is_null() {
                            (*handle_conn).last_insert_rowid = rowid;
                        }
                        crate::pg_client::rust_set_global_last_insert_rowid(rowid);
                    }

                    if !stmt.pg_sql.is_null()
                        && contains_bytes(cstr_bytes(stmt.pg_sql), b"play_queue_generators")
                    {
                        log_debug_lazy!(
                            "STEP play_queue_generators: RETURNING id = {} on thread {:p} conn {:p}",
                            cstr_to_string_or(id_str, "?"),
                            libc::pthread_self() as *mut c_void,
                            exec_conn
                        );
                    }
                    let meta_id = crate::pg_statement::rust_extract_metadata_id(stmt.sql);
                    if meta_id > 0 {
                        crate::pg_client::rust_set_global_metadata_id(meta_id);
                    }
                }
            }
        } else {
            crate::db_interpose_exec::pg_path::record_pg_result_error(exec_conn, exec_conn, res);
            ec.last_changes = 0;
            crate::db_interpose_exec::pg_path::copy_pg_outcome(handle_conn, exec_conn);
            let err = if !exec_conn.is_null() && !ec.conn.is_null() {
                crate::libpq_helpers::rust_pq_error_message(ec.conn)
            } else {
                b"NULL connection\0".as_ptr() as *const c_char
            };
            log_error(&format!(
                "STEP PG write error: {}",
                cstr_to_string_or(err, "NULL connection")
            ));
            log_error(&format!(
                "  Original SQL: {}",
                cstr_prefix(stmt.sql, 300, "(null)")
            ));
            log_error(&format!(
                "  Translated SQL: {}",
                cstr_prefix(stmt.pg_sql, 300, "(null)")
            ));
            if is_stale_prepared_stmt(res) {
                crate::pg_client::rust_stmt_cache_clear_local(exec_conn as *mut c_void);
            }
            crate::libpq_helpers::rust_pq_clear(res);
            stmt.write_executed = -1;
            conn_guard.unlock();
            crate::pg_client::rust_pool_check_health(exec_conn as *mut c_void);
            return STEP_RESULT_ERROR;
        }

        crate::db_interpose_exec::pg_path::copy_pg_outcome(handle_conn, exec_conn);
        stmt.write_executed = 1;
        if !res.is_null() {
            crate::libpq_helpers::rust_pq_clear(res);
        }
        STEP_RESULT_DONE
    }
}
