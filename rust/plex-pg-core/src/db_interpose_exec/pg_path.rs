use super::support::{
    is_duplicate_prepared_stmt, is_stale_prepared_stmt, malloc_cstring, parse_returning_rowid,
};
use super::*;
use crate::log_info_lazy;

pub(crate) unsafe fn set_pg_last_error(pg: *mut crate::ffi_types::PgConnection, msg: &str) {
    let Some(pg) = pg.as_mut() else { return };
    pg.last_error_code = SQLITE_ERROR;
    pg.last_error.fill(0);
    let bytes = msg.as_bytes();
    let len = bytes.len().min(pg.last_error.len().saturating_sub(1));
    for (dst, src) in pg.last_error.iter_mut().zip(bytes.iter()).take(len) {
        *dst = *src as c_char;
    }
}

pub(crate) unsafe fn record_pg_result_error(
    target: *mut crate::ffi_types::PgConnection,
    exec_conn: *mut crate::ffi_types::PgConnection,
    res: *mut PGresult,
) {
    let state = cstr_to_string_or(
        crate::libpq_helpers::rust_pq_result_error_field(res, PG_DIAG_SQLSTATE),
        "",
    );
    let result_message =
        cstr_to_string_or(crate::libpq_helpers::rust_pq_result_error_message(res), "");
    let message = if !result_message.trim().is_empty() {
        result_message
    } else if !exec_conn.is_null() && !(*exec_conn).conn.is_null() {
        cstr_to_string_or(
            crate::libpq_helpers::rust_pq_error_message((*exec_conn).conn),
            "PostgreSQL execution failed",
        )
    } else {
        "PostgreSQL connection unavailable".to_owned()
    };
    let message = if state.is_empty() {
        format!(
            "PostgreSQL execution failed (outcome may be unknown): {}",
            message.trim()
        )
    } else {
        format!("[SQLSTATE {state}] {}", message.trim())
    };
    set_pg_last_error(target, &message);
    if let Some(target) = target.as_mut() {
        target.last_error_code = match state.as_str() {
            "23505" => 2067,
            "23502" => 1299,
            "23503" => 787,
            "23514" => 275,
            "23P01" => 19,
            _ => SQLITE_ERROR,
        };
    }
}

pub(crate) fn primary_error_code(target: *mut crate::ffi_types::PgConnection) -> c_int {
    if target.is_null() {
        return SQLITE_ERROR;
    }
    let code = unsafe { (*target).last_error_code } & 255;
    if code == SQLITE_OK {
        SQLITE_ERROR
    } else {
        code
    }
}

pub(crate) unsafe fn clear_pg_last_error(pg: *mut crate::ffi_types::PgConnection) {
    if let Some(pg) = pg.as_mut() {
        pg.last_error_code = SQLITE_OK;
        pg.last_error.fill(0);
    }
}

pub(crate) unsafe fn copy_pg_outcome(
    target: *mut crate::ffi_types::PgConnection,
    source: *mut crate::ffi_types::PgConnection,
) {
    if target.is_null() || source.is_null() || target == source {
        return;
    }
    (*target).last_error_code = (*source).last_error_code;
    (*target).last_error = (*source).last_error;
    (*target).last_changes = (*source).last_changes;
}

