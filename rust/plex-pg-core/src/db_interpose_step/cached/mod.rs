use std::os::raw::{c_char, c_int, c_void};

use crate::ffi_types::sqlite3_stmt;

mod read;
mod write;

use read::handle_cached_read;
use write::handle_cached_write;

use super::support::{
    call_sqlite3_db_handle, call_sqlite3_expanded_sql, call_sqlite3_free, call_sqlite3_sql,
};
use super::{STEP_RESULT_DONE, STEP_RESULT_FALLBACK};

pub(super) unsafe fn step_handle_cached_stmt(p_stmt: *mut sqlite3_stmt) -> c_int {
    let db = call_sqlite3_db_handle(p_stmt);
    let pg_conn = crate::pg_client::rust_pg_find_connection(db);

    if pg_conn.is_null() {
        return STEP_RESULT_FALLBACK;
    }
    let pg_conn_ref = &*pg_conn;
    if pg_conn_ref.is_pg_active == 0
        || pg_conn_ref.conn.is_null()
        || crate::db_interpose_helpers::rust_is_library_or_blobs_db_path(
            pg_conn_ref.db_path.as_ptr(),
        ) == 0
    {
        return STEP_RESULT_FALLBACK;
    }

    let expanded_sql = call_sqlite3_expanded_sql(p_stmt);
    let sql = if !expanded_sql.is_null() {
        expanded_sql as *const c_char
    } else {
        call_sqlite3_sql(p_stmt)
    };
    let orig_sql = call_sqlite3_sql(p_stmt);

    // SQLite passthrough (fts3_tokenizer, icu_load_collation, load_extension)
    // must not be routed to PG via the cached path. Fall through to orig_step.
    if !sql.is_null() {
        let sql_str = crate::db_interpose_helpers::cstr_to_str_or_empty(sql);
        if crate::pg_config::is_sqlite_passthrough_str(sql_str) {
            if !expanded_sql.is_null() {
                call_sqlite3_free(expanded_sql as *mut c_void);
            }
            return STEP_RESULT_FALLBACK;
        }
    }

    if !sql.is_null() && crate::pg_config::pg_config_should_skip_sql(sql) != 0 {
        free_expanded_sql(expanded_sql);
        return STEP_RESULT_DONE;
    }

    if !sql.is_null()
        && crate::pg_config::pg_config_is_write_operation(sql) != 0
        && crate::pg_config::pg_config_should_skip_sql(sql) == 0
        && crate::pg_config::pg_config_should_skip_sql(orig_sql) == 0
    {
        let rc = handle_cached_write(p_stmt, pg_conn, sql, expanded_sql);
        if rc != STEP_RESULT_FALLBACK {
            return rc;
        }
    }

    if !sql.is_null()
        && crate::pg_config::pg_config_is_read_operation(sql) != 0
        && crate::pg_config::pg_config_should_skip_sql(sql) == 0
    {
        let rc = handle_cached_read(p_stmt, pg_conn, sql, expanded_sql);
        if rc != STEP_RESULT_FALLBACK {
            return rc;
        }
    }

    free_expanded_sql(expanded_sql);
    STEP_RESULT_FALLBACK
}

pub(super) unsafe fn free_expanded_sql(expanded_sql: *mut c_char) {
    if !expanded_sql.is_null() {
        call_sqlite3_free(expanded_sql as *mut c_void);
    }
}
