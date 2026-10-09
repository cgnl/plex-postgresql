use super::*;
use crate::db_interpose_bind::support::{
    begin_bind, bytes_to_pg_hex, contains_binary_bytes, free_dynamic_param_value,
    invoke_destructor_if_custom, is_pg_routed_noncached, mapped_param_index, retry_on_misuse,
};
use crate::log_debug_lazy;

// Read only a Preferences start tag. Attribute names and quoted values are
// scanned independently, so prefixed names and values in comments cannot match.
fn parse_machine_identifier(content: &str) -> Option<String> {
    let content = content.trim_start_matches('\u{feff}').trim_start();
    let content = if content.starts_with("<?xml") {
        content.split_once("?>")?.1.trim_start()
    } else {
        content
    };
    let mut rest = content.strip_prefix("<Preferences")?;
    if !rest.starts_with(char::is_whitespace) && !rest.starts_with(['/', '>']) {
        return None;
    }
    let mut identifier = None;
    loop {
        rest = rest.trim_start();
        if rest.starts_with("/>") || rest.starts_with('>') {
            return identifier;
        }
        let end = rest.find(|c: char| c.is_whitespace() || c == '=')?;
        let name = &rest[..end];
        if name.is_empty() {
            return None;
        }
        rest = rest[end..].trim_start().strip_prefix('=')?.trim_start();
        let quote = rest.chars().next()?;
        if quote != '\'' && quote != '"' {
            return None;
        }
        rest = &rest[1..];
        let end = rest.find(quote)?;
        let value = &rest[..end];
        if name == "MachineIdentifier" {
            if identifier.is_some() {
                return None;
            }
            identifier = Some(normalize_machine_identifier(value)?);
        }
        rest = &rest[end + 1..];
        if !rest.starts_with(char::is_whitespace) && !rest.starts_with(['/', '>']) {
            return None;
        }
    }
}

fn normalize_machine_identifier(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                *b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
    {
        return Some(value.to_owned());
    }
    if bytes.len() != 32 || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    // ASCII validation above makes these byte boundaries safe UTF-8 boundaries.
    Some(format!(
        "{}-{}-{}-{}-{}",
        &value[..8],
        &value[8..12],
        &value[12..16],
        &value[16..20],
        &value[20..]
    ))
}

fn machine_identifier_preferences_path(
    support_dir: Option<std::ffi::OsString>,
) -> std::path::PathBuf {
    match support_dir {
        Some(directory) => std::path::PathBuf::from(directory)
            .join("Plex Media Server")
            .join("Preferences.xml"),
        None => std::path::PathBuf::from(
            "/config/Library/Application Support/Plex Media Server/Preferences.xml",
        ),
    }
}

fn get_machine_identifier() -> Option<String> {
    let path = machine_identifier_preferences_path(std::env::var_os(
        "PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR",
    ));
    // An explicit support directory selects exactly one identity source.
    // Missing or malformed preferences never fall back to another server.
    parse_machine_identifier(&std::fs::read_to_string(path).ok()?)
}

// Only proven identifier predicates qualify. In particular, SELECT's first
// parameter and parameters whose names merely contain "identifier" do not.
fn is_device_identifier_bind(sql: &str, idx: c_int) -> bool {
    use sqlparser::tokenizer::{Token, Tokenizer};
    let dialect = sqlparser::dialect::SQLiteDialect {};
    let Ok(tokens) = Tokenizer::new(&dialect, sql).tokenize() else {
        return false;
    };
    let tokens: Vec<_> = tokens
        .into_iter()
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .collect();
    let word =
        |t: &Token, value: &str| matches!(t, Token::Word(w) if w.value.eq_ignore_ascii_case(value));
    if !tokens
        .windows(2)
        .any(|w| (word(&w[0], "from") || word(&w[0], "update")) && word(&w[1], "devices"))
    {
        return false;
    }
    if tokens.iter().filter(|t| word(t, "from")).count() > 1
        || tokens.iter().any(|t| word(t, "join") || word(t, "union"))
    {
        return false;
    }
    if tokens
        .iter()
        .enumerate()
        .any(|(i, token)| *token == Token::SemiColon && i + 1 < tokens.len())
    {
        return false;
    }
    let mut next_index = 0;
    let mut names = std::collections::HashMap::new();
    let mut in_predicate = false;
    let mut i = 0;
    while i < tokens.len() {
        if word(&tokens[i], "where") {
            in_predicate = true;
        }
        let (parameter, consumed) = match &tokens[i] {
            Token::Placeholder(p) => (Some(p.clone()), 1),
            Token::Colon | Token::AtSign if matches!(tokens.get(i + 1), Some(Token::Word(_))) => {
                (Some(format!("{}{}", tokens[i], tokens[i + 1])), 2)
            }
            _ => (None, 1),
        };
        if let Some(parameter) = parameter {
            let parameter_index = if parameter == "?" {
                next_index + 1
            } else if let Some(number) = parameter.strip_prefix('?') {
                let Ok(number) = number.parse::<c_int>() else {
                    return false;
                };
                number
            } else {
                *names.entry(parameter).or_insert(next_index + 1)
            };
            next_index = next_index.max(parameter_index);
            let column_start = if i >= 4 && tokens[i - 3] == Token::Period {
                if !word(&tokens[i - 4], "devices") {
                    i
                } else {
                    i - 4
                }
            } else {
                i.saturating_sub(2)
            };
            let direct_predicate = column_start > 0
                && (word(&tokens[column_start - 1], "where")
                    || word(&tokens[column_start - 1], "and")
                    || word(&tokens[column_start - 1], "or")
                    || tokens[column_start - 1] == Token::LParen);
            if in_predicate
                && i >= 2
                && direct_predicate
                && tokens[i - 1] == Token::Eq
                && word(&tokens[i - 2], "identifier")
                && parameter_index == idx
            {
                return true;
            }
        }
        i += consumed;
    }
    false
}

