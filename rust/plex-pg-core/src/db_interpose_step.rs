use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::byte_utils::contains_icase_bytes;
use crate::db_interpose_common::stderr_ptr;
use crate::db_interpose_common::tls_in_resolve_tables_ptr;
use crate::db_interpose_conn_utils::cstr_prefix;
use crate::ffi_types::{sqlite3, sqlite3_stmt, PgConnection, PgStmt};
use crate::log_debug_lazy;

mod cached;
mod retry;
mod support;

pub(crate) const SQLITE_DONE: c_int = 101;
pub(crate) const SQLITE_ROW: c_int = 100;
pub(crate) const SQLITE_ERROR: c_int = 1;

pub(crate) const STEP_RESULT_FALLBACK: c_int = -1;
pub(crate) const STEP_RESULT_DONE: c_int = SQLITE_DONE;
pub(crate) const STEP_RESULT_ROW: c_int = SQLITE_ROW;
pub(crate) const STEP_RESULT_ERROR: c_int = SQLITE_ERROR;

const LOADONE_STEP_TRACE_LIMIT: u32 = 5_000;
static LOADONE_STEP_TRACE_COUNT: AtomicU32 = AtomicU32::new(0);

use cached::step_handle_cached_stmt;
use retry::{maybe_retry_step, note_pg_conn_error};
use support::{
    call_sqlite3_db_handle, call_sqlite3_sql, orig_step, pg_exception_note_phase,
    shim_alloc_maybe_log,
};

fn set_stmt_translation_error(pg_stmt: *mut PgStmt, msg: &str) {
    unsafe {
        if pg_stmt.is_null() {
            return;
        }
        let stmt = &mut *pg_stmt;
        if stmt.conn.is_null() {
            return;
        }
        let conn = &mut *stmt.conn;
        conn.last_error_code = SQLITE_ERROR;
        conn.last_error.fill(0);
        let bytes = msg.as_bytes();
        let len = bytes.len().min(conn.last_error.len().saturating_sub(1));
        for (dst, src) in conn.last_error.iter_mut().zip(bytes.iter()).take(len) {
            *dst = *src as c_char;
        }
    }
}

use crate::env_utils::loadone_trace_enabled;

fn take_step_trace_slot() -> Option<u32> {
    if !loadone_trace_enabled() {
        return None;
    }

    let slot = LOADONE_STEP_TRACE_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if slot > LOADONE_STEP_TRACE_LIMIT {
        return None;
    }
    Some(slot)
}

unsafe fn trace_step_entry(
    slot: u32,
    p_stmt: *mut sqlite3_stmt,
    db: *mut sqlite3,
    registry_hit: bool,
    cache_hit: bool,
    sql: *const c_char,
) {
    let file = crate::db_interpose_open::lookup_db_handle_filename(db);
    let file_ptr = file
        .as_ref()
        .map(|s| s.as_ptr())
        .unwrap_or(b"<untracked>\0".as_ptr() as *const c_char);
    let handle_conn = crate::pg_client::rust_pg_find_handle_connection(db);
    let trace_sql = if sql.is_null() {
        b"<null>\0".as_ptr() as *const c_char
    } else {
        sql
    };

    let _ = libc::fprintf(
        stderr_ptr(),
        b"[LOADONE_TRACE][step] seq=%u stmt=%p db=%p file=%.900s registry=%d cache=%d handle_conn=%p sql=%.900s\n\0"
            .as_ptr() as *const c_char,
        slot,
        p_stmt as *mut c_void,
        db as *mut c_void,
        file_ptr,
        registry_hit as c_int,
        cache_hit as c_int,
        handle_conn as *mut c_void,
        trace_sql,
    );
    let _ = libc::fflush(stderr_ptr());
}

