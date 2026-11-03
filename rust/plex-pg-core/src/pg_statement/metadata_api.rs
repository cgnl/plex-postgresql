use std::ffi::CString;
use std::os::raw::c_char;

use crate::db_interpose_helpers::cstr_to_str_or_empty;

use super::{
    convert_metadata_settings_upsert, extract_metadata_id, oid_to_sqlite_decltype,
    oid_to_sqlite_type, DECLTYPE_CASE_DT_INTEGER_8, DECLTYPE_CASE_NONE, DECLTYPE_CASE_NULL,
};

fn is_aggregate_expression_name(col: &str) -> bool {
    let col = col.trim();
    if col.is_empty() {
        return false;
    }

    let lower = col.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "count" | "cnt" | "sum" | "max" | "min" | "avg"
    ) || ["count(", "sum(", "max(", "min(", "avg("]
        .iter()
        .any(|needle| lower.contains(needle))
}

/// True when `col` is the alias of an aggregate call in `sql`, e.g.
/// `count(tags.id) AS tags_count`. SQLite reports no declared type for any
/// expression column; only plain table columns have one. Without this, an
/// aliased count got `dt_integer(8)` from its int8 OID and Plex's SOCI layer
/// raised std::bad_cast (e.g. the library genre list returned HTTP 500).
fn alias_is_aggregate(sql: &str, col: &str) -> bool {
    const AGGREGATES: [&str; 8] = [
        "count", "sum", "max", "min", "avg", "total", "group_concat", "string_agg",
    ];
    if col.is_empty() || sql.is_empty() {
        return false;
    }
    let lower = sql.to_ascii_lowercase();
    let col = col.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    for alias in [format!(" as {col}"), format!(" as \"{col}\""), format!(" as '{col}'")] {
        let mut from = 0;
        while let Some(found) = lower[from..].find(&alias) {
            let at = from + found;
            from = at + alias.len();
            if bytes
                .get(from)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
            {
                continue;
            }
            let before = lower[..at].trim_end().as_bytes();
            if before.last() != Some(&b')') {
                continue;
            }
            let mut depth = 0usize;
            let mut open = None;
            for index in (0..before.len()).rev() {
                match before[index] {
                    b')' => depth += 1,
                    b'(' => {
                        depth -= 1;
                        if depth == 0 {
                            open = Some(index);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let Some(open) = open else { continue };
            let head = lower[..open].trim_end();
            let name_start = head
                .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .map_or(0, |i| i + 1);
            if AGGREGATES.contains(&&head[name_start..]) {
                return true;
            }
        }
    }
    false
}

pub fn rust_oid_to_sqlite_type(oid: u32) -> i32 {
    oid_to_sqlite_type(oid)
}

pub fn rust_oid_to_sqlite_decltype(oid: u32) -> *const c_char {
    oid_to_sqlite_decltype(oid).as_ptr()
}

pub fn rust_decltype_special_case(
    oid: u32,
    col_name: *const c_char,
    pg_sql: *const c_char,
    table_oid: u32,
) -> i32 {
    let col = unsafe { cstr_to_str_or_empty(col_name) };
    let sql = unsafe { cstr_to_str_or_empty(pg_sql) };

    if oid == 20 && !col.is_empty() {
        if col.contains("_at") || col.contains("timestamp") || col.contains("time") {
            return DECLTYPE_CASE_DT_INTEGER_8;
        }
        if col == "greatest" && sql.contains("metadata_items.changed_at") {
            return DECLTYPE_CASE_DT_INTEGER_8;
        }
    }

    // Only classify as aggregate when table_oid==0 (expression column).
    // Real table columns named "count", "max", etc. have table_oid != 0
    // and must keep their normal decltype.
    if table_oid == 0 && (is_aggregate_expression_name(col) || alias_is_aggregate(sql, col)) {
        return DECLTYPE_CASE_NULL;
    }

    DECLTYPE_CASE_NONE
}

pub fn rust_convert_metadata_settings_upsert(sql: *const c_char) -> *mut c_char {
    let s = unsafe { cstr_to_str_or_empty(sql) };
    if s.is_empty() {
        return std::ptr::null_mut();
    }
    match convert_metadata_settings_upsert(s) {
        Some(result) => CString::new(result)
            .map(|cs| cs.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        None => std::ptr::null_mut(),
    }
}

pub fn rust_extract_metadata_id(sql: *const c_char) -> i64 {
    let s = unsafe { cstr_to_str_or_empty(sql) };
    if s.is_empty() {
        return 0;
    }
    extract_metadata_id(s)
}

#[cfg(test)]
mod alias_aggregate_tests {
    use super::alias_is_aggregate;

    #[test]
    fn aliased_aggregates_are_detected() {
        let genre = r#"SELECT * FROM (SELECT tags.id AS "tags_id", count(tags.id) AS tags_count FROM tags) AS t"#;
        assert!(alias_is_aggregate(genre, "tags_count"));
        assert!(!alias_is_aggregate(genre, "tags_id"));
        assert!(alias_is_aggregate("select max(coalesce(a, b)) as \"m\" from t", "m"));
        assert!(alias_is_aggregate("select group_concat(x) as names from t", "names"));
        assert!(!alias_is_aggregate("select coalesce(a, b) as c from t", "c"));
        assert!(!alias_is_aggregate("select count(*) as n_total from t", "n"));
        assert!(!alias_is_aggregate("select x as tags_count from t", "tags_count"));
    }
}

