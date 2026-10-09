use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::Ordering;

use crate::db_interpose_conn_utils::{log_debug, log_error, PthreadMutexGuard};
use crate::db_interpose_helpers::cstr_to_str_or_empty;
use crate::ffi_types::PgStmt;
use crate::log_debug_lazy;
use crate::log_info_lazy;

const PMT_STMT_SWEEP_EXTRA_FREE: i32 = 6;

unsafe fn clear_streaming_state(stmt_ptr: *mut PgStmt, stmt: &mut PgStmt, op_name: &str) {
    if stmt.streaming_mode == 0 || stmt.streaming_conn.is_null() {
        return;
    }

    if crate::pg_client::rust_pool_is_tracked_connection(stmt.streaming_conn.cast()) == 0 {
        log_error(&format!(
            "{}: streaming_conn invalid, skipping cancel/drain (stmt={:p})",
            op_name, stmt_ptr
        ));
        stmt.streaming_mode = 0;
        stmt.streaming_conn = std::ptr::null_mut();
        return;
    }

    // SAFETY: streaming_conn is non-null (checked above) and validated by
    // pg_pool_validate_connection.
    let sconn = &mut *stmt.streaming_conn;
    let _conn_guard = PthreadMutexGuard::lock(&mut sconn.mutex as *mut _);
    if !sconn.conn.is_null() {
        let cancel = crate::libpq_helpers::rust_pq_get_cancel(sconn.conn);
        if !cancel.is_null() {
            let mut errbuf = [0 as c_char; 256];
            if crate::libpq_helpers::rust_pq_cancel(
                cancel,
                errbuf.as_mut_ptr(),
                errbuf.len() as c_int,
            ) == 0
            {
                let err = cstr_to_str_or_empty(errbuf.as_ptr());
                log_error(&format!("{}: PQcancel failed: {}", op_name, err));
            }
            crate::libpq_helpers::rust_pq_free_cancel(cancel);
        }
        let mut drain_count = 0;
        loop {
            let drain = crate::libpq_helpers::rust_pq_get_result(sconn.conn);
            if drain.is_null() {
                break;
            }
            drain_count += 1;
            crate::libpq_helpers::rust_pq_clear(drain);
            if drain_count > 1000 {
                log_info_lazy!(
                    "{}: drain after cancel exceeded 1000 on {:p}",
                    op_name,
                    stmt.streaming_conn
                );
                break;
            }
        }
        if drain_count > 0 {
            if op_name == "pg_stmt_clear_result" {
                let sql = if stmt.sql.is_null() {
                    "NULL"
                } else {
                    cstr_to_str_or_empty(stmt.sql)
                };
                log_debug_lazy!(
                    "{}: drained {} results after cancel (sql={:.60})",
                    op_name,
                    drain_count,
                    sql
                );
            } else {
                log_debug_lazy!("{}: drained {} results after cancel", op_name, drain_count);
            }
        }
    }

    stmt.streaming_mode = 0;
    sconn.streaming_active.store(0, Ordering::Release);
    stmt.streaming_conn = std::ptr::null_mut();
}

unsafe fn safe_param_count(stmt: &PgStmt) -> usize {
    let mut count = stmt.param_count;
    if count < 0 {
        count = 0;
    }
    let cap = stmt.param_values.len();
    if count as usize > cap {
        count = cap as c_int;
    }
    count as usize
}

unsafe fn free_col_names(stmt: &mut PgStmt) {
    if stmt.col_names.is_null() {
        return;
    }
    let count = if stmt.num_col_names > 0 {
        stmt.num_col_names as usize
    } else {
        0
    };
    for i in 0..count {
        let slot = stmt.col_names.add(i);
        if !(*slot).is_null() {
            libc::free(*slot as *mut c_void);
        }
    }
    libc::free(stmt.col_names as *mut c_void);
    stmt.col_names = std::ptr::null_mut();
    stmt.num_col_names = 0;
}

