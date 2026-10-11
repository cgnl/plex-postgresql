use std::os::raw::{c_char, c_int};

use crate::byte_utils::{cstr_bytes, starts_with_icase_bytes};
use crate::db_interpose_conn_utils::PthreadMutexGuard;
use crate::ffi_types::PgConnection;

const PQTRANS_IDLE: i32 = 0;
const PQTRANS_INTRANS: i32 = 2;
const PQTRANS_INERROR: i32 = 3;

static EMPTY: &[u8] = b"\0";

unsafe fn skip_leading_sql_noise_ptr(sql: *const c_char) -> *const c_char {
    if sql.is_null() {
        return EMPTY.as_ptr() as *const c_char;
    }

    let mut p = sql.cast::<u8>();

    loop {
        // Skip whitespace
        loop {
            let b = *p;
            if b == 0 {
                return p as *const c_char;
            }
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                p = p.add(1);
                continue;
            }
            break;
        }

        let b0 = *p;
        if b0 == 0 {
            return p as *const c_char;
        }
        let b1 = *p.add(1);

        // Line comment --
        if b0 == b'-' && b1 == b'-' {
            p = p.add(2);
            while *p != 0 && *p != b'\n' {
                p = p.add(1);
            }
            continue;
        }

        // Block comment /* ... */
        if b0 == b'/' && b1 == b'*' {
            p = p.add(2);
            while *p != 0 {
                if *p == b'*' && *p.add(1) == b'/' {
                    p = p.add(2);
                    break;
                }
                p = p.add(1);
            }
            continue;
        }

        break;
    }

    p as *const c_char
}

#[no_mangle]
pub extern "C" fn rust_skip_leading_sql_noise(sql: *const c_char) -> *const c_char {
    unsafe { skip_leading_sql_noise_ptr(sql) }
}

#[no_mangle]
pub extern "C" fn rust_is_txn_terminator_sql(sql: *const c_char) -> c_int {
    let s = unsafe { skip_leading_sql_noise_ptr(sql) };
    let bytes = unsafe { cstr_bytes(s) };
    let is_term = starts_with_icase_bytes(bytes, b"commit")
        || starts_with_icase_bytes(bytes, b"rollback")
        || starts_with_icase_bytes(bytes, b"end");
    if is_term {
        1
    } else {
        0
    }
}

#[no_mangle]
pub extern "C" fn rust_txn_terminator_should_noop(
    conn: *mut PgConnection,
    sql: *const c_char,
    txn_state_out: *mut c_int,
) -> c_int {
    unsafe {
        if !txn_state_out.is_null() {
            *txn_state_out = PQTRANS_IDLE;
        }
        if conn.is_null() || (&*conn).conn.is_null() || rust_is_txn_terminator_sql(sql) == 0 {
            return 0;
        }

        let c = &mut *conn;
        let mut txn_state = PQTRANS_IDLE;
        let _guard = PthreadMutexGuard::lock(&mut c.mutex as *mut _);
        if !c.conn.is_null() {
            txn_state = crate::libpq_helpers::rust_pq_transaction_status(c.conn);
        }

        if !txn_state_out.is_null() {
            *txn_state_out = txn_state;
        }

        if txn_state != PQTRANS_INTRANS && txn_state != PQTRANS_INERROR {
            1
        } else {
            0
        }
    }
}
