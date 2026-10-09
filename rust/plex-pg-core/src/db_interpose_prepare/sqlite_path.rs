use super::*;
use crate::log_debug_lazy;
use crate::log_info_lazy;

pub(super) struct DummyShadowPrepareResult {
    pub(super) rc: c_int,
    pub(super) pre_trans: SqlTranslation,
    pub(super) have_pre_trans: bool,
}

pub(super) unsafe fn prepare_dummy_shadow_stmt(
    db: *mut sqlite3,
    z_sql: *const c_char,
    bytes: &[u8],
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> DummyShadowPrepareResult {
    let mut pre_trans = sql_translate(z_sql);

    if contains_icase_ptr(z_sql, "json_each(") {
        log_info_lazy!(
            "JSON_EACH_TRANSLATE: orig={}",
            cstr_prefix(z_sql, 220, "NULL")
        );
        log_info_lazy!(
            "JSON_EACH_TRANSLATE: rc={} err={} out={}",
            pre_trans.success,
            if pre_trans.error[0] != 0 {
                cstr_to_string_or(pre_trans.error.as_ptr(), "(null)")
            } else {
                "(null)".to_string()
            },
            cstr_prefix(pre_trans.sql, 220, "(null)")
        );
    }

    if contains_icase_ptr(z_sql, "metadata_item_settings")
        && contains_icase_ptr(z_sql, "metadata_items")
    {
        let q_count = bytes.iter().filter(|b| **b == b'?').count();
        let mut out_q_count = 0usize;
        if !pre_trans.sql.is_null() {
            let out_bytes = CStr::from_ptr(pre_trans.sql).to_bytes();
            out_q_count = out_bytes.iter().filter(|b| **b == b'?').count();
        }
        log_info_lazy!("MIS_TRANSLATE: orig={}", cstr_prefix(z_sql, 1000, "NULL"));
        log_info_lazy!(
            "MIS_TRANSLATE: rc={} params={} q_orig={} q_out={} out={}",
            pre_trans.success,
            pre_trans.param_count,
            q_count,
            out_q_count,
            cstr_prefix(pre_trans.sql, 1000, "(null)")
        );
    }

    if !pre_trans.sql.is_null() {
        let orig_q = bytes.iter().filter(|b| **b == b'?').count() as i32;
        if orig_q > pre_trans.param_count {
            let mut pos = None;
            for (i, b) in bytes.iter().enumerate() {
                if *b == b'?' {
                    pos = Some(i);
                    break;
                }
            }
            if let Some(pos) = pos {
                let start = pos.saturating_sub(60);
                let snippet = String::from_utf8_lossy(&bytes[start..bytes.len().min(start + 160)])
                    .into_owned();
                log_error(&format!(
                    "PLACEHOLDER_MISMATCH: orig_q={} translated_params={} around='{}'",
                    orig_q, pre_trans.param_count, snippet
                ));
            } else {
                log_error(&format!(
                    "PLACEHOLDER_MISMATCH: orig_q={} translated_params={} (no snippet)",
                    orig_q, pre_trans.param_count
                ));
            }
        }
    }

    let param_count = pre_trans.param_count;
    let dummy_sql = if param_count == 0 {
        "SELECT 1 WHERE 0".to_string()
    } else {
        let has_names = !pre_trans.param_names.is_null();
        let mut out = String::from("SELECT 1 WHERE ");
        for i in 0..param_count {
            if i > 0 {
                out.push_str(" AND ");
            }
            if has_names {
                let name_ptr = *pre_trans.param_names.add(i as usize);
                if !name_ptr.is_null() {
                    let name = CStr::from_ptr(name_ptr).to_string_lossy();
                    out.push(':');
                    out.push_str(&name);
                    out.push_str(" IS NOT NULL");
                } else {
                    out.push_str("? IS NOT NULL");
                }
            } else {
                out.push_str("? IS NOT NULL");
            }
            if out.len() >= 4096 - 40 {
                break;
            }
        }
        out
    };

    let dummy_c = CString::new(dummy_sql).ok();
    let dummy_ptr = dummy_c
        .as_ref()
        .map(|c| c.as_ptr())
        .unwrap_or_else(|| c"SELECT 1 WHERE 0".as_ptr());

    let rc = if let Some(prepare) = shim_sqlite3_prepare_v2 {
        prepare(db, dummy_ptr, -1, pp_stmt, pz_tail)
    } else {
        log_error("CRITICAL: shim_sqlite3_prepare_v2 not initialized!");
        if !pp_stmt.is_null() {
            *pp_stmt = ptr::null_mut();
        }
        SQLITE_ERROR
    };

    if rc == SQLITE_OK && !pp_stmt.is_null() && !(*pp_stmt).is_null() {
        pg_note_stmt_prepare(*pp_stmt, dummy_ptr);
    } else {
        log_error(&format!(
            "PREPARE: Dummy shadow prepare failed (rc={}, params={}): {} dummy={}",
            rc,
            param_count,
            cstr_prefix(z_sql, 100, "NULL"),
            cstr_prefix(dummy_ptr, 200, "NULL")
        ));
        sql_translation_free(&mut pre_trans as *mut SqlTranslation);
        return DummyShadowPrepareResult {
            rc,
            pre_trans: std::mem::zeroed(),
            have_pre_trans: false,
        };
    }

    log_debug_lazy!(
        "PREPARE: Dummy shadow OK ({} params) for PG query: {}",
        param_count,
        cstr_prefix(z_sql, 100, "NULL")
    );

    DummyShadowPrepareResult {
        rc,
        pre_trans,
        have_pre_trans: true,
    }
}

pub(super) unsafe fn prepare_real_sqlite_stmt(
    skip_complex_processing: c_int,
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    if !z_sql.is_null()
        && crate::pg_config::is_sqlite_passthrough_str(CStr::from_ptr(z_sql).to_str().unwrap_or(""))
    {
        let rc = if let Some(prepare) = shim_sqlite3_prepare_v2 {
            prepare(db, z_sql, n_byte, pp_stmt, pz_tail)
        } else {
            if !pp_stmt.is_null() {
                *pp_stmt = ptr::null_mut();
            }
            SQLITE_ERROR
        };
        if rc == SQLITE_OK && !pp_stmt.is_null() && !(*pp_stmt).is_null() {
            pg_note_stmt_prepare(*pp_stmt, z_sql);
        }
        return rc;
    }
    let mut cleaned_sql: Option<CString> = None;
    let mut sql_for_sqlite = z_sql;

    if skip_complex_processing == 0 && contains_icase_ptr(z_sql, "fts4_") {
        if let Ok(sql_str) = CStr::from_ptr(z_sql).to_str() {
            if let Some(out) = crate::db_interpose_prepare_helpers::simplify_fts_for_sqlite(sql_str)
            {
                if let Ok(cs) = CString::new(out) {
                    sql_for_sqlite = cs.as_ptr();
                    cleaned_sql = Some(cs);
                    log_info_lazy!("FTS query ORIGINAL: {}", cstr_prefix(z_sql, 500, "NULL"));
                    log_info_lazy!(
                        "FTS query SIMPLIFIED: {}",
                        cstr_prefix(sql_for_sqlite, 500, "NULL")
                    );
                }
            }
        }
    }

    if skip_complex_processing == 0 && contains_icase_ptr(sql_for_sqlite, "collate icu_root") {
        if let Ok(sql_str) = CStr::from_ptr(sql_for_sqlite).to_str() {
            if let Some(out) = crate::db_interpose_prepare_helpers::strip_collate_icu_root(sql_str)
            {
                if let Ok(cs) = CString::new(out) {
                    cleaned_sql = Some(cs);
                    sql_for_sqlite = cleaned_sql.as_ref().unwrap().as_ptr();
                }
            }
        }
    }

    if contains_icase_ptr(sql_for_sqlite, "fts4_") || contains_icase_ptr(sql_for_sqlite, " match ")
    {
        log_info_lazy!(
            "FTS query blocked from SQLite (tokenizer not available): {}",
            cstr_prefix(sql_for_sqlite, 100, "NULL")
        );
        if let Some(prepare) = shim_sqlite3_prepare_v2 {
            let rc = prepare(db, c"SELECT 1 WHERE 0".as_ptr(), -1, pp_stmt, pz_tail);
            if rc == SQLITE_OK && !pp_stmt.is_null() && !(*pp_stmt).is_null() {
                pg_note_stmt_prepare(*pp_stmt, c"SELECT 1 WHERE 0".as_ptr());
            }
            return rc;
        }
    }

    if skip_complex_processing == 0 && !sql_for_sqlite.is_null() {
        if let Ok(sql_str) = CStr::from_ptr(sql_for_sqlite).to_str() {
            if let Some(out) =
                crate::db_interpose_prepare_helpers::add_if_not_exists_for_sqlite_ddl(sql_str)
            {
                if let Ok(cs) = CString::new(out) {
                    cleaned_sql = Some(cs);
                    sql_for_sqlite = cleaned_sql.as_ref().unwrap().as_ptr();
                    log_info_lazy!(
                        "Added IF NOT EXISTS for SQLite DDL: {}",
                        cstr_prefix(sql_for_sqlite, 200, "NULL")
                    );
                }
            }
        }
    }

    let rc = if let Some(prepare) = shim_sqlite3_prepare_v2 {
        let n = if cleaned_sql.is_some() { -1 } else { n_byte };
        prepare(db, sql_for_sqlite, n, pp_stmt, pz_tail)
    } else {
        log_error("CRITICAL: shim_sqlite3_prepare_v2 not initialized!");
        if !pp_stmt.is_null() {
            *pp_stmt = ptr::null_mut();
        }
        SQLITE_ERROR
    };

    if rc == SQLITE_OK && !pp_stmt.is_null() && !(*pp_stmt).is_null() {
        pg_note_stmt_prepare(*pp_stmt, sql_for_sqlite);
    } else {
        let sqlite_err = orig_sqlite3_errmsg
            .map(|f| f(db))
            .unwrap_or_else(|| c"unknown".as_ptr());
        let sqlite_errcode = orig_sqlite3_errcode.map(|f| f(db)).unwrap_or(-1);
        log_error(&format!(
            "PREPARE_REAL_SQLITE FAILED: rc={} errcode={} errmsg='{}' sql={}",
            rc,
            sqlite_errcode,
            cstr_to_string_or(sqlite_err, "NULL"),
            cstr_prefix(sql_for_sqlite, 200, "NULL")
        ));
    }

    rc
}
