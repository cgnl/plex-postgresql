use super::*;
use crate::log_info_lazy;

unsafe fn column_exists_in_sqlite(
    db: *mut sqlite3,
    table_name: *const c_char,
    column_name: *const c_char,
) -> bool {
    if db.is_null() || table_name.is_null() || column_name.is_null() {
        return false;
    }
    let prepare = match shim_sqlite3_prepare_v2 {
        Some(f) => f,
        None => return false,
    };

    let mut pragma_sql = [0 as c_char; 512];
    libc::snprintf(
        pragma_sql.as_mut_ptr(),
        pragma_sql.len(),
        b"PRAGMA table_info(%s)\0".as_ptr() as *const c_char,
        table_name,
    );

    let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
    let rc = prepare(db, pragma_sql.as_ptr(), -1, &mut stmt, ptr::null_mut());
    if rc != SQLITE_OK || stmt.is_null() {
        return false;
    }

    let mut found = false;
    if let Some(step) = orig_sqlite3_step {
        while step(stmt) == SQLITE_ROW {
            let col_ptr = match orig_sqlite3_column_text {
                Some(col) => col(stmt, 1) as *const c_char,
                None => ptr::null(),
            };
            if !col_ptr.is_null() {
                let col = CStr::from_ptr(col_ptr).to_bytes();
                let want = CStr::from_ptr(column_name).to_bytes();
                if col.eq_ignore_ascii_case(want) {
                    found = true;
                    break;
                }
            }
        }
    }

    if let Some(fin) = orig_sqlite3_finalize {
        fin(stmt);
    }
    found
}

pub(super) unsafe fn maybe_skip_alter_table_add(
    db: *mut sqlite3,
    z_sql: *const c_char,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> Option<c_int> {
    if z_sql.is_null() {
        return None;
    }
    if !contains_icase_ptr(z_sql, "ALTER TABLE") || !contains_icase_ptr(z_sql, " ADD ") {
        return None;
    }

    let table_pos = libc::strcasestr(z_sql, b"ALTER TABLE\0".as_ptr() as *const c_char);
    if table_pos.is_null() {
        return None;
    }
    let mut table_start = table_pos.add(11);
    while *table_start == b' ' as c_char {
        table_start = table_start.add(1);
    }

    let mut table_name = [0 as c_char; 256];
    if *table_start == b'\'' as c_char || *table_start == b'"' as c_char {
        let quote = *table_start;
        table_start = table_start.add(1);
        let end = libc::strchr(table_start, quote as i32);
        if !end.is_null() {
            let len = (end as usize).saturating_sub(table_start as usize);
            if len < table_name.len() {
                ptr::copy_nonoverlapping(
                    table_start as *const u8,
                    table_name.as_mut_ptr().cast::<u8>(),
                    len,
                );
            }
        }
    } else {
        let mut i = 0usize;
        while *table_start.add(i) != 0
            && *table_start.add(i) != b' ' as c_char
            && i < table_name.len() - 1
        {
            table_name[i] = *table_start.add(i);
            i += 1;
        }
    }

    if table_name[0] == 0 {
        return None;
    }

    let add_pos = libc::strcasestr(z_sql, b" ADD \0".as_ptr() as *const c_char);
    if add_pos.is_null() {
        return None;
    }
    let mut add_ptr = add_pos.add(5);
    while *add_ptr == b' ' as c_char {
        add_ptr = add_ptr.add(1);
    }

    let mut column_name = [0 as c_char; 256];
    if *add_ptr == b'\'' as c_char || *add_ptr == b'"' as c_char {
        let quote = *add_ptr;
        add_ptr = add_ptr.add(1);
        let end = libc::strchr(add_ptr, quote as i32);
        if !end.is_null() {
            let len = (end as usize).saturating_sub(add_ptr as usize);
            if len < column_name.len() {
                ptr::copy_nonoverlapping(
                    add_ptr as *const u8,
                    column_name.as_mut_ptr().cast::<u8>(),
                    len,
                );
            }
        }
    } else {
        let mut i = 0usize;
        while *add_ptr.add(i) != 0 && *add_ptr.add(i) != b' ' as c_char && i < column_name.len() - 1
        {
            column_name[i] = *add_ptr.add(i);
            i += 1;
        }
    }

    if column_name[0] == 0 {
        return None;
    }

    if column_exists_in_sqlite(db, table_name.as_ptr(), column_name.as_ptr()) {
        log_info_lazy!(
            "ALTER TABLE ADD COLUMN skipped (column '{}' already exists in '{}')",
            cstr_to_string_or(column_name.as_ptr(), ""),
            cstr_to_string_or(table_name.as_ptr(), "")
        );
        if let Some(prepare) = shim_sqlite3_prepare_v2 {
            let rc = prepare(
                db,
                b"SELECT 1 WHERE 0\0".as_ptr() as *const c_char,
                -1,
                pp_stmt,
                pz_tail,
            );
            if rc == SQLITE_OK && !pp_stmt.is_null() && !(*pp_stmt).is_null() {
                pg_note_stmt_prepare(*pp_stmt, b"SELECT 1 WHERE 0\0".as_ptr() as *const c_char);
            }
            return Some(rc);
        }
        if !pp_stmt.is_null() {
            *pp_stmt = ptr::null_mut();
        }
        if !pz_tail.is_null() {
            *pz_tail = ptr::null();
        }
        return Some(SQLITE_OK);
    }

    None
}