unsafe fn replacement_identifier(pg_stmt: *mut PgStmt, idx: c_int, empty: bool) -> Option<String> {
    if pg_stmt.is_null() || !empty || std::env::var_os("BYPASS_UUID_INTERCEPT").is_some() {
        return None;
    }
    let sql = (*pg_stmt).sql;
    if sql.is_null() {
        return None;
    }
    let sql = std::ffi::CStr::from_ptr(sql).to_str().ok()?;
    if !is_device_identifier_bind(sql, idx) {
        return None;
    }
    get_machine_identifier()
}

// Normalize only the PostgreSQL-owned snapshot. SQLite still receives the
// original pointer, bytes and destructor, and custom destructors cannot consume
// the normalized buffer while PostgreSQL is copying it.
unsafe fn normalize_match_snapshot(
    pg_stmt: *mut PgStmt,
    idx: c_int,
    snapshot: Option<Vec<u8>>,
) -> Result<Option<Vec<u8>>, &'static str> {
    let Some(bytes) = snapshot else {
        return Ok(None);
    };
    if pg_stmt.is_null() || (*pg_stmt).is_pg == 0 || (*pg_stmt).sql.is_null() {
        return Ok(Some(bytes));
    }
    let Ok(sql) = std::ffi::CStr::from_ptr((*pg_stmt).sql).to_str() else {
        return Ok(Some(bytes));
    };
    // Ordinary bind paths avoid tokenization. This is only a fast rejection;
    // the tokenizer below still proves source table, RHS and SQLite index.
    if !sql
        .as_bytes()
        .windows(5)
        .any(|token| token.eq_ignore_ascii_case(b"MATCH"))
    {
        return Ok(Some(bytes));
    }
    if crate::query::fts_match_parameter(sql, idx)? {
        let term = std::str::from_utf8(&bytes).map_err(|_| "MATCH term must be UTF-8")?;
        return Ok(Some(crate::query::convert_fts_term(term).into_bytes()));
    }
    Ok(Some(bytes))
}

unsafe fn store_text_param(
    pg_stmt: *mut PgStmt,
    pg_idx: usize,
    val: *const c_char,
    actual_len: usize,
    duplicate_input: bool,
    idx: c_int,
    label: &str,
) {
    free_dynamic_param_value(pg_stmt, pg_idx);
    let stmt = &mut *pg_stmt;

    if contains_binary_bytes(val.cast::<u8>(), actual_len) {
        log_debug_lazy!(
            "{}: detected binary data at idx={}, len={}, converting to hex",
            label,
            idx,
            actual_len
        );
        stmt.param_values[pg_idx] = bytes_to_pg_hex(val.cast::<u8>(), actual_len);
        return;
    }

    if duplicate_input {
        stmt.param_values[pg_idx] = libc::strdup(val);
        if crate::pg_mem_telemetry::rust_mem_telemetry_enabled() != 0 {
            crate::pg_mem_telemetry::rust_mem_telemetry_add(
                PMT_BIND_TEXT_ALLOC,
                actual_len as u64 + 1,
                1,
            );
        }
        return;
    }

    stmt.param_values[pg_idx] = libc::malloc(actual_len + 1) as *mut c_char;
    if !stmt.param_values[pg_idx].is_null() {
        libc::memcpy(
            stmt.param_values[pg_idx] as *mut c_void,
            val as *const c_void,
            actual_len,
        );
        *stmt.param_values[pg_idx].add(actual_len) = 0;
        if crate::pg_mem_telemetry::rust_mem_telemetry_enabled() != 0 {
            crate::pg_mem_telemetry::rust_mem_telemetry_add(
                PMT_BIND_TEXT_ALLOC,
                actual_len as u64 + 1,
                1,
            );
        }
    }
}

unsafe fn store_blob_hex_param(
    pg_stmt: *mut PgStmt,
    pg_idx: usize,
    val: *const c_void,
    n_bytes: usize,
    idx: c_int,
    label: &str,
) {
    free_dynamic_param_value(pg_stmt, pg_idx);
    let stmt = &mut *pg_stmt;
    log_debug_lazy!(
        "{}: converting {} bytes to hex at idx={}",
        label,
        n_bytes,
        idx
    );
    stmt.param_values[pg_idx] = bytes_to_pg_hex(val.cast::<u8>(), n_bytes);
    stmt.param_lengths[pg_idx] = 0;
    stmt.param_formats[pg_idx] = 0;
}

