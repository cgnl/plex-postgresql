use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};

use crate::db_interpose_conn_utils::{cstr_prefix, log_debug, log_error};
use crate::ffi_types::{sqlite3_stmt, PgConnection, PgStmt};

use super::super::support::{sql_translate, sql_translation_free};
use super::super::{note_pg_conn_error, STEP_RESULT_DONE, STEP_RESULT_ERROR};
use super::free_expanded_sql;
use crate::log_debug_lazy;

unsafe fn set_cached_write_error(pg_conn: *mut PgConnection, msg: &str) {
    if pg_conn.is_null() {
        return;
    }
    let conn = &mut *pg_conn;
    conn.last_error_code = STEP_RESULT_ERROR;
    conn.last_error.fill(0);
    let bytes = msg.as_bytes();
    let len = bytes.len().min(conn.last_error.len().saturating_sub(1));
    for (dst, src) in conn.last_error.iter_mut().zip(bytes.iter()).take(len) {
        *dst = *src as c_char;
    }
}

pub(super) unsafe fn handle_cached_write(
    p_stmt: *mut sqlite3_stmt,
    pg_conn: *mut PgConnection,
    sql: *const c_char,
    expanded_sql: *mut c_char,
) -> c_int {
    if !sql.is_null()
        && CStr::from_ptr(sql)
            .to_bytes()
            .windows(b"INSERT".len())
            .any(|w| w.eq_ignore_ascii_case(b"INSERT"))
        && CStr::from_ptr(sql)
            .to_bytes()
            .windows(b"metadata_items".len())
            .any(|w| w.eq_ignore_ascii_case(b"metadata_items"))
    {
        log_debug("CACHED INSERT metadata_items:");
        log_debug_lazy!(
            "  expanded_sql={}",
            if expanded_sql.is_null() { "NO" } else { "YES" }
        );
        log_debug_lazy!("  sql (first 300): {}", cstr_prefix(sql, 300, "(null)"));
    }
    if crate::db_interpose_helpers::rust_is_junk_metadata_insert(sql) != 0 {
        log_error(
            "GUARD: Blocked cached junk INSERT into metadata_items (library_section_id=NULL, metadata_type=NULL)",
        );
        free_expanded_sql(expanded_sql);
        return STEP_RESULT_DONE;
    }

    let mut cached = crate::pg_statement::rust_cached_stmt_find(p_stmt as usize) as *mut PgStmt;
    if !cached.is_null() && (&*cached).write_executed != 0 {
        free_expanded_sql(expanded_sql);
        return if (&*cached).write_executed < 0 {
            STEP_RESULT_ERROR
        } else {
            STEP_RESULT_DONE
        };
    }

    let mut cached_exec_conn: *mut PgConnection = std::ptr::null_mut();
    if crate::db_interpose_step_write_utils::rust_step_cached_write_should_noop(
        pg_conn,
        sql,
        &mut cached_exec_conn,
    ) != 0
    {
        free_expanded_sql(expanded_sql);
        return STEP_RESULT_DONE;
    }

    let mut trans = sql_translate(sql);
    if trans.success != 0 && !trans.sql.is_null() {
        let mut exec_sql = trans.sql as *const c_char;
        let insert_sql =
            crate::db_interpose_step_write_utils::rust_step_cached_write_build_exec_sql(
                sql,
                trans.sql,
                &mut exec_sql,
            );
        let mut cached_write_conn_error = 0;
        let cached_write_rc =
            crate::db_interpose_step_write_utils::rust_step_cached_write_execute_and_finalize(
                &mut cached,
                p_stmt,
                pg_conn,
                cached_exec_conn,
                sql,
                exec_sql,
                &mut cached_write_conn_error,
            );
        if !insert_sql.is_null() {
            libc::free(insert_sql as *mut c_void);
        }
        if cached_write_rc == STEP_RESULT_ERROR {
            sql_translation_free(&mut trans as *mut _);
            free_expanded_sql(expanded_sql);
            if cached_write_conn_error != 0 {
                note_pg_conn_error();
            }
            return STEP_RESULT_ERROR;
        }
    } else {
        let msg = format!(
            "PG cached write translation failed: {}",
            cstr_prefix(sql, 220, "NULL")
        );
        log_error(&msg);
        set_cached_write_error(pg_conn, &msg);
        sql_translation_free(&mut trans as *mut _);
        free_expanded_sql(expanded_sql);
        return STEP_RESULT_ERROR;
    }
    sql_translation_free(&mut trans as *mut _);
    free_expanded_sql(expanded_sql);
    STEP_RESULT_DONE
}