pub(crate) fn exec_via_postgres(
    pg_conn: *mut crate::ffi_types::PgConnection,
    sql: *const c_char,
    handle_conn: *mut crate::ffi_types::PgConnection,
) -> c_int {
    let pg = unsafe { &mut *pg_conn };
    unsafe {
        if pg.conn.is_null() || crate::libpq_helpers::rust_pq_status(pg.conn) != CONNECTION_OK {
            set_pg_last_error(
                pg_conn,
                "PostgreSQL execution connection unavailable; session was not replaced",
            );
            pg.last_changes = 0;
            return SQLITE_ERROR;
        }

        let mut exec_sql = sql;
        let blobs_rewrite = rewrite_blobs_schema_migrations(sql, pg.db_path.as_ptr());
        if !blobs_rewrite.is_null() {
            exec_sql = blobs_rewrite;
        }

        if crate::pg_config::pg_config_should_skip_sql(exec_sql) == 0 {
            if crate::db_interpose_helpers::rust_is_junk_metadata_insert(exec_sql) != 0 {
                log_error(
                    "GUARD: Blocked exec junk INSERT into metadata_items (library_section_id=NULL, metadata_type=NULL)",
                );
                if !blobs_rewrite.is_null() {
                    libc::free(blobs_rewrite as *mut c_void);
                }
                return SQLITE_OK;
            }

            let mut trans = sql_translate(exec_sql);
            if trans.success != 0 && !trans.sql.is_null() {
                let mut owned_insert: *mut c_char = std::ptr::null_mut();
                let mut exec_pg_sql = trans.sql;
                let sql_bytes = CStr::from_ptr(exec_sql).to_bytes();

                if starts_with_icase_bytes(sql_bytes, b"INSERT")
                    && !contains_bytes(CStr::from_ptr(trans.sql).to_bytes(), b"RETURNING")
                {
                    let base = cstr_to_string_or(trans.sql, "");
                    let sql = format!("{base} RETURNING id");
                    owned_insert = malloc_cstring(&sql);
                    if !owned_insert.is_null() {
                        exec_pg_sql = owned_insert;
                        if contains_bytes(sql_bytes, b"play_queue_generators") {
                            log_info_lazy!(
                                "EXEC play_queue_generators INSERT with RETURNING: {}",
                                cstr_prefix(exec_pg_sql, 300, "NULL")
                            );
                        }
                    }
                }

                let mut conn_guard = PthreadMutexGuard::lock(&mut pg.mutex as *mut _);

                let normalized =
                    crate::db_interpose_helpers::rust_normalize_sql_literals(exec_pg_sql);
                let replacement =
                    crate::db_interpose_exec::replace_guard::is_replacement(exec_pg_sql);
                let res: *mut PGresult = crate::db_interpose_exec::replace_guard::execute_locked(
                    pg_conn,
                    exec_pg_sql,
                    || {
                        if !normalized.is_null() {
                            let norm = &*normalized;
                            let norm_hash = crate::pg_client::rust_hash_sql(norm.normalized_sql);
                            let mut cached_stmt_name: *const c_char = std::ptr::null();

                            if crate::pg_client::rust_stmt_cache_lookup(
                                pg_conn as *mut c_void,
                                norm_hash,
                                &mut cached_stmt_name,
                            ) != 0
                            {
                                crate::libpq_helpers::rust_pq_exec_prepared(
                                    pg.conn,
                                    cached_stmt_name,
                                    norm.param_count,
                                    norm.param_values as *const *const c_char,
                                    std::ptr::null(),
                                    std::ptr::null(),
                                    0,
                                )
                            } else {
                                let stmt_name = format!("nx_{:x}", norm_hash);
                                let stmt_name_c = CString::new(stmt_name)
                                    .unwrap_or_else(|_| CString::new("").unwrap());
                                let prep_res = crate::libpq_helpers::rust_pq_prepare(
                                    pg.conn,
                                    stmt_name_c.as_ptr(),
                                    norm.normalized_sql,
                                    0,
                                    std::ptr::null(),
                                );
                                if replacement
                                    && crate::libpq_helpers::rust_pq_result_status(prep_res)
                                        != PGRES_COMMAND_OK
                                {
                                    return prep_res;
                                }
                                let ok = crate::libpq_helpers::rust_pq_result_status(prep_res)
                                    == PGRES_COMMAND_OK
                                    || is_duplicate_prepared_stmt(prep_res);
                                if ok {
                                    crate::pg_client::rust_stmt_cache_add(
                                        pg_conn as *mut c_void,
                                        norm_hash,
                                        stmt_name_c.as_ptr(),
                                        norm.param_count,
                                    );
                                    crate::libpq_helpers::rust_pq_clear(prep_res);
                                    crate::libpq_helpers::rust_pq_exec_prepared(
                                        pg.conn,
                                        stmt_name_c.as_ptr(),
                                        norm.param_count,
                                        norm.param_values as *const *const c_char,
                                        std::ptr::null(),
                                        std::ptr::null(),
                                        0,
                                    )
                                } else {
                                    crate::libpq_helpers::rust_pq_clear(prep_res);
                                    crate::libpq_helpers::rust_pq_exec(pg.conn, exec_pg_sql)
                                }
                            }
                        } else {
                            let sql_hash = crate::pg_client::rust_hash_sql(exec_pg_sql);
                            let mut cached_stmt_name: *const c_char = std::ptr::null();
                            if crate::pg_client::rust_stmt_cache_lookup(
                                pg_conn as *mut c_void,
                                sql_hash,
                                &mut cached_stmt_name,
                            ) != 0
                            {
                                crate::libpq_helpers::rust_pq_exec_prepared(
                                    pg.conn,
                                    cached_stmt_name,
                                    0,
                                    std::ptr::null(),
                                    std::ptr::null(),
                                    std::ptr::null(),
                                    0,
                                )
                            } else {
                                crate::libpq_helpers::rust_pq_exec(pg.conn, exec_pg_sql)
                            }
                        }
                    },
                );

                if !normalized.is_null() {
                    crate::db_interpose_helpers::rust_free_normalized_sql(normalized);
                }

                let status = crate::libpq_helpers::rust_pq_result_status(res);
                if status == PGRES_COMMAND_OK || status == PGRES_TUPLES_OK {
                    clear_pg_last_error(pg_conn);
                    let cmd_tuples = crate::libpq_helpers::rust_pq_cmd_tuples(res);
                    let tuples_ptr = if cmd_tuples.is_null() {
                        c"1".as_ptr()
                    } else {
                        cmd_tuples
                    };
                    pg.last_changes = crate::db_interpose_helpers::rust_pg_text_to_int(tuples_ptr);

                    if (starts_with_icase_bytes(sql_bytes, b"INSERT") || replacement)
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
                            if let Some(rowid) = parse_returning_rowid(id_str) {
                                pg.last_insert_rowid = rowid;
                                if !handle_conn.is_null() {
                                    (*handle_conn).last_insert_rowid = rowid;
                                }
                                crate::pg_client::rust_set_global_last_insert_rowid(rowid);
                            }
                            if contains_bytes(sql_bytes, b"play_queue_generators") {
                                log_info_lazy!(
                                    "EXEC play_queue_generators: RETURNING id = {}",
                                    cstr_to_string_or(id_str, "?")
                                );
                            }
                            let meta_id = crate::pg_statement::rust_extract_metadata_id(exec_sql);
                            if meta_id > 0 {
                                crate::pg_client::rust_set_global_metadata_id(meta_id);
                            }
                        }
                    }
                } else {
                    let err = if pg.conn.is_null() {
                        c"NULL connection".as_ptr()
                    } else {
                        crate::libpq_helpers::rust_pq_error_message(pg.conn)
                    };
                    log_error(&format!(
                        "PostgreSQL exec error: {}",
                        cstr_to_string_or(err, "NULL connection")
                    ));
                    record_pg_result_error(pg_conn, pg_conn, res);
                    pg.last_changes = 0;
                    let is_stale_stmt = is_stale_prepared_stmt(res);
                    if is_stale_stmt {
                        crate::pg_client::rust_stmt_cache_clear_local(pg_conn as *mut c_void);
                    }
                    if !owned_insert.is_null() {
                        libc::free(owned_insert as *mut c_void);
                    }
                    crate::libpq_helpers::rust_pq_clear(res);
                    conn_guard.unlock();
                    crate::pg_client::rust_pool_check_health(pg_conn as *mut c_void);
                    sql_translation_free(&mut trans as *mut SqlTranslation);
                    if !blobs_rewrite.is_null() {
                        libc::free(blobs_rewrite as *mut c_void);
                    }
                    return SQLITE_ERROR;
                }

                if !owned_insert.is_null() {
                    libc::free(owned_insert as *mut c_void);
                }
                crate::libpq_helpers::rust_pq_clear(res);
                conn_guard.unlock();
            } else {
                let err = cstr_to_string_or(trans.error.as_ptr(), "translation failed");
                let msg = format!(
                    "PG exec translation failed: {} :: {}",
                    cstr_prefix(exec_sql, 220, "NULL"),
                    err
                );
                log_error(&msg);
                set_pg_last_error(pg_conn, &msg);
                sql_translation_free(&mut trans as *mut SqlTranslation);
                if !blobs_rewrite.is_null() {
                    libc::free(blobs_rewrite as *mut c_void);
                }
                return SQLITE_ERROR;
            }
            sql_translation_free(&mut trans as *mut SqlTranslation);
        }

        if !blobs_rewrite.is_null() {
            libc::free(blobs_rewrite as *mut c_void);
        }
        clear_pg_last_error(pg_conn);
        SQLITE_OK
    }
}