pub(super) fn bind_text_impl(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    mut val: *const c_char,
    mut n_bytes: c_int,
    destructor: *mut c_void,
) -> c_int {
    let (pg_stmt, guard) = unsafe { begin_bind(PHASE_BIND_TEXT, p_stmt) };
    let original_val = val;
    let original_length = n_bytes;
    let replacement = unsafe {
        replacement_identifier(
            pg_stmt,
            idx,
            !val.is_null() && (n_bytes < 0 && *val == 0 || n_bytes == 0),
        )
    };
    let bind_destructor = if let Some(value) = replacement.as_ref() {
        val = value.as_ptr().cast();
        n_bytes = value.len() as c_int;
        usize::MAX as *mut c_void // SQLITE_TRANSIENT: SQLite must copy this local string.
    } else {
        destructor
    };
    let pg_idx = unsafe { mapped_param_index(pg_stmt, p_stmt, idx) };
    // Snapshot before SQLite or a caller destructor can consume the input.
    let snapshot = if pg_idx.is_some() && !val.is_null() {
        let len = if n_bytes < 0 {
            unsafe { libc::strlen(val) }
        } else {
            n_bytes as usize
        };
        Some(unsafe { std::slice::from_raw_parts(val.cast::<u8>(), len) }.to_vec())
    } else {
        None
    };

    let snapshot = match unsafe { normalize_match_snapshot(pg_stmt, idx, snapshot) } {
        Ok(snapshot) => snapshot,
        Err(reason) => {
            log_debug_lazy!("bind_text: unsupported MATCH parameter: {}", reason);
            if let Some(pg_idx) = pg_idx {
                unsafe {
                    free_dynamic_param_value(pg_stmt, pg_idx);
                    (*pg_stmt).param_values[pg_idx] = ptr::null_mut();
                }
            }
            if !original_val.is_null() && original_length >= 0 {
                unsafe { invoke_destructor_if_custom(original_val.cast(), destructor) };
            }
            drop(guard);
            return SQLITE_ERROR;
        }
    };

    if !pg_stmt.is_null() {
        let stmt = unsafe { &*pg_stmt };
        let sql_bytes = unsafe {
            if !stmt.sql.is_null() {
                crate::byte_utils::cstr_bytes(stmt.sql)
            } else {
                b""
            }
        };
        if crate::byte_utils::contains_bytes(sql_bytes, b"devices")
            || crate::byte_utils::contains_bytes(sql_bytes, b"library_sections")
            || crate::byte_utils::contains_bytes(sql_bytes, b"plugins")
        {
            let val_str = if val.is_null() {
                "NULL".to_string()
            } else {
                let actual_len = if n_bytes < 0 {
                    unsafe { libc::strlen(val) as usize }
                } else {
                    n_bytes as usize
                };
                let bytes =
                    unsafe { std::slice::from_raw_parts(val.cast::<u8>(), actual_len.min(100)) };
                String::from_utf8_lossy(bytes).into_owned()
            };
            log_debug_lazy!(
                "BIND TEXT: stmt={:p} idx={} val='{}' n_bytes={} sql={}",
                pg_stmt,
                idx,
                val_str,
                n_bytes,
                crate::db_interpose_conn_utils::cstr_to_string_or(stmt.sql, "NULL")
            );
        }
    }

    let rc = if is_pg_routed_noncached(pg_stmt) {
        // Skip orig_sqlite3_bind_text — PG param storage below is sufficient.
        if pg_idx.is_some() {
            SQLITE_OK
        } else {
            25
        } // SQLITE_RANGE
    } else {
        let mut rc = get_orig_sqlite3_bind_text()
            .map(|f| unsafe { f(p_stmt, idx, val, n_bytes, bind_destructor) })
            .unwrap_or(SQLITE_ERROR);
        unsafe {
            rc = if replacement.is_some()
                || destructor.is_null()
                || destructor as usize == usize::MAX
            {
                retry_on_misuse(rc, p_stmt, pg_stmt, || {
                    get_orig_sqlite3_bind_text()
                        .map(|f| f(p_stmt, idx, val, n_bytes, bind_destructor))
                        .unwrap_or(SQLITE_ERROR)
                })
            } else {
                rc
            };
        }
        rc
    };

    if rc == SQLITE_OK {
        if let Some(pg_idx) = pg_idx {
            if let Some(bytes) = snapshot.as_ref() {
                unsafe {
                    store_text_param(
                        pg_stmt,
                        pg_idx,
                        bytes.as_ptr().cast(),
                        bytes.len(),
                        false,
                        idx,
                        "bind_text_impl",
                    )
                };
            } else {
                unsafe {
                    free_dynamic_param_value(pg_stmt, pg_idx);
                    (*pg_stmt).param_values[pg_idx] = ptr::null_mut();
                }
            }
        }
    }
    // A replacement never transfers the caller's original pointer to SQLite.
    // Negative-length bind_text and NULL have no destructor obligation.
    if !original_val.is_null()
        && original_length >= 0
        && (replacement.is_some() || is_pg_routed_noncached(pg_stmt))
    {
        unsafe { invoke_destructor_if_custom(original_val.cast(), destructor) };
    }

    drop(guard);
    crate::pg_mem_telemetry::rust_mem_telemetry_maybe_log();
    rc
}