pub fn rust_stmt_free(stmt_ptr: *mut PgStmt) {
    if stmt_ptr.is_null() {
        return;
    }
    unsafe {
        let stmt = &mut *stmt_ptr;

        let ref_count = stmt.ref_count.load(Ordering::Acquire);
        if ref_count != 0 {
            let sql = if stmt.sql.is_null() {
                "NULL"
            } else {
                cstr_to_str_or_empty(stmt.sql)
            };
            log_error(&format!(
                "pg_stmt_free: WARNING ref_count={} (expected 0) for stmt={:p} sql={:.50}",
                ref_count, stmt_ptr, sql
            ));
            if ref_count > 0 {
                log_error(&format!(
                    "pg_stmt_free: ABORT - ref_count={} not freeing to prevent use-after-free",
                    ref_count
                ));
                return;
            }
        }

        clear_streaming_state(stmt_ptr, stmt, "pg_stmt_free");

        log_debug_lazy!(
            "pg_stmt_free: START stmt={:p} sql={:p} pg_sql={:p}",
            stmt_ptr,
            stmt.sql,
            stmt.pg_sql
        );

        let pg_sql_is_separate = !stmt.pg_sql.is_null() && stmt.pg_sql != stmt.sql;

        if !stmt.sql.is_null() {
            let sql = if stmt.sql.is_null() {
                "NULL"
            } else {
                cstr_to_str_or_empty(stmt.sql)
            };
            log_debug_lazy!("pg_stmt_free: freeing sql={:p} ({:.50})", stmt.sql, sql);
            libc::free(stmt.sql as *mut c_void);
            stmt.sql = std::ptr::null_mut();
        }
        if pg_sql_is_separate && !stmt.pg_sql.is_null() {
            let sql = cstr_to_str_or_empty(stmt.pg_sql);
            log_debug_lazy!(
                "pg_stmt_free: freeing pg_sql={:p} ({:.50})",
                stmt.pg_sql,
                sql
            );
            libc::free(stmt.pg_sql as *mut c_void);
            stmt.pg_sql = std::ptr::null_mut();
        }
        if !stmt.result.is_null() {
            log_debug_lazy!("pg_stmt_free: PQclear result={:p}", stmt.result);
            crate::libpq_helpers::rust_pq_clear(stmt.result);
            stmt.result = std::ptr::null_mut();
        }
        if !stmt.cached_result.is_null() {
            crate::pg_query_cache::rust_query_cache_release(stmt.cached_result);
            stmt.cached_result = std::ptr::null_mut();
        }

        let safe_param_count = safe_param_count(stmt);

        for i in 0..stmt.param_values.len() {
            let val = stmt.param_values[i];
            if !val.is_null() && !stmt.is_preallocated_buffer(i) {
                log_debug_lazy!("pg_stmt_free: freeing param_values[{}]={:p}", i, val);
                libc::free(val as *mut c_void);
                stmt.param_values[i] = std::ptr::null_mut();
                if i >= safe_param_count
                    && crate::pg_mem_telemetry::rust_mem_telemetry_enabled() != 0
                {
                    crate::pg_mem_telemetry::rust_mem_telemetry_add(
                        PMT_STMT_SWEEP_EXTRA_FREE,
                        0,
                        1,
                    );
                }
            }
        }

        if !stmt.param_names.is_null() {
            log_debug_lazy!(
                "pg_stmt_free: freeing param_names={:p} (array of {})",
                stmt.param_names,
                safe_param_count
            );
            for i in 0..safe_param_count {
                let slot = stmt.param_names.add(i);
                if !(*slot).is_null() {
                    let name = cstr_to_str_or_empty(*slot);
                    log_debug_lazy!(
                        "pg_stmt_free: freeing param_names[{}]={:p} ({:.30})",
                        i,
                        *slot,
                        name
                    );
                    libc::free(*slot as *mut c_void);
                    *slot = std::ptr::null_mut();
                }
            }
            log_debug_lazy!(
                "pg_stmt_free: freeing param_names array at {:p}",
                stmt.param_names
            );
            libc::free(stmt.param_names as *mut c_void);
            stmt.param_names = std::ptr::null_mut();
        }

        for i in 0..stmt.decoded_blobs.len() {
            let blob = stmt.decoded_blobs[i];
            if !blob.is_null() {
                log_debug_lazy!("pg_stmt_free: freeing decoded_blobs[{}]={:p}", i, blob);
                libc::free(blob as *mut c_void);
                stmt.decoded_blobs[i] = std::ptr::null_mut();
            }
        }

        for i in 0..stmt.cached_text.len() {
            let text = stmt.cached_text[i];
            if !text.is_null() {
                log_debug_lazy!("pg_stmt_free: freeing cached_text[{}]={:p}", i, text);
                libc::free(text as *mut c_void);
                stmt.cached_text[i] = std::ptr::null_mut();
            }
        }
        for i in 0..stmt.cached_blob.len() {
            let blob = stmt.cached_blob[i];
            if !blob.is_null() {
                log_debug_lazy!("pg_stmt_free: freeing cached_blob[{}]={:p}", i, blob);
                libc::free(blob as *mut c_void);
                stmt.cached_blob[i] = std::ptr::null_mut();
            }
        }

        for i in 0..stmt.col_table_names.len() {
            let name = stmt.col_table_names[i];
            if !name.is_null() {
                libc::free(name as *mut c_void);
                stmt.col_table_names[i] = std::ptr::null_mut();
            }
        }

        free_col_names(stmt);

        log_debug_lazy!(
            "pg_stmt_free: destroying mutex and freeing stmt={:p}",
            stmt_ptr
        );
        // std::sync::Mutex is cleaned up automatically by drop(Box::from_raw) below.
        drop(Box::from_raw(stmt_ptr));
        log_debug("pg_stmt_free: DONE");
    }
}

