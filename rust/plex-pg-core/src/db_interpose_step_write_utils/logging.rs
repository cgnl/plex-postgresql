use super::*;
use crate::log_debug_lazy;

#[no_mangle]
pub extern "C" fn rust_step_write_log_debug_context(
    pg_stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
    param_values: *const *const c_char,
) {
    if pg_stmt.is_null() {
        return;
    }
    let stmt = unsafe { &*pg_stmt };

    unsafe {
        let has_match = !stmt.sql.is_null()
            && (contains_icase_bytes(cstr_bytes(stmt.sql), b"devices")
                || contains_icase_bytes(cstr_bytes(stmt.sql), b"library_sections"))
            || !stmt.pg_sql.is_null()
            && (contains_icase_bytes(cstr_bytes(stmt.pg_sql), b"devices")
                || contains_icase_bytes(cstr_bytes(stmt.pg_sql), b"library_sections"));
        if has_match {
            log_debug_lazy!(
                "STEP WRITE devices/library_sections: param_count={} is_pg={}",
                stmt.param_count,
                stmt.is_pg
            );
            for i in 0..(stmt.param_count as usize) {
                if i < stmt.param_values.len() {
                    let p = stmt.param_values[i];
                    log_debug_lazy!("  PARAM[{}]: {}", i, cstr_to_string_or(p, "NULL"));
                } else {
                    log_debug_lazy!("  PARAM[{}]: OUT_OF_BOUNDS", i);
                }
            }
            log_debug_lazy!("  SQL: {}", cstr_prefix(stmt.sql, 1000, "NULL"));
            if !stmt.pg_sql.is_null() {
                log_debug_lazy!("  PG_SQL: {}", cstr_prefix(stmt.pg_sql, 1000, "NULL"));
            }
        }

        if !stmt.pg_sql.is_null()
            && contains_bytes(cstr_bytes(stmt.pg_sql), b"play_queue_generators")
        {
            log_debug_lazy!(
                "INSERT play_queue_generators on thread {:p} conn {:p}",
                libc::pthread_self() as *mut c_void,
                exec_conn
            );
        }

        if !stmt.sql.is_null()
            && contains_icase_bytes(cstr_bytes(stmt.sql), b"INSERT INTO metadata_items")
        {
            let p0 = if stmt.param_count > 0 {
                param_at(param_values, 0)
            } else {
                std::ptr::null()
            };
            let p1 = if stmt.param_count > 1 {
                param_at(param_values, 1)
            } else {
                std::ptr::null()
            };
            let p2 = if stmt.param_count > 2 {
                param_at(param_values, 2)
            } else {
                std::ptr::null()
            };
            let p8 = if stmt.param_count > 8 {
                param_at(param_values, 8)
            } else {
                std::ptr::null()
            };
            let p9 = if stmt.param_count > 9 {
                param_at(param_values, 9)
            } else {
                std::ptr::null()
            };
            log_debug_lazy!(
                "STEP metadata_items INSERT: param_count={}",
                stmt.param_count
            );
            log_debug_lazy!(
                "  PARAMS: [0]={} [1]={} [2]={} [8]={} [9]={}",
                cstr_to_string_or(p0, "NULL"),
                cstr_to_string_or(p1, "NULL"),
                cstr_to_string_or(p2, "NULL"),
                cstr_to_string_or(p8, "NULL"),
                cstr_to_string_or(p9, "NULL")
            );
        }

        if !stmt.sql.is_null() && contains_bytes(cstr_bytes(stmt.sql), b"play_queue_generators") {
            let p0 = if stmt.param_count > 0 {
                param_at(param_values, 0)
            } else {
                std::ptr::null()
            };
            let p1 = if stmt.param_count > 1 {
                param_at(param_values, 1)
            } else {
                std::ptr::null()
            };
            let p2 = if stmt.param_count > 2 {
                param_at(param_values, 2)
            } else {
                std::ptr::null()
            };
            let p3 = if stmt.param_count > 3 {
                param_at(param_values, 3)
            } else {
                std::ptr::null()
            };
            log_debug_lazy!(
                "STEP play_queue_generators INSERT: param_count={}",
                stmt.param_count
            );
            log_debug_lazy!(
                "  PARAMS: [0]={} [1]={} [2]={} [3]={}",
                cstr_to_string_or(p0, "NULL"),
                cstr_to_string_or(p1, "NULL"),
                cstr_to_string_or(p2, "NULL"),
                cstr_to_string_or(p3, "NULL")
            );
            log_debug_lazy!("  SQL: {}", cstr_prefix(stmt.pg_sql, 300, "NULL"));
        }
    }
}

#[no_mangle]
pub extern "C" fn rust_step_log_step_exit_trace(pg_stmt: *mut PgStmt) {
    if pg_stmt.is_null() {
        return;
    }
    let stmt = unsafe { &*pg_stmt };
    if stmt.pg_sql.is_null() {
        return;
    }
    let sql_bytes = unsafe { cstr_bytes(stmt.pg_sql) };
    let is_count = contains_bytes(sql_bytes, b"COUNT(")
        || contains_bytes(sql_bytes, b"SUM(")
        || contains_bytes(sql_bytes, b"MAX(");
    let is_playqueue = contains_bytes(sql_bytes, b"play_queue");

    if is_count || is_playqueue {
        log_debug_lazy!(
            "DEBUG_TRACE: STEP_EXIT - rows={} cols={} sql={}",
            stmt.num_rows,
            stmt.num_cols,
            cstr_prefix(stmt.pg_sql, 100, "NULL")
        );
    }
}
