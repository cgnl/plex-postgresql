use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rusqlite::ffi;
use std::ffi::CString;
use std::os::raw::c_char;
use std::ptr;

unsafe fn open_db() -> *mut ffi::sqlite3 {
    let path = std::env::var("PLEX_SQLITE_DB").unwrap_or_else(|_| ":memory:".to_string());
    let cpath = CString::new(path).unwrap();
    let mut db: *mut ffi::sqlite3 = ptr::null_mut();
    let rc = ffi::sqlite3_open(cpath.as_ptr(), &mut db);
    if rc != ffi::SQLITE_OK {
        panic!("sqlite3_open failed: {}", rc);
    }

    let ddl = c"CREATE TABLE IF NOT EXISTS metadata_items (id INTEGER PRIMARY KEY, title TEXT);";
    let mut err: *mut c_char = ptr::null_mut();
    let rc = ffi::sqlite3_exec(db, ddl.as_ptr(), None, ptr::null_mut(), &mut err);
    if rc != ffi::SQLITE_OK {
        if !err.is_null() {
            ffi::sqlite3_free(err as *mut _);
        }
        panic!("sqlite3_exec DDL failed: {}", rc);
    }

    if cpath.to_bytes() == b":memory:" {
        let _ = ffi::sqlite3_exec(db, c"BEGIN;".as_ptr(), None, ptr::null_mut(), &mut err);
        for i in 1..=1000 {
            let sql = format!(
                "INSERT INTO metadata_items (id, title) VALUES ({}, 't{}');",
                i, i
            );
            let csql = CString::new(sql).unwrap();
            let _ = ffi::sqlite3_exec(db, csql.as_ptr(), None, ptr::null_mut(), &mut err);
        }
        let _ = ffi::sqlite3_exec(db, c"COMMIT;".as_ptr(), None, ptr::null_mut(), &mut err);
    }

    db
}

fn bench_micro(c: &mut Criterion) {
    unsafe {
        let db = open_db();
        let sql = CString::new("SELECT id, title FROM metadata_items WHERE id = ?").unwrap();

        c.bench_function("sqlite_prepare_finalize", |b| {
            b.iter(|| {
                let mut stmt: *mut ffi::sqlite3_stmt = ptr::null_mut();
                let rc = ffi::sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut());
                black_box(rc);
                if !stmt.is_null() {
                    ffi::sqlite3_finalize(stmt);
                }
            })
        });

        let mut stmt_bind: *mut ffi::sqlite3_stmt = ptr::null_mut();
        let rc = ffi::sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt_bind, ptr::null_mut());
        if rc == ffi::SQLITE_OK && !stmt_bind.is_null() {
            c.bench_function("sqlite_bind_step_reset", |b| {
                let mut i = 0i32;
                b.iter(|| {
                    i = i.wrapping_add(1);
                    let _ = ffi::sqlite3_bind_int(stmt_bind, 1, i % 1000 + 1);
                    let _ = ffi::sqlite3_step(stmt_bind);
                    let _ = ffi::sqlite3_reset(stmt_bind);
                })
            });
            ffi::sqlite3_finalize(stmt_bind);
        }

        let mut stmt_no_row: *mut ffi::sqlite3_stmt = ptr::null_mut();
        let rc = ffi::sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt_no_row, ptr::null_mut());
        if rc == ffi::SQLITE_OK && !stmt_no_row.is_null() {
            let _ = ffi::sqlite3_bind_int(stmt_no_row, 1, 9_999_999);
            c.bench_function("sqlite_step_reset_no_row", |b| {
                b.iter(|| {
                    let _ = ffi::sqlite3_step(stmt_no_row);
                    let _ = ffi::sqlite3_reset(stmt_no_row);
                })
            });
            ffi::sqlite3_finalize(stmt_no_row);
        }

        let mut stmt_row: *mut ffi::sqlite3_stmt = ptr::null_mut();
        let rc = ffi::sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt_row, ptr::null_mut());
        if rc == ffi::SQLITE_OK && !stmt_row.is_null() {
            let _ = ffi::sqlite3_bind_int(stmt_row, 1, 1);
            c.bench_function("sqlite_step_reset_row", |b| {
                b.iter(|| {
                    let _ = ffi::sqlite3_step(stmt_row);
                    let _ = ffi::sqlite3_reset(stmt_row);
                })
            });
            ffi::sqlite3_finalize(stmt_row);
        }

        let mut stmt_cycle: *mut ffi::sqlite3_stmt = ptr::null_mut();
        let rc = ffi::sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt_cycle, ptr::null_mut());
        if rc == ffi::SQLITE_OK && !stmt_cycle.is_null() {
            let mut i = 0i32;
            c.bench_function("sqlite_full_cycle", |b| {
                b.iter(|| {
                    i = i.wrapping_add(1);
                    let _ = ffi::sqlite3_bind_int(stmt_cycle, 1, i % 1000 + 1);
                    loop {
                        let step_rc = ffi::sqlite3_step(stmt_cycle);
                        if step_rc != ffi::SQLITE_ROW {
                            break;
                        }
                    }
                    let _ = ffi::sqlite3_reset(stmt_cycle);
                })
            });
            ffi::sqlite3_finalize(stmt_cycle);
        }

        ffi::sqlite3_close(db);
    }
}

criterion_group!(benches, bench_micro);
criterion_main!(benches);
