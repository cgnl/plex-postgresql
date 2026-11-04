//! Free a handle session that is busy streaming a result.
//!
//! SQLite lets one connection keep several statements open at once; Plex relies
//! on that, e.g. while emptying the trash it iterates the relations of an item
//! and looks up each related media item before the outer cursor is finished.
//! Inside a transaction every statement must run on the handle's single
//! PostgreSQL session, and a libpq connection can only stream one result. The
//! nested statement used to fail with "cannot switch an active handle session
//! while a result is streaming", aborting Plex's transaction (issue #61).
//!
//! Instead, the remaining rows of the streaming statement are read into one
//! compact result (the same rows it would have fetched one by one) and the
//! session is handed to the nested statement.

use std::cell::RefCell;
use std::ffi::CString;
use std::os::raw::c_int;
use std::sync::atomic::Ordering;

use super::{rust_stmt_ref, rust_stmt_unref, REGISTRY};
use crate::db_interpose_conn_utils::log_info;
use crate::ffi_types::{PgConnection, PgStmt};
use crate::libpq_helpers::{self, PGresult, PGRES_TUPLES_OK, PG_COPYRES_ATTRS};

const PGRES_SINGLE_TUPLE: c_int = 9;

thread_local! {
    /// Owners pinned by a read-ahead. Released at the next statement step,
    /// when this thread holds no connection mutex: dropping the last reference
    /// frees the statement, and freeing must never happen under that mutex.
    static DEFERRED_UNREF: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// Release references pinned by earlier read-aheads. Call with no locks held.
pub(crate) fn release_deferred_spill_refs() {
    let pending = DEFERRED_UNREF.with(|list| {
        let mut list = list.borrow_mut();
        if list.is_empty() {
            Vec::new()
        } else {
            std::mem::take(&mut *list)
        }
    });
    for owner in pending {
        rust_stmt_unref(owner as *mut PgStmt);
    }
}

/// Read the rest of the result streaming on `conn` into its owning statement
/// so the session can serve another statement.
///
/// # Safety
/// The caller holds `conn`'s mutex and `conn` is a live pool/handle connection.
pub(crate) unsafe fn spill_live_stream_locked(
    conn: *mut PgConnection,
    requester: *mut PgStmt,
) -> bool {
    take_live_stream_locked(conn, requester, false)
}

/// ROLLBACK while a result streams: as in SQLite, the pending statement is
/// aborted (its next step fails) and the rollback proceeds. The stream is
/// cancelled rather than read, since its rows are discarded anyway.
///
/// # Safety
/// As for `spill_live_stream_locked`.
pub(crate) unsafe fn abort_live_stream_locked(conn: *mut PgConnection) -> bool {
    take_live_stream_locked(conn, std::ptr::null_mut(), true)
}

unsafe fn take_live_stream_locked(
    conn: *mut PgConnection,
    requester: *mut PgStmt,
    abort: bool,
) -> bool {
    if conn.is_null() || (*conn).conn.is_null() {
        return false;
    }
    let generation = (*conn).stream_generation.load(Ordering::SeqCst);
    // Find and pin the owner under the registry read lock only; the read-ahead
    // itself runs without it so prepare/finalize on other threads never wait
    // for a long result to drain.
    let owner = {
        let Ok(registry) = REGISTRY.read() else {
            return false;
        };
        let mut found = std::ptr::null_mut();
        for &value in registry.forward.values() {
            let candidate = value as *mut PgStmt;
            if candidate.is_null() || candidate == requester {
                continue;
            }
            // Statements being stepped on another thread hold their lock and
            // are left alone.
            let Ok(_probe) = (*candidate).mutex.try_lock() else {
                continue;
            };
            if is_live_owner(&*candidate, conn, generation) {
                found = candidate;
                break;
            }
        }
        if !found.is_null() {
            rust_stmt_ref(found);
        }
        found
    };
    if owner.is_null() {
        return false;
    }
    DEFERRED_UNREF.with(|list| list.borrow_mut().push(owner as usize));

    let Ok(_owner_guard) = (*owner).mutex.try_lock() else {
        return false;
    };
    let s = &mut *owner;
    if !is_live_owner(s, conn, generation) {
        return false;
    }
    if abort {
        abort_stream(s, conn);
    } else {
        read_ahead(s, conn);
    }
    true
}

unsafe fn abort_stream(s: &mut PgStmt, conn: *mut PgConnection) {
    let pq = (*conn).conn;
    let cancel = libpq_helpers::rust_pq_get_cancel(pq);
    if !cancel.is_null() {
        let mut errbuf = [0 as std::os::raw::c_char; 256];
        libpq_helpers::rust_pq_cancel(cancel, errbuf.as_mut_ptr(), errbuf.len() as c_int);
        libpq_helpers::rust_pq_free_cancel(cancel);
    }
    discard_stream(pq);
    s.spilled_result = std::ptr::null_mut();
    s.spill_installed = false;
    s.spill_state = -1;
    s.spill_error = Some(CString::new("abort due to ROLLBACK").unwrap_or_default());
    s.streaming_conn = std::ptr::null_mut();
    (*conn).streaming_active.store(0, Ordering::SeqCst);
    log_info("STREAM: aborted a streaming statement for ROLLBACK");
}

fn is_live_owner(s: &PgStmt, conn: *mut PgConnection, generation: u64) -> bool {
    s.streaming_mode != 0
        && s.spill_state == 0
        && s.streaming_conn == conn
        && s.stream_generation == generation
}

/// Drain the stream into one compact result. Caller holds both locks.
unsafe fn read_ahead(s: &mut PgStmt, conn: *mut PgConnection) {
    let pq = (*conn).conn;
    let mut merged: *mut PGresult = std::ptr::null_mut();
    let mut rows: c_int = 0;
    let mut error: Option<String> = None;
    loop {
        let res = libpq_helpers::rust_pq_get_result(pq);
        if res.is_null() {
            error = Some("PostgreSQL stream ended without a completion result".into());
            break;
        }
        match libpq_helpers::rust_pq_result_status(res) {
            PGRES_SINGLE_TUPLE => {
                if merged.is_null() {
                    merged = libpq_helpers::pq_copy_result(res, PG_COPYRES_ATTRS);
                }
                if merged.is_null() || !append_row(merged, rows, res) {
                    libpq_helpers::rust_pq_clear(res);
                    error = Some("Out of memory while reading a PostgreSQL stream ahead".into());
                    // Keep consuming so the session ends up idle.
                    discard_stream(pq);
                    break;
                }
                rows += 1;
                libpq_helpers::rust_pq_clear(res);
            }
            PGRES_TUPLES_OK => {
                libpq_helpers::rust_pq_clear(res);
                break;
            }
            _ => {
                let message = libpq_helpers::rust_pq_result_error_message(res);
                error = Some(if message.is_null() {
                    "PostgreSQL stream failed".into()
                } else {
                    std::ffi::CStr::from_ptr(message).to_string_lossy().trim().to_string()
                });
                libpq_helpers::rust_pq_clear(res);
                break;
            }
        }
    }
    discard_stream(pq);

    s.spilled_result = merged;
    s.spill_installed = false;
    s.spill_state = if error.is_some() { -1 } else { 1 };
    s.spill_error = error.map(|e| CString::new(e.replace('\0', " ")).unwrap_or_default());
    s.streaming_conn = std::ptr::null_mut();
    (*conn).streaming_active.store(0, Ordering::SeqCst);
    log_info(&format!(
        "STREAM: read ahead {} remaining row(s) so a nested statement can use the session",
        rows
    ));
}

/// Copy every field of the single-row result `row` into tuple `tuple` of `merged`.
unsafe fn append_row(merged: *mut PGresult, tuple: c_int, row: *const PGresult) -> bool {
    let fields = libpq_helpers::rust_pq_nfields(row);
    for field in 0..fields {
        let ok = if libpq_helpers::pq_get_is_null(row, 0, field) {
            libpq_helpers::pq_set_value(merged, tuple, field, std::ptr::null(), -1)
        } else {
            libpq_helpers::pq_set_value(
                merged,
                tuple,
                field,
                libpq_helpers::pq_get_value(row, 0, field),
                libpq_helpers::pq_get_length(row, 0, field),
            )
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Consume any remaining results so the connection is idle again.
unsafe fn discard_stream(pq: *mut libpq_helpers::PGconn) {
    let mut tail = libpq_helpers::rust_pq_get_result(pq);
    while !tail.is_null() {
        libpq_helpers::rust_pq_clear(tail);
        tail = libpq_helpers::rust_pq_get_result(pq);
    }
}
