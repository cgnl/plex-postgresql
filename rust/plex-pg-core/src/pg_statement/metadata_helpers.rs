use std::ffi::CStr;

const DECLTYPE_INTEGER: &[u8] = b"INTEGER\0";
const DECLTYPE_DT_INTEGER_8: &[u8] = b"dt_integer(8)\0";
const DECLTYPE_REAL: &[u8] = b"REAL\0";
const DECLTYPE_BLOB: &[u8] = b"BLOB\0";
const DECLTYPE_TEXT: &[u8] = b"TEXT\0";

pub(crate) fn oid_to_sqlite_type(oid: u32) -> i32 {
    match oid {
        16 | 20 | 21 | 23 | 26 => super::SQLITE_INTEGER,
        700 | 701 | 1700 => super::SQLITE_FLOAT,
        17 => super::SQLITE_BLOB,
        _ => super::SQLITE_TEXT,
    }
}

pub(crate) fn oid_to_sqlite_decltype(oid: u32) -> &'static CStr {
    let bytes: &'static [u8] = match oid {
        16 | 21 | 23 | 26 => DECLTYPE_INTEGER,
        20 | 1114 | 1184 => DECLTYPE_DT_INTEGER_8, // 64-bit epoch/int8 → SOCI int64
        700 | 701 | 1700 => DECLTYPE_REAL,
        17 => DECLTYPE_BLOB,
        _ => DECLTYPE_TEXT,
    };
    unsafe { CStr::from_bytes_with_nul_unchecked(bytes) }
}

// Keep this compatibility helper for the existing FFI entry point. SQLite has
// no UNIQUE(account_id, guid) constraint, so an ordinary INSERT must neither
// merge watch state nor silently replace a row with the same natural key.
pub(crate) fn convert_metadata_settings_upsert(_sql: &str) -> Option<String> {
    None
}

pub(crate) fn extract_metadata_id(sql: &str) -> i64 {
    let lower = sql.to_lowercase();
    if !lower.contains("play_queue_generators") {
        return 0;
    }
    if !lower.contains("insert") {
        return 0;
    }

    let pat_encoded = "%2Fmetadata%2F";
    let pat_plain = "/metadata/";

    let after = if let Some(i) = sql.find(pat_encoded) {
        &sql[i + pat_encoded.len()..]
    } else if let Some(i) = sql.find(pat_plain) {
        &sql[i + pat_plain.len()..]
    } else {
        return 0;
    };

    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return 0;
    }
    digits.parse::<i64>().unwrap_or(0)
}