pub fn rust_stmt_clear_result(stmt_ptr: *mut PgStmt) {
    if stmt_ptr.is_null() {
        return;
    }
    unsafe {
        let stmt = &mut *stmt_ptr;

        clear_streaming_state(stmt_ptr, stmt, "pg_stmt_clear_result");

        if !stmt.result.is_null() {
            crate::libpq_helpers::rust_pq_clear(stmt.result);
            stmt.result = std::ptr::null_mut();
        }
        if !stmt.cached_result.is_null() {
            crate::pg_query_cache::rust_query_cache_release(stmt.cached_result);
            stmt.cached_result = std::ptr::null_mut();
        }
        stmt.result_conn = std::ptr::null_mut();
        stmt.metadata_only_result = 0;
        stmt.current_row = -1;
        stmt.num_rows = 0;
        stmt.num_cols = 0;
        stmt.write_executed = 0;
        stmt.read_done = 0;

        for i in 0..stmt.decoded_blobs.len() {
            let blob = stmt.decoded_blobs[i];
            if !blob.is_null() {
                libc::free(blob as *mut c_void);
                stmt.decoded_blobs[i] = std::ptr::null_mut();
                stmt.decoded_blob_lens[i] = 0;
            }
        }
        stmt.decoded_blob_row = -1;

        for i in 0..stmt.cached_text.len() {
            let text = stmt.cached_text[i];
            if !text.is_null() {
                libc::free(text as *mut c_void);
                stmt.cached_text[i] = std::ptr::null_mut();
            }
        }
        for i in 0..stmt.cached_blob.len() {
            let blob = stmt.cached_blob[i];
            if !blob.is_null() {
                libc::free(blob as *mut c_void);
                stmt.cached_blob[i] = std::ptr::null_mut();
                stmt.cached_blob_len[i] = 0;
            }
        }
        stmt.cached_row = -1;

        free_col_names(stmt);
    }
}
