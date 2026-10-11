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

    let ddl = c"CREATE TABLE IF NOT EXISTS metadata_items (id INTEGER PRIMARY KEY, title TEXT, rating INTEGER, parent_id INTEGER, updated_at INTEGER);";
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
            let sql = format!("INSERT INTO metadata_items (id, title, rating, parent_id, updated_at) VALUES ({}, 't{}', {}, {}, {});", i, i, i % 10, i % 5, 123456 + i);
            let csql = CString::new(sql).unwrap();
            let _ = ffi::sqlite3_exec(db, csql.as_ptr(), None, ptr::null_mut(), &mut err);
        }
        let _ = ffi::sqlite3_exec(db, c"COMMIT;".as_ptr(), None, ptr::null_mut(), &mut err);
    }

    db
}

fn bench_shim(c: &mut Criterion) {
    unsafe {
        let db = open_db();
        let queries = [
            "SELECT * FROM metadata_items WHERE id = ?",
            "SELECT id, title, rating FROM metadata_items WHERE parent_id = ? ORDER BY \"index\"",
            "INSERT INTO metadata_items (title, updated_at) VALUES (?, ?)",
            "UPDATE metadata_items SET updated_at = ? WHERE id = ?",
            "SELECT id, title FROM metadata_items WHERE title LIKE ? COLLATE NOCASE",
        ];

        c.bench_function("shim_prepare_bind_step_finalize", |b| {
            let mut i = 0i32;
            b.iter(|| {
                let sql = queries[(i as usize) % queries.len()];
                i = i.wrapping_add(1);
                let csql = CString::new(sql).unwrap();
                let mut stmt: *mut ffi::sqlite3_stmt = ptr::null_mut();
                let rc = ffi::sqlite3_prepare_v2(db, csql.as_ptr(), -1, &mut stmt, ptr::null_mut());
                if rc == ffi::SQLITE_OK && !stmt.is_null() {
                    let _ = ffi::sqlite3_bind_int(stmt, 1, i);
                    let _ = ffi::sqlite3_step(stmt);
                    ffi::sqlite3_finalize(stmt);
                }
                black_box(rc);
            })
        });

        c.bench_function("shim_exec_varying_sql", |b| {
            let mut i = 0i32;
            b.iter(|| {
                i = i.wrapping_add(1);
                let sql = format!("SELECT * FROM metadata_items WHERE id = {}", i);
                let csql = CString::new(sql).unwrap();
                let mut err: *mut c_char = ptr::null_mut();
                let rc = ffi::sqlite3_exec(db, csql.as_ptr(), None, ptr::null_mut(), &mut err);
                if !err.is_null() {
                    ffi::sqlite3_free(err as *mut _);
                }
                black_box(rc);
            })
        });

        let cached_sql = CString::new("SELECT id, title FROM metadata_items WHERE id = 1").unwrap();
        c.bench_function("shim_exec_cached_sql", |b| {
            b.iter(|| {
                let mut err: *mut c_char = ptr::null_mut();
                let rc =
                    ffi::sqlite3_exec(db, cached_sql.as_ptr(), None, ptr::null_mut(), &mut err);
                if !err.is_null() {
                    ffi::sqlite3_free(err as *mut _);
                }
                black_box(rc);
            })
        });

        let param_sql =
            CString::new("SELECT id, title, rating FROM metadata_items WHERE id = ?").unwrap();
        c.bench_function("shim_parameterized", |b| {
            let mut i = 0i32;
            b.iter(|| {
                i = i.wrapping_add(1);
                let mut stmt: *mut ffi::sqlite3_stmt = ptr::null_mut();
                let rc =
                    ffi::sqlite3_prepare_v2(db, param_sql.as_ptr(), -1, &mut stmt, ptr::null_mut());
                if rc == ffi::SQLITE_OK && !stmt.is_null() {
                    let _ = ffi::sqlite3_bind_int(stmt, 1, (i % 1000) + 1);
                    let _ = ffi::sqlite3_step(stmt);
                    ffi::sqlite3_finalize(stmt);
                }
                black_box(rc);
            })
        });

        ffi::sqlite3_close(db);
    }
}

criterion_group!(benches, bench_shim);
criterion_main!(benches);
