use super::*;

mod connection;
mod eager_fetch;
mod prepared_send;
mod streaming_fetch;

use connection::{acquire_exec_connection, ensure_connection_ready, lock_exec_connection};
use eager_fetch::eager_fetch_result;
use prepared_send::{disable_streaming_env, send_query_for_read};
use streaming_fetch::streaming_fetch_result;

pub(super) unsafe fn set_pg_conn_error(pg_conn_error_out: *mut c_int) {
    if !pg_conn_error_out.is_null() {
        *pg_conn_error_out = 1;
    }
}

pub(super) fn first_execute_impl(
    pg_stmt: *mut PgStmt,
    exec_conn_io: *mut *mut PgConnection,
    param_values: *const *const c_char,
    pg_conn_error_out: *mut c_int,
) -> c_int {
    unsafe {
        if !pg_conn_error_out.is_null() {
            *pg_conn_error_out = 0;
        }
        if pg_stmt.is_null() || exec_conn_io.is_null() {
            return STEP_RESULT_ERROR;
        }
        let mut stmt_guard = Some(PgStmt::lock_mutex(pg_stmt));
        (&mut *pg_stmt).executing_thread = libc::pthread_self();

        let mut exec_conn = match acquire_exec_connection(
            pg_stmt,
            exec_conn_io,
            &mut stmt_guard,
            pg_conn_error_out,
        ) {
            Ok(exec_conn) => exec_conn,
            Err(rc) => return rc,
        };

        let mut conn_guard =
            match lock_exec_connection(pg_stmt, &mut exec_conn, &mut stmt_guard, pg_conn_error_out) {
                Ok(conn_guard) => conn_guard,
                Err(rc) => return rc,
            };

        (&mut *pg_stmt).conn = exec_conn;
        if let Err(rc) = ensure_connection_ready(
            pg_stmt,
            exec_conn,
            &mut stmt_guard,
            &mut conn_guard,
            pg_conn_error_out,
        ) {
            return rc;
        }

        if let Err(rc) = send_query_for_read(
            pg_stmt,
            exec_conn,
            param_values,
            &mut stmt_guard,
            &mut conn_guard,
            pg_conn_error_out,
        ) {
            return rc;
        }

        let disable_streaming = disable_streaming_env();
        let thread_has_other_streaming =
            crate::pg_client::current_thread_has_other_streaming_connection(
                exec_conn as *const c_void,
            );
        let mut use_streaming =
            should_use_streaming(pg_stmt, disable_streaming) && !thread_has_other_streaming;
        if disable_streaming {
            log_info("STREAM: disabled via PLEX_PG_DISABLE_STREAMING, using eager fetch");
        } else if thread_has_other_streaming {
            log_debug(
                "STREAM: disabled because current thread already owns another active streaming connection, using eager fetch",
            );
        } else if !use_streaming {
            log_debug(
                "STREAM: disabled for stmt after prior cross-thread requery, using eager fetch",
            );
        }

        if use_streaming {
            if crate::libpq_helpers::rust_pq_set_single_row_mode((&*exec_conn).conn) == 0 {
                log_error("PQsetSingleRowMode failed, falling back to eager fetch");
                use_streaming = false;
            }
        }

        if use_streaming {
            return streaming_fetch_result(
                pg_stmt,
                exec_conn_io,
                exec_conn,
                &mut stmt_guard,
                &mut conn_guard,
                pg_conn_error_out,
            );
        }

        eager_fetch_result(
            pg_stmt,
            exec_conn_io,
            exec_conn,
            &mut stmt_guard,
            &mut conn_guard,
        )
    }
}
