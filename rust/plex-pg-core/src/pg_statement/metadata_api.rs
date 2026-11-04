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

/// True when `col` is the alias of a select item that calls an aggregate,
/// e.g. `count(tags.id) AS tags_count` or `coalesce(sum(x), 0) AS total`.
/// SQLite reports no declared type for any expression column; only plain
/// table columns have one. Without this, an aliased count got `dt_integer(8)`
/// from its int8 OID and Plex's SOCI layer raised std::bad_cast (e.g. the
/// library genre list returned HTTP 500). Works on bytes: SQL may hold
/// non-ASCII literals.
fn alias_is_aggregate(sql: &str, col: &str) -> bool {
    let (sql, col) = (sql.as_bytes(), col.as_bytes());
    if col.is_empty() {
        return false;
    }
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    for at in 1..sql.len().saturating_sub(1) {
        let prev = sql[at - 1];
        if !sql[at..at + 2].eq_ignore_ascii_case(b"as") || !(prev.is_ascii_whitespace() || prev == b')') {
            continue;
        }
        let mut name = at + 2;
        while sql.get(name).is_some_and(|b| b.is_ascii_whitespace()) {
            name += 1;
        }
        if name == at + 2 && !matches!(sql.get(name), Some(b'"' | b'\'' | b'`')) {
            continue; // `as` inside a longer word
        }
        let quote = sql.get(name).copied().filter(|b| matches!(b, b'"' | b'\'' | b'`'));
        if quote.is_some() {
            name += 1;
        }
        let Some(candidate) = sql.get(name..name + col.len()) else {
            continue;
        };
        let after = sql.get(name + col.len()).copied();
        let ends = match quote {
            Some(q) => after == Some(q),
            None => after.is_none_or(|b| !word(b)),
        };
        if candidate.eq_ignore_ascii_case(col) && ends && select_item_calls_aggregate(&sql[..at]) {
            return true;
        }
    }
    false
}

/// Whether the select item ending at the end of `before` calls an aggregate.
fn select_item_calls_aggregate(before: &[u8]) -> bool {
    const AGGREGATES: [&[u8]; 8] = [
        b"count", b"sum", b"max", b"min", b"avg", b"total", b"group_concat", b"string_agg",
    ];
    let mut depth = 0usize;
    let mut start = 0;
    for index in (0..before.len()).rev() {
        match before[index] {
            b')' => depth += 1,
            b'(' if depth == 0 => {
                start = index + 1;
                break;
            }
            b'(' => depth -= 1,
            b',' if depth == 0 => {
                start = index + 1;
                break;
            }
            _ => {}
        }
    }
    let item = &before[start..];
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    AGGREGATES.iter().any(|name| {
        (0..item.len().saturating_sub(name.len() - 1)).any(|at| {
            item[at..].len() >= name.len()
                && item[at..at + name.len()].eq_ignore_ascii_case(name)
                && (at == 0 || !word(item[at - 1]))
                && item[at + name.len()..]
                    .iter()
                    .find(|b| !b.is_ascii_whitespace())
                    == Some(&b'(')
        })
    })
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
        assert!(alias_is_aggregate("select coalesce(sum(x), 0) as total from t", "total"));
        assert!(alias_is_aggregate("select count(*) filter (where a) as c from t", "c"));
        assert!(alias_is_aggregate("select a, max(b)AS m from t", "m"));
        assert!(alias_is_aggregate("select a,\n\tcount(b)\n\tAS n from t", "n"));
        assert!(!alias_is_aggregate("select count(a) as n, b as c from t", "c"));
        // Literals are not parsed; non-ASCII text must simply not panic.
        let _ = alias_is_aggregate("select 'émax(a) as c', b as c from t", "c");
        let _ = alias_is_aggregate("select é as é from t", "é");
        assert!(!alias_is_aggregate("select counter(a) as c from t", "c"));
    }
}

