use super::*;
use crate::log_debug_lazy;
use crate::log_info_lazy;

pub(super) fn trace_prepare_sql_ok(sql: *const c_char) -> bool {
    crate::db_interpose_helpers::rust_trace_prepare_sql_ok(sql) != 0
}

pub(super) fn trace_prepare_pgsql_if_enabled(sqlite_sql: *const c_char, pg_sql: *const c_char) {
    if !trace_prepare_sql_ok(sqlite_sql) {
        return;
    }
    if pg_sql.is_null() {
        return;
    }
    log_debug_lazy!("TRACE_PREPARE_PGSQL: {}", cstr_prefix(pg_sql, 900, ""));
}

pub(super) fn prepared_statements_disabled() -> bool {
    let cached = DISABLE_PREPARED_CACHED.load(Ordering::Relaxed);
    if cached != -1 {
        return cached == 1;
    }
    let name = b"PLEX_PG_DISABLE_PREPARED\0";
    let val = unsafe {
        let env = libc::getenv(name.as_ptr() as *const c_char);
        crate::db_interpose_helpers::rust_env_truthy(env)
    };
    let flag = if val != 0 { 1 } else { 0 };
    DISABLE_PREPARED_CACHED.store(flag, Ordering::Relaxed);
    flag == 1
}

pub(super) fn is_txn_control_sql(sql: *const c_char) -> bool {
    if sql.is_null() {
        return false;
    }
    let bytes = unsafe { CStr::from_ptr(sql).to_bytes() };
    let mut i = 0usize;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    let rest = &bytes[i..];
    starts_with_ascii_icase(rest, b"begin")
        || starts_with_ascii_icase(rest, b"commit")
        || starts_with_ascii_icase(rest, b"rollback")
        || starts_with_ascii_icase(rest, b"savepoint")
        || starts_with_ascii_icase(rest, b"release savepoint")
}

pub(super) fn should_bypass_worker_delegation(sql: *const c_char) -> bool {
    crate::pg_config::pg_config_should_skip_sql(sql) != 0
        || contains_icase_ptr(sql, "sqlite_master")
        || contains_icase_ptr(sql, "sqlite_schema")
        || contains_icase_ptr(sql, "locations_node")
        || contains_icase_ptr(sql, "locations_parent")
        || contains_icase_ptr(sql, "locations_rowid")
}

pub(super) fn detect_query_loop(sql: *const c_char) -> bool {
    if sql.is_null() {
        return false;
    }
    let s = match unsafe { CStr::from_ptr(sql).to_str() } {
        Ok(s) => s,
        Err(_) => return false,
    };
    if let Some((count, elapsed_ms)) =
        crate::db_interpose_prepare_helpers::prepare_query_loop_tick(s)
    {
        QUERY_LOOP_LOG_COUNTER.with(|c| {
            let cur = c.get();
            if cur % 10 == 0 {
                log_info_lazy!(
                    "High-frequency query: {} calls in {} ms (likely batch operation with different params) sql={}",
                    count,
                    elapsed_ms,
                    cstr_prefix(sql, 200, "NULL")
                );
            }
            c.set(cur.wrapping_add(1));
        });
    }
    false
}