unsafe fn trace_step_null_select_stmt(
    p_stmt: *mut sqlite3_stmt,
    db: *mut sqlite3,
    sql: *const c_char,
) {
    if !loadone_trace_enabled() || sql.is_null() {
        return;
    }

    let sql_bytes = CStr::from_ptr(sql).to_bytes();
    if !contains_icase_bytes(sql_bytes, b"select") {
        return;
    }

    let registry_hit = crate::pg_statement::rust_stmt_find(p_stmt as usize) != 0;
    let cache_hit = crate::pg_statement::rust_cached_stmt_find(p_stmt as usize) != 0;
    let file = crate::db_interpose_open::lookup_db_handle_filename(db);
    let file_ptr = file
        .as_ref()
        .map(|s| s.as_ptr())
        .unwrap_or(b"<untracked>\0".as_ptr() as *const c_char);
    let handle_conn = crate::pg_client::rust_pg_find_handle_connection(db);
    let _ = libc::fprintf(
        stderr_ptr(),
        b"[LOADONE_TRACE][step] null_pg_stmt stmt=%p db=%p file=%.900s registry=%d cache=%d handle_conn=%p sql=%.900s\n\0"
            .as_ptr() as *const c_char,
        p_stmt as *mut c_void,
        db as *mut c_void,
        file_ptr,
        registry_hit as c_int,
        cache_hit as c_int,
        handle_conn as *mut c_void,
        sql,
    );
    let _ = libc::fflush(stderr_ptr());
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_step(p_stmt: *mut sqlite3_stmt) -> c_int {
    let dbg_stmt = crate::pg_statement::rust_stmt_find(p_stmt as usize) as *mut PgStmt;
    let mut dbg_sql: *const c_char = std::ptr::null();
    let dbg_db: *mut sqlite3;

    unsafe {
        if !dbg_stmt.is_null() {
            let s = &*dbg_stmt;
            dbg_sql = if !s.pg_sql.is_null() { s.pg_sql } else { s.sql };
        }
        if dbg_sql.is_null() {
            dbg_sql = call_sqlite3_sql(p_stmt);
        }
        dbg_db = call_sqlite3_db_handle(p_stmt);
        if let Some(slot) = take_step_trace_slot() {
            let cache_hit = crate::pg_statement::rust_cached_stmt_find(p_stmt as usize) != 0;
            trace_step_entry(
                slot,
                p_stmt,
                dbg_db,
                !dbg_stmt.is_null(),
                cache_hit,
                dbg_sql,
            );
        }
    }

    let phase = b"step\0";
    unsafe {
        pg_exception_note_phase(phase.as_ptr() as *const c_char, dbg_sql, p_stmt, dbg_db);
    }

    let rc = unsafe { my_sqlite3_step_impl(p_stmt) };

    let rc = maybe_retry_step(p_stmt, rc).unwrap_or(rc);
    if rc == SQLITE_ERROR {
        let handle = crate::pg_client::rust_pg_find_handle_connection(dbg_db);
        return crate::db_interpose_exec::pg_path::primary_error_code(handle);
    }
    rc
}

unsafe fn my_sqlite3_step_impl(p_stmt: *mut sqlite3_stmt) -> c_int {
    shim_alloc_maybe_log();

    if *tls_in_resolve_tables_ptr() != 0 {
        return orig_step(p_stmt);
    }

    let db = call_sqlite3_db_handle(p_stmt);
    let handle_conn = crate::pg_client::rust_pg_find_handle_connection(db);
    if !handle_conn.is_null() && (*handle_conn).is_pg_active != 0 {
        crate::pg_client::transaction::ensure_handle_session(handle_conn);
        let original_sql = call_sqlite3_sql(p_stmt);
        let sql = crate::db_interpose_helpers::cstr_to_str_or_empty(original_sql);
        if !crate::pg_config::is_sqlite_passthrough_str(sql)
            && !crate::pg_client::transaction::session_ready(handle_conn)
        {
            return SQLITE_ERROR;
        }
    }

    let pg_stmt = crate::pg_statement::rust_stmt_find(p_stmt as usize) as *mut PgStmt;

    if !pg_stmt.is_null() {
        let s = &*pg_stmt;
        s.in_step.store(1, Ordering::SeqCst);
    }

    if !pg_stmt.is_null() {
        let s = &*pg_stmt;
        let original_sql = crate::db_interpose_helpers::cstr_to_str_or_empty(s.sql);
        if crate::pg_client::transaction::is_transaction_sql(original_sql) {
            let _stmt_guard = PgStmt::lock_mutex(pg_stmt);
            if (*pg_stmt).write_executed != 0 {
                return SQLITE_DONE;
            }
            let db = call_sqlite3_db_handle(p_stmt);
            let conn = crate::pg_client::rust_pg_find_connection(db);
            let rc = crate::pg_client::transaction::execute_transaction(conn, original_sql)
                .unwrap_or(SQLITE_ERROR);
            if rc == 0 {
                (*pg_stmt).write_executed = 1;
                return SQLITE_DONE;
            }
            return rc;
        }
        if s.is_pg != 0 && s.is_pg != 3 && s.pg_sql.is_null() {
            let msg = format!(
                "PG step missing translated SQL: {}",
                cstr_prefix(s.sql, 220, "NULL")
            );
            set_stmt_translation_error(pg_stmt, &msg);
            crate::db_interpose_conn_utils::log_error(&msg);
            return SQLITE_ERROR;
        }

        if s.is_pg == 3 {
            log_debug_lazy!(
                "[RACE_DEBUG] STEP_END thread={:p} stmt={:p} rc={} reason=skip",
                libc::pthread_self() as *mut c_void,
                p_stmt,
                SQLITE_DONE
            );
            return SQLITE_DONE;
        }
    }

    if pg_stmt.is_null() {
        // SQLite passthrough (fts3_tokenizer, icu_load_collation, load_extension):
        // skip all PG routing and go directly to orig_step on real SQLite.
        let step_sql = call_sqlite3_sql(p_stmt);
        if !step_sql.is_null() {
            let sql_str = std::ffi::CStr::from_ptr(step_sql).to_str().unwrap_or("");
            if crate::pg_config::is_sqlite_passthrough_str(sql_str) {
                return orig_step(p_stmt);
            }
        }
        trace_step_null_select_stmt(p_stmt, call_sqlite3_db_handle(p_stmt), step_sql);
        let cached_rc = step_handle_cached_stmt(p_stmt);
        if cached_rc != STEP_RESULT_FALLBACK {
            return cached_rc;
        }
    }

    let mut exec_conn: *mut PgConnection = std::ptr::null_mut();

    if !pg_stmt.is_null() {
        let s = &*pg_stmt;
        if !s.shadow_stmt.is_null() {
            let db = call_sqlite3_db_handle(s.shadow_stmt);
            let handle_conn = crate::pg_client::rust_pg_find_connection(db);
            if !handle_conn.is_null() {
                let hc = &*handle_conn;
                if hc.is_pg_active != 0
                    && crate::db_interpose_helpers::rust_is_library_or_blobs_db_path(
                        hc.db_path.as_ptr(),
                    ) != 0
                {
                    if !hc.conn.is_null() {
                        exec_conn = handle_conn;
                    }
                }
            }
        }
    }

    if !pg_stmt.is_null() && !exec_conn.is_null() {
        let s = &*pg_stmt;
        if !s.pg_sql.is_null() && !(&*exec_conn).conn.is_null() {
            let (param_values, stmt_is_pg, read_done, has_cached_result, write_executed) = {
                let _stmt_guard = PgStmt::lock_mutex(pg_stmt);
                let max_params = s.param_count.max(0) as usize;
                let max_params = max_params.min(s.param_values.len());
                let mut pv: Vec<*const c_char> = vec![std::ptr::null(); max_params];
                for i in 0..max_params {
                    pv[i] = s.param_values[i] as *const c_char;
                }
                (
                    pv,
                    s.is_pg,
                    s.read_done != 0,
                    !s.cached_result.is_null(),
                    s.write_executed != 0,
                )
            };

            if stmt_is_pg == 2 {
                if (*pg_stmt).read_done < 0 {
                    return crate::db_interpose_exec::pg_path::primary_error_code(s.conn);
                }
                if read_done {
                    return SQLITE_DONE;
                }

                if has_cached_result {
                    return crate::db_interpose_step_read_utils::rust_step_read_advance_cached_result(
                        pg_stmt,
                    );
                }

                crate::db_interpose_step_read_utils::rust_step_read_log_debug_context(
                    pg_stmt, exec_conn,
                );
                crate::db_interpose_step_read_utils::rust_step_read_prepare_reexecution_state(
                    pg_stmt, exec_conn,
                );

                let (read_done, has_cached_result, streaming_mode, has_result) = {
                    let _stmt_guard = PgStmt::lock_mutex(pg_stmt);
                    (
                        s.read_done != 0,
                        !s.cached_result.is_null(),
                        s.streaming_mode != 0,
                        !s.result.is_null(),
                    )
                };

                if read_done {
                    return if (*pg_stmt).read_done < 0 {
                        crate::db_interpose_exec::pg_path::primary_error_code(s.conn)
                    } else {
                        SQLITE_DONE
                    };
                }

                if has_cached_result {
                    return crate::db_interpose_step_read_utils::rust_step_read_advance_cached_result(
                        pg_stmt,
                    );
                }

                if streaming_mode {
                    return crate::db_interpose_step_read_utils::rust_step_read_streaming_next(
                        p_stmt, pg_stmt,
                    );
                }

                if has_result {
                    return crate::db_interpose_step_read_utils::rust_step_read_eager_next(pg_stmt);
                }

                let mut conn_error = 0;
                let first_rc = crate::db_interpose_step_read_utils::rust_step_read_first_execute(
                    pg_stmt,
                    &mut exec_conn,
                    param_values.as_ptr(),
                    &mut conn_error,
                );
                if first_rc == STEP_RESULT_ERROR && conn_error != 0 {
                    note_pg_conn_error();
                }
                return first_rc;
            } else if stmt_is_pg == 1 {
                if write_executed {
                    return if (*pg_stmt).write_executed < 0 {
                        SQLITE_ERROR
                    } else {
                        SQLITE_DONE
                    };
                }

                crate::db_interpose_step_write_utils::rust_step_write_log_debug_context(
                    pg_stmt,
                    exec_conn,
                    param_values.as_ptr(),
                );

                if crate::db_interpose_step_write_utils::rust_step_write_should_skip_special_insert(
                    pg_stmt,
                    exec_conn,
                    param_values.as_ptr(),
                ) != 0
                {
                    return SQLITE_DONE;
                }

                let mut prep_conn_error = 0;
                let prep_rc =
                    crate::db_interpose_step_write_utils::rust_step_write_prepare_connection(
                        pg_stmt,
                        &mut exec_conn,
                        &mut prep_conn_error,
                    );
                if prep_rc == STEP_RESULT_ERROR {
                    if prep_conn_error != 0 {
                        note_pg_conn_error();
                    }
                    return SQLITE_ERROR;
                }

                let mut write_conn_error = 0;
                let write_rc =
                    crate::db_interpose_step_write_utils::rust_step_write_execute_and_finalize(
                        pg_stmt,
                        exec_conn,
                        param_values.as_ptr(),
                        &mut write_conn_error,
                    );
                if write_rc == STEP_RESULT_ERROR {
                    if write_conn_error != 0 {
                        note_pg_conn_error();
                    }
                    return SQLITE_ERROR;
                }
            }
        }
    }

    if !pg_stmt.is_null() {
        let s = &*pg_stmt;
        if s.is_pg != 0 {
            if s.is_pg == 1 {
                return SQLITE_DONE;
            }
            crate::db_interpose_step_write_utils::rust_step_log_step_exit_trace(pg_stmt);
        }
    }

    let rc = orig_step(p_stmt);
    if rc != SQLITE_ROW && rc != SQLITE_DONE {
        let db = call_sqlite3_db_handle(p_stmt);
        let errmsg = if db.is_null() {
            b"no db handle\0".as_ptr() as *const c_char
        } else {
            crate::db_interpose_metadata::rust_my_sqlite3_errmsg(db)
        };
        let sql = call_sqlite3_sql(p_stmt);
        libc::fprintf(
            stderr_ptr(),
            b"[SQLITE_STEP_ERROR] rc=%d errmsg='%s' sql='%.900s'\n\0".as_ptr() as *const c_char,
            rc,
            errmsg,
            if sql.is_null() {
                b"<null>\0".as_ptr() as *const c_char
            } else {
                sql
            },
        );
        libc::fflush(stderr_ptr());
    }
    rc
}
