use rusqlite::ffi;
use rusqlite::{Connection, Result};
use std::ffi::CStr;
use std::ffi::CString;
use std::ptr;

/// Helper: prepare via FFI and return raw stmt pointer (caller must finalize).
unsafe fn prepare_raw(conn: &Connection, sql: &str) -> *mut ffi::sqlite3_stmt {
    let c_sql = CString::new(sql).unwrap();
    let mut raw: *mut ffi::sqlite3_stmt = ptr::null_mut();
    let mut tail: *const std::os::raw::c_char = ptr::null();
    let rc = ffi::sqlite3_prepare_v2(conn.handle(), c_sql.as_ptr(), -1, &mut raw, &mut tail);
    assert_eq!(rc, ffi::SQLITE_OK);
    raw
}

#[test]
fn expanded_sql_no_params() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    let raw = unsafe { prepare_raw(&conn, "SELECT 1, 2, 3") };
    let expanded = unsafe { ffi::sqlite3_expanded_sql(raw) };
    assert!(!expanded.is_null());
    let sql = unsafe { CStr::from_ptr(expanded) }.to_string_lossy();
    assert!(sql.to_ascii_lowercase().contains("select"));
    unsafe { ffi::sqlite3_free(expanded as *mut std::os::raw::c_void) };
    unsafe { ffi::sqlite3_finalize(raw) };
    Ok(())
}

#[test]
fn expanded_sql_with_int_param() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    let raw = unsafe { prepare_raw(&conn, "SELECT ? + 10") };
    unsafe { ffi::sqlite3_bind_int(raw, 1, 42) };
    let expanded = unsafe { ffi::sqlite3_expanded_sql(raw) };
    assert!(!expanded.is_null());
    let sql = unsafe { CStr::from_ptr(expanded) }.to_string_lossy();
    assert!(sql.contains("42"));
    unsafe { ffi::sqlite3_free(expanded as *mut std::os::raw::c_void) };
    unsafe { ffi::sqlite3_finalize(raw) };
    Ok(())
}

#[test]
fn expanded_sql_with_text_param() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    let raw = unsafe { prepare_raw(&conn, "SELECT ?") };
    let text = CString::new("hello").unwrap();
    unsafe { ffi::sqlite3_bind_text(raw, 1, text.as_ptr(), -1, ffi::SQLITE_TRANSIENT()) };
    let expanded = unsafe { ffi::sqlite3_expanded_sql(raw) };
    assert!(!expanded.is_null());
    let sql = unsafe { CStr::from_ptr(expanded) }.to_string_lossy();
    assert!(sql.contains("hello"));
    unsafe { ffi::sqlite3_free(expanded as *mut std::os::raw::c_void) };
    unsafe { ffi::sqlite3_finalize(raw) };
    Ok(())
}

#[test]
fn expanded_sql_null_stmt_is_null() {
    let expanded = unsafe { ffi::sqlite3_expanded_sql(std::ptr::null_mut()) };
    assert!(expanded.is_null());
}

#[test]
fn value_double_from_true_or_false_is_ok() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    conn.execute("CREATE TABLE test_bool(val TEXT)", [])?;
    conn.execute("INSERT INTO test_bool VALUES('t')", [])?;

    let sql = CString::new("SELECT val FROM test_bool").unwrap();
    let mut stmt: *mut ffi::sqlite3_stmt = ptr::null_mut();
    let mut tail: *const std::os::raw::c_char = ptr::null();
    let rc =
        unsafe { ffi::sqlite3_prepare_v2(conn.handle(), sql.as_ptr(), -1, &mut stmt, &mut tail) };
    assert_eq!(rc, ffi::SQLITE_OK);
    let step_rc = unsafe { ffi::sqlite3_step(stmt) };
    assert_eq!(step_rc, ffi::SQLITE_ROW);
    let val = unsafe { ffi::sqlite3_column_value(stmt, 0) };
    let d = unsafe { ffi::sqlite3_value_double(val) };
    assert!(d == 1.0 || d == 0.0);
    unsafe { ffi::sqlite3_finalize(stmt) };
    Ok(())
}

#[test]
fn value_double_from_false_is_zero() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    conn.execute("CREATE TABLE test_bool(val TEXT)", [])?;
    conn.execute("INSERT INTO test_bool VALUES('f')", [])?;

    let sql = CString::new("SELECT val FROM test_bool").unwrap();
    let mut stmt: *mut ffi::sqlite3_stmt = ptr::null_mut();
    let mut tail: *const std::os::raw::c_char = ptr::null();
    let rc =
        unsafe { ffi::sqlite3_prepare_v2(conn.handle(), sql.as_ptr(), -1, &mut stmt, &mut tail) };
    assert_eq!(rc, ffi::SQLITE_OK);
    let step_rc = unsafe { ffi::sqlite3_step(stmt) };
    assert_eq!(step_rc, ffi::SQLITE_ROW);
    let val = unsafe { ffi::sqlite3_column_value(stmt, 0) };
    let d = unsafe { ffi::sqlite3_value_double(val) };
    assert_eq!(d, 0.0);
    unsafe { ffi::sqlite3_finalize(stmt) };
    Ok(())
}

#[test]
fn value_double_from_number_parses() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    conn.execute("CREATE TABLE test_num(val TEXT)", [])?;
    conn.execute("INSERT INTO test_num VALUES('3.14159')", [])?;

    let sql = CString::new("SELECT val FROM test_num").unwrap();
    let mut stmt: *mut ffi::sqlite3_stmt = ptr::null_mut();
    let mut tail: *const std::os::raw::c_char = ptr::null();
    let rc =
        unsafe { ffi::sqlite3_prepare_v2(conn.handle(), sql.as_ptr(), -1, &mut stmt, &mut tail) };
    assert_eq!(rc, ffi::SQLITE_OK);
    let step_rc = unsafe { ffi::sqlite3_step(stmt) };
    assert_eq!(step_rc, ffi::SQLITE_ROW);
    let val = unsafe { ffi::sqlite3_column_value(stmt, 0) };
    let d = unsafe { ffi::sqlite3_value_double(val) };
    assert!((d - std::f64::consts::PI).abs() < 0.00001);
    unsafe { ffi::sqlite3_finalize(stmt) };
    Ok(())
}