pub(super) fn bind_blob_impl(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_void,
    n_bytes: c_int,
    destructor: *mut c_void,
) -> c_int {
    let (pg_stmt, guard) = unsafe { begin_bind(PHASE_BIND_BLOB, p_stmt) };

    let rc = if is_pg_routed_noncached(pg_stmt) {
        if !val.is_null() {
            unsafe { invoke_destructor_if_custom(val, destructor) };
        }
        SQLITE_OK
    } else {
        let mut rc = get_orig_sqlite3_bind_blob()
            .map(|f| unsafe { f(p_stmt, idx, val, n_bytes, destructor) })
            .unwrap_or(SQLITE_ERROR);
        unsafe {
            rc = retry_on_misuse(rc, p_stmt, pg_stmt, || {
                get_orig_sqlite3_bind_blob()
                    .map(|f| f(p_stmt, idx, val, n_bytes, destructor))
                    .unwrap_or(SQLITE_ERROR)
            });
        }
        rc
    };

    if !val.is_null() && n_bytes > 0 {
        if let Some(pg_idx) = unsafe { mapped_param_index(pg_stmt, p_stmt, idx) } {
            unsafe {
                store_blob_hex_param(pg_stmt, pg_idx, val, n_bytes as usize, idx, "bind_blob");
            }
        }
    }

    drop(guard);
    rc
}

pub(super) fn bind_blob64_impl(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_void,
    n_bytes: u64,
    destructor: *mut c_void,
) -> c_int {
    let (pg_stmt, guard) = unsafe { begin_bind(PHASE_BIND_BLOB64, p_stmt) };

    let rc = if is_pg_routed_noncached(pg_stmt) {
        if !val.is_null() {
            unsafe { invoke_destructor_if_custom(val, destructor) };
        }
        SQLITE_OK
    } else {
        let mut rc = get_orig_sqlite3_bind_blob64()
            .map(|f| unsafe { f(p_stmt, idx, val, n_bytes, destructor) })
            .unwrap_or(SQLITE_ERROR);
        unsafe {
            rc = retry_on_misuse(rc, p_stmt, pg_stmt, || {
                get_orig_sqlite3_bind_blob64()
                    .map(|f| f(p_stmt, idx, val, n_bytes, destructor))
                    .unwrap_or(SQLITE_ERROR)
            });
        }
        rc
    };

    if !val.is_null() && n_bytes > 0 {
        if let Some(pg_idx) = unsafe { mapped_param_index(pg_stmt, p_stmt, idx) } {
            unsafe {
                store_blob_hex_param(pg_stmt, pg_idx, val, n_bytes as usize, idx, "bind_blob64")
            };
        }
    }

    drop(guard);
    rc
}

pub(super) fn bind_text64_impl(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    mut val: *const c_char,
    mut n_bytes: u64,
    destructor: *mut c_void,
    encoding: c_uchar,
) -> c_int {
    let (pg_stmt, guard) = unsafe { begin_bind(PHASE_BIND_TEXT64, p_stmt) };
    let original_val = val;
    let replacement = unsafe {
        replacement_identifier(
            pg_stmt,
            idx,
            encoding == 1 && !val.is_null() && (n_bytes == 0),
        )
    };
    let bind_destructor = if let Some(value) = replacement.as_ref() {
        val = value.as_ptr().cast();
        n_bytes = value.len() as u64;
        usize::MAX as *mut c_void // SQLITE_TRANSIENT: SQLite must copy this local string.
    } else {
        destructor
    };
    let pg_idx = unsafe { mapped_param_index(pg_stmt, p_stmt, idx) };
    // Snapshot before SQLite or a caller destructor can consume the input.
    let snapshot = if pg_idx.is_some() && !val.is_null() && n_bytes <= c_int::MAX as u64 {
        let len = n_bytes as usize;
        Some(unsafe { std::slice::from_raw_parts(val.cast::<u8>(), len) }.to_vec())
    } else {
        None
    };

    // UTF-16 inputs retain the existing path; MATCH normalization is UTF-8 only.
    let snapshot = match if encoding == 1 {
        unsafe { normalize_match_snapshot(pg_stmt, idx, snapshot) }
    } else {
        Ok(snapshot)
    } {
        Ok(snapshot) => snapshot,
        Err(reason) => {
            log_debug_lazy!("bind_text64: unsupported MATCH parameter: {}", reason);
            if let Some(pg_idx) = pg_idx {
                unsafe {
                    free_dynamic_param_value(pg_stmt, pg_idx);
                    (*pg_stmt).param_values[pg_idx] = ptr::null_mut();
                }
            }
            if !original_val.is_null() {
                unsafe { invoke_destructor_if_custom(original_val.cast(), destructor) };
            }
            drop(guard);
            return SQLITE_ERROR;
        }
    };

    let rc = if is_pg_routed_noncached(pg_stmt) {
        if !val.is_null() && n_bytes > c_int::MAX as u64 {
            18
        } else if pg_idx.is_some() {
            SQLITE_OK
        } else {
            25
        }
    } else {
        let mut rc = get_orig_sqlite3_bind_text64()
            .map(|f| unsafe { f(p_stmt, idx, val, n_bytes, bind_destructor, encoding) })
            .unwrap_or(SQLITE_ERROR);
        unsafe {
            rc = if replacement.is_some()
                || destructor.is_null()
                || destructor as usize == usize::MAX
            {
                retry_on_misuse(rc, p_stmt, pg_stmt, || {
                    get_orig_sqlite3_bind_text64()
                        .map(|f| f(p_stmt, idx, val, n_bytes, bind_destructor, encoding))
                        .unwrap_or(SQLITE_ERROR)
                })
            } else {
                rc
            };
        }
        rc
    };

    if rc == SQLITE_OK {
        if let Some(pg_idx) = pg_idx {
            if let Some(bytes) = snapshot.as_ref() {
                unsafe {
                    store_text_param(
                        pg_stmt,
                        pg_idx,
                        bytes.as_ptr().cast(),
                        bytes.len(),
                        false,
                        idx,
                        "bind_text64_impl",
                    )
                };
            } else {
                unsafe {
                    free_dynamic_param_value(pg_stmt, pg_idx);
                    (*pg_stmt).param_values[pg_idx] = ptr::null_mut();
                }
            }
        }
    }
    // A replacement never transfers the caller's original pointer to SQLite.
    // Negative-length bind_text and NULL have no destructor obligation.
    if !original_val.is_null() && (replacement.is_some() || is_pg_routed_noncached(pg_stmt)) {
        unsafe { invoke_destructor_if_custom(original_val.cast(), destructor) };
    }

    drop(guard);
    crate::pg_mem_telemetry::rust_mem_telemetry_maybe_log();
    rc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_machine_identifier_valid() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?><Preferences MachineIdentifier="53cfd87bf8b24db2af2d6aaa373b2b34" ProcessedMachineIdentifier="53cfd87bf8b24db2af2d6aaa373b2b34" AcceptedEULA="1"/>"#;
        let uuid = parse_machine_identifier(xml);
        assert_eq!(
            uuid,
            Some("53cfd87b-f8b2-4db2-af2d-6aaa373b2b34".to_string())
        );
    }

    #[test]
    fn test_parse_machine_identifier_invalid_len() {
        let xml = r#"<Preferences MachineIdentifier="abc123" AcceptedEULA="1"/>"#;
        let uuid = parse_machine_identifier(xml);
        assert_eq!(uuid, None);
    }

    #[test]
    fn test_parse_machine_identifier_missing() {
        let xml = r#"<Preferences AcceptedEULA="1"/>"#;
        let uuid = parse_machine_identifier(xml);
        assert_eq!(uuid, None);
    }

    #[test]
    fn machine_identifier_exact_attribute_and_canonical_identity() {
        let canonical = "53CFD87B-f8b2-4db2-af2d-6aaa373b2b34";
        let xml = format!("<Preferences AnonymousMachineIdentifier='ffffffffffffffffffffffffffffffff' ProcessedMachineIdentifier='eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee' MachineIdentifier = '{canonical}'/>");
        assert_eq!(parse_machine_identifier(&xml), Some(canonical.to_owned()));
        assert_eq!(
            parse_machine_identifier(
                "<Preferences ProcessedMachineIdentifier=\"53cfd87bf8b24db2af2d6aaa373b2b34\"/>"
            ),
            None
        );
    }

    #[test]
    fn machine_identifier_rejects_nonhex_unicode_and_malformed_attributes() {
        for value in [
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
            "éééééééééééééééé",
            "53cfd87b_f8b2-4db2-af2d-6aaa373b2b34",
            "53cfd87bf8b24db2af2d6aaa373b2b34junk",
        ] {
            assert_eq!(
                parse_machine_identifier(&format!("<Preferences MachineIdentifier='{value}'/>")),
                None
            );
        }
        assert_eq!(
            parse_machine_identifier(
                "<Preferences MachineIdentifier='53cfd87bf8b24db2af2d6aaa373b2b34>"
            ),
            None
        );
        assert_eq!(
            parse_machine_identifier(
                "<!-- MachineIdentifier=\"53cfd87bf8b24db2af2d6aaa373b2b34\" --><Preferences/>"
            ),
            None
        );
    }
    #[test]
    fn identifier_bind_proves_column_and_parameter_position() {
        for (sql, idx) in [
            ("SELECT * FROM devices WHERE identifier = ?", 1),
            ("SELECT * FROM devices WHERE name = ? AND identifier = ?", 2),
            ("SELECT * FROM devices WHERE identifier = ?3", 3),
            ("SELECT * FROM devices WHERE identifier = :identifier", 1),
            (
                "SELECT * FROM devices WHERE name = :name AND identifier = :identifier",
                2,
            ),
            ("SELECT * FROM devices WHERE devices.identifier = ?", 1),
        ] {
            assert!(is_device_identifier_bind(sql, idx), "{sql}");
        }
        for (sql, idx) in [
            ("SELECT * FROM devices WHERE name = ?", 1),
            ("SELECT * FROM devices WHERE name = identifier = ?", 1),
            ("SELECT * FROM other WHERE identifier = ?; SELECT * FROM devices WHERE identifier = ?", 1),
            ("SELECT * FROM devices WHERE name = ? AND identifier = ?", 1),
            ("SELECT * FROM devices WHERE name = :identifier", 1),
            ("SELECT * FROM devices WHERE processed_identifier = ?", 1),
            ("SELECT * FROM devices WHERE identifier = ?3", 1),
            ("SELECT * FROM devices WHERE name = 'identifier = ?' AND name = ?", 1),
            ("SELECT * FROM devices WHERE name = ? /* identifier = ? */", 1),
            ("SELECT * FROM devices WHERE identifier IN (SELECT identifier FROM other WHERE identifier = ?)", 1),
            ("SELECT * FROM devices JOIN other WHERE other.identifier = ?", 1),
        ] {
            assert!(!is_device_identifier_bind(sql, idx), "{sql}");
        }
    }

    #[test]
    fn pg_text_bind_consumes_original_custom_pointer_after_copy() {
        // Other unit tests mutate and unload the process-global original SQLite
        // symbols. Exercise this ownership case in a fresh process rather than
        // allowing test-only symbol publication to race this ABI fixture.
        const CHILD: &str = "PLEX_PG_TEXT_OWNERSHIP_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "db_interpose_bind::text_blob_binds::tests::pg_text_bind_consumes_original_custom_pointer_after_copy",
                    "--test-threads=1",
                ])
                .env(CHILD, "1")
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success(), "ownership child failed: {status}");
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("ownership child exceeded its 30-second bound");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        use std::sync::atomic::{AtomicUsize, Ordering};
        static LAST: AtomicUsize = AtomicUsize::new(0);
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C" fn consume(pointer: *mut c_void) {
            LAST.store(pointer as usize, Ordering::SeqCst);
            CALLS.fetch_add(1, Ordering::SeqCst);
            libc::free(pointer);
        }
        let mut stmt = PgStmt::new();
        stmt.is_pg = 1;
        stmt.param_count = 1;
        stmt.param_values = vec![ptr::null_mut()];
        stmt.param_lengths = vec![0];
        stmt.param_formats = vec![0];
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        let mut sqlite_stmt = ptr::null_mut();
        assert_eq!(
            unsafe {
                rusqlite::ffi::sqlite3_prepare_v2(
                    connection.handle(),
                    c"SELECT ?".as_ptr(),
                    -1,
                    &mut sqlite_stmt,
                    ptr::null_mut(),
                )
            },
            SQLITE_OK
        );
        let handle = sqlite_stmt.cast::<sqlite3_stmt>();
        crate::pg_statement::c_abi::pg_register_stmt(handle, &mut stmt);
        let callback = consume as *const () as *mut c_void;
        let original = unsafe { libc::strdup(c"keep-original".as_ptr()) };
        assert_eq!(bind_text_impl(handle, 1, original, 13, callback), SQLITE_OK);
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(LAST.load(Ordering::SeqCst), original as usize);
        assert_eq!(
            unsafe { std::ffi::CStr::from_ptr(stmt.param_values[0]) }.to_bytes(),
            b"keep-original"
        );
        let invalid = unsafe { libc::strdup(c"invalid-index".as_ptr()) };
        assert_eq!(bind_text_impl(handle, 2, invalid, 13, callback), 25);
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);
        assert_eq!(LAST.load(Ordering::SeqCst), invalid as usize);
        let negative = unsafe { libc::strdup(c"negative".as_ptr()) };
        assert_eq!(bind_text_impl(handle, 1, negative, -1, callback), SQLITE_OK);
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);
        unsafe { libc::free(negative.cast()) };
        assert_eq!(
            bind_text_impl(handle, 1, ptr::null(), 0, callback),
            SQLITE_OK
        );
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);
        assert!(stmt.param_values[0].is_null());
        let oversized = unsafe { libc::strdup(c"oversized".as_ptr()) };
        assert_eq!(
            bind_text64_impl(handle, 1, oversized, u64::MAX, callback, 1),
            18
        );
        assert_eq!(CALLS.load(Ordering::SeqCst), 3);
        assert_eq!(LAST.load(Ordering::SeqCst), oversized as usize);
        assert_eq!(
            bind_text64_impl(handle, 1, ptr::null(), u64::MAX, callback, 1),
            SQLITE_OK
        );
        assert_eq!(CALLS.load(Ordering::SeqCst), 3);
        crate::pg_statement::c_abi::pg_unregister_stmt(handle);
        assert_eq!(
            unsafe { rusqlite::ffi::sqlite3_finalize(sqlite_stmt) },
            SQLITE_OK
        );
    }
    #[test]
    #[ignore = "requires isolated PostgreSQL via TEST_FTS_PG_URL"]
    fn prepared_match_bind_parity_and_destructor_ownership() {
        const CHILD: &str = "PLEX_PG_FTS_BIND_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "db_interpose_bind::text_blob_binds::tests::prepared_match_bind_parity_and_destructor_ownership", "--ignored", "--test-threads=1"])
                .env(CHILD, "1").spawn().unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success(), "{status}");
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("FTS bind child timed out");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        static LAST: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C" fn consume(pointer: *mut c_void) {
            CALLS.fetch_add(1, Ordering::SeqCst);
            LAST.store(pointer as usize, Ordering::SeqCst);
            libc::free(pointer);
        }
        // The isolated child publishes the bundled SQLite ABI for the cached
        // path so the real original binder owns the real original pointer.
        unsafe {
            crate::db_interpose_common::orig_sqlite3_bind_text = Some(std::mem::transmute::<
                *const (),
                crate::db_interpose_common::Sqlite3BindTextFn,
            >(
                rusqlite::ffi::sqlite3_bind_text as *const (),
            ));
            crate::db_interpose_common::orig_sqlite3_bind_text64 = Some(std::mem::transmute::<
                *const (),
                crate::db_interpose_common::Sqlite3BindText64Fn,
            >(
                rusqlite::ffi::sqlite3_bind_text64 as *const (),
            ));
            crate::db_interpose_common::orig_sqlite3_reset =
                Some(std::mem::transmute::<
                    *const (),
                    crate::db_interpose_common::Sqlite3StmtToIntFn,
                >(rusqlite::ffi::sqlite3_reset as *const ()));
        }
        let sqlite = rusqlite::Connection::open_in_memory().unwrap();
        sqlite.execute_batch("CREATE VIRTUAL TABLE fts4_metadata_titles USING fts4(title,title_sort,original_title,tokenize=unicode61 'remove_diacritics=0');
            INSERT INTO fts4_metadata_titles(rowid,title,title_sort,original_title) VALUES(1,'Big Bunny','Zebra',NULL),(2,NULL,'Only Sort','Bunny'),(3,'München',NULL,NULL),(4,'Unicode o東京',NULL,NULL),(5,'Big','Bunny',NULL);").unwrap();
        let mut pg =
            postgres::Client::connect(&std::env::var("TEST_FTS_PG_URL").unwrap(), postgres::NoTls)
                .unwrap();
        pg.batch_execute("CREATE TEMP TABLE fts4_metadata_titles(rowid integer,title text,title_sort text,original_title text,title_fts tsvector);
            INSERT INTO fts4_metadata_titles VALUES(1,'Big Bunny','Zebra',NULL,setweight(to_tsvector('simple','Big Bunny'),'A') || setweight(to_tsvector('simple','Zebra'),'B')),(2,NULL,'Only Sort','Bunny',setweight(to_tsvector('simple','Only Sort'),'B') || setweight(to_tsvector('simple','Bunny'),'C')),(3,'München',NULL,NULL,setweight(to_tsvector('simple','München'),'A')),(4,'Unicode o東京',NULL,NULL,setweight(to_tsvector('simple','Unicode o東京'),'A')),(5,'Big','Bunny',NULL,setweight(to_tsvector('simple','Big'),'A') || setweight(to_tsvector('simple','Bunny'),'B'));").unwrap();
        for cached in [false, true] {
            for (column, term, expected) in [
                ("title_sort", "Zeb*", vec![1]),
                ("fts4_metadata_titles", "Big Bunny", vec![1, 5]),
                ("fts4_metadata_titles", "\"Big Bunny\"", vec![1]),
                ("fts4_metadata_titles", "Absent OR Big", vec![1, 5]),
                ("original_title", "bun*", vec![2]),
                ("fts4_metadata_titles", "mün*", vec![3]),
                ("fts4_metadata_titles", "Unicode o東京", vec![4]),
            ] {
                for style in 0..4 {
                    let sql = format!("SELECT rowid FROM fts4_metadata_titles WHERE {column} MATCH ? ORDER BY rowid");
                    let sql_c = std::ffi::CString::new(sql.clone()).unwrap();
                    let translated = crate::translate(&sql).unwrap().sql;
                    let prepared = pg.prepare(&translated).unwrap();
                    let mut native = ptr::null_mut();
                    assert_eq!(
                        unsafe {
                            rusqlite::ffi::sqlite3_prepare_v2(
                                sqlite.handle(),
                                sql_c.as_ptr(),
                                -1,
                                &mut native,
                                ptr::null_mut(),
                            )
                        },
                        SQLITE_OK
                    );
                    let handle = native.cast::<sqlite3_stmt>();
                    let mut stmt = PgStmt::new();
                    stmt.is_pg = 1;
                    stmt.is_cached = i32::from(cached);
                    stmt.sql = sql_c.as_ptr().cast_mut();
                    stmt.param_count = 1;
                    stmt.param_values = vec![ptr::null_mut()];
                    stmt.param_lengths = vec![0];
                    stmt.param_formats = vec![0];
                    crate::pg_statement::c_abi::pg_register_stmt(handle, &mut stmt);
                    let value = std::ffi::CString::new(term).unwrap();
                    let custom = style == 1 || style == 3;
                    let pointer = if custom {
                        unsafe { libc::strdup(value.as_ptr()) }
                    } else {
                        value.as_ptr().cast_mut()
                    };
                    let before_calls = CALLS.load(Ordering::SeqCst);
                    let destructor = if custom {
                        consume as *const () as *mut c_void
                    } else {
                        ptr::null_mut()
                    };
                    let rc = if style >= 2 {
                        bind_text64_impl(handle, 1, pointer, term.len() as u64, destructor, 1)
                    } else {
                        bind_text_impl(handle, 1, pointer, term.len() as c_int, destructor)
                    };
                    assert_eq!(rc, SQLITE_OK, "{sql} {term}");
                    let normalized = unsafe { std::ffi::CStr::from_ptr(stmt.param_values[0]) }
                        .to_str()
                        .unwrap()
                        .to_owned();
                    assert_eq!(normalized, crate::query::convert_fts_term(term));
                    let actual: Vec<i32> = pg
                        .query(&prepared, &[&normalized])
                        .unwrap()
                        .iter()
                        .map(|row| row.get(0))
                        .collect();
                    let oracle: Vec<i32> = sqlite
                        .prepare(&sql)
                        .unwrap()
                        .query_map([term], |row| row.get(0))
                        .unwrap()
                        .collect::<Result<_, _>>()
                        .unwrap();
                    assert_eq!(actual, expected, "{sql} {term}");
                    assert_eq!(actual, oracle);
                    if cached {
                        let expanded = unsafe { rusqlite::ffi::sqlite3_expanded_sql(native) };
                        assert!(
                            unsafe { std::ffi::CStr::from_ptr(expanded) }
                                .to_str()
                                .unwrap()
                                .contains(term),
                            "SQLite input changed"
                        );
                        unsafe { rusqlite::ffi::sqlite3_free(expanded.cast()) };
                        let mut native_rows = Vec::new();
                        loop {
                            let rc = unsafe { rusqlite::ffi::sqlite3_step(native) };
                            if rc == 101 {
                                break;
                            }
                            assert_eq!(rc, 100);
                            native_rows
                                .push(unsafe { rusqlite::ffi::sqlite3_column_int(native, 0) });
                        }
                        assert_eq!(native_rows, expected);
                    }
                    crate::pg_statement::c_abi::pg_unregister_stmt(handle);
                    assert_eq!(
                        unsafe { rusqlite::ffi::sqlite3_finalize(native) },
                        SQLITE_OK
                    );
                    assert_eq!(
                        CALLS.load(Ordering::SeqCst),
                        before_calls + usize::from(custom)
                    );
                    if custom {
                        assert_eq!(LAST.load(Ordering::SeqCst), pointer as usize);
                    }
                    unsafe { free_dynamic_param_value(&mut stmt, 0) };
                }
            }
        }
        for sql in [
            "SELECT rowid FROM fts4_metadata_titles WHERE title MATCH ?1 AND title=?1",
            "SELECT rowid FROM fts4_metadata_titles WHERE title MATCH ? || '*'",
        ] {
            let sql_c = std::ffi::CString::new(sql).unwrap();
            let mut native = ptr::null_mut();
            assert_eq!(
                unsafe {
                    rusqlite::ffi::sqlite3_prepare_v2(
                        sqlite.handle(),
                        sql_c.as_ptr(),
                        -1,
                        &mut native,
                        ptr::null_mut(),
                    )
                },
                SQLITE_OK
            );
            let handle = native.cast::<sqlite3_stmt>();
            let mut stmt = PgStmt::new();
            stmt.is_pg = 1;
            stmt.sql = sql_c.as_ptr().cast_mut();
            stmt.param_count = 1;
            stmt.param_values = vec![ptr::null_mut()];
            stmt.param_lengths = vec![0];
            stmt.param_formats = vec![0];
            crate::pg_statement::c_abi::pg_register_stmt(handle, &mut stmt);
            for use_64 in [false, true] {
                let before = CALLS.load(Ordering::SeqCst);
                let original = unsafe { libc::strdup(c"Zeb*".as_ptr()) };
                let destructor = consume as *const () as *mut c_void;
                let rc = if use_64 {
                    bind_text64_impl(handle, 1, original, 4, destructor, 1)
                } else {
                    bind_text_impl(handle, 1, original, 4, destructor)
                };
                assert_eq!(rc, SQLITE_ERROR);
                assert_eq!(CALLS.load(Ordering::SeqCst), before + 1);
                assert_eq!(LAST.load(Ordering::SeqCst), original as usize);
                assert!(stmt.param_values[0].is_null());
            }
            // An ordinary SQLite-only statement retains its existing behavior.
            stmt.is_pg = 0;
            assert_eq!(
                bind_text_impl(handle, 1, c"Zeb*".as_ptr(), 4, ptr::null_mut()),
                SQLITE_OK
            );
            assert_eq!(
                unsafe { std::ffi::CStr::from_ptr(stmt.param_values[0]) }.to_bytes(),
                b"Zeb*"
            );
            crate::pg_statement::c_abi::pg_unregister_stmt(handle);
            unsafe {
                free_dynamic_param_value(&mut stmt, 0);
                rusqlite::ffi::sqlite3_finalize(native);
            }
        }
        // A regular parameter in the same statement is copied unchanged.
        let sql = std::ffi::CString::new(
            "SELECT rowid FROM fts4_metadata_titles WHERE title=? AND title_sort MATCH ?",
        )
        .unwrap();
        let mut stmt = PgStmt::new();
        stmt.is_pg = 1;
        stmt.sql = sql.as_ptr().cast_mut();
        assert_eq!(
            unsafe { normalize_match_snapshot(&mut stmt, 1, Some(b"Big Bunny".to_vec())) }
                .unwrap()
                .unwrap(),
            b"Big Bunny"
        );
        assert_eq!(
            unsafe { normalize_match_snapshot(&mut stmt, 2, Some(b"Zeb*".to_vec())) }
                .unwrap()
                .unwrap(),
            b"Zeb:*"
        );
    }

    #[test]
    fn preferences_path_selects_only_explicit_support_directory() {
        assert_eq!(
            machine_identifier_preferences_path(None),
            std::path::PathBuf::from(
                "/config/Library/Application Support/Plex Media Server/Preferences.xml"
            ),
        );
        assert_eq!(
            machine_identifier_preferences_path(Some("/tmp/runtime fixture/support".into())),
            std::path::PathBuf::from(
                "/tmp/runtime fixture/support/Plex Media Server/Preferences.xml"
            ),
        );
        // Even an explicitly empty directory stays explicit rather than
        // silently borrowing the default server's machine identity.
        assert_eq!(
            machine_identifier_preferences_path(Some("".into())),
            std::path::PathBuf::from("Plex Media Server/Preferences.xml"),
        );
    }
}
