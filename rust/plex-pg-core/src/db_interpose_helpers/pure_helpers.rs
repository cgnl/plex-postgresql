use super::*;

pub(super) fn rewrite_server_library_uri_bytes(
    input_bytes: &[u8],
    out_cap: usize,
) -> Option<Vec<u8>> {
    const SERVER_PREFIX: &[u8] = b"server://";
    const NEEDLE: &[u8] = b"/com.plexapp.plugins.library/library/";
    const REPLACEMENT: &[u8] = b"library://";

    if find_subslice(input_bytes, SERVER_PREFIX).is_none()
        || find_subslice(input_bytes, NEEDLE).is_none()
    {
        return None;
    }

    let mut out_buf = Vec::with_capacity(input_bytes.len().min(out_cap));
    let mut in_pos = 0usize;
    let mut rewrites = 0;

    while in_pos < input_bytes.len() {
        if out_buf.len() >= out_cap {
            break;
        }
        let slice = &input_bytes[in_pos..];
        let Some(rel_match) = find_subslice(slice, SERVER_PREFIX) else {
            push_capped(&mut out_buf, out_cap, slice);
            break;
        };
        let abs_match = in_pos + rel_match;
        push_capped(&mut out_buf, out_cap, &input_bytes[in_pos..abs_match]);
        in_pos = abs_match;

        let search_start = in_pos + SERVER_PREFIX.len();
        if search_start >= input_bytes.len() {
            push_capped(&mut out_buf, out_cap, &input_bytes[in_pos..]);
            break;
        }

        let tail = &input_bytes[search_start..];
        let Some(rel_lib) = find_subslice(tail, NEEDLE) else {
            push_capped(&mut out_buf, out_cap, &input_bytes[in_pos..search_start]);
            in_pos = search_start;
            continue;
        };
        let abs_lib = search_start + rel_lib;
        let lib_end = abs_lib + NEEDLE.len();

        push_capped(&mut out_buf, out_cap, REPLACEMENT);
        in_pos = lib_end;
        rewrites += 1;
    }

    if rewrites == 0 {
        None
    } else {
        Some(out_buf)
    }
}

pub(super) fn is_aggregate_alias(col: &str) -> bool {
    if col.is_empty() {
        return false;
    }
    let lower = col.trim().to_ascii_lowercase();
    matches!(lower.as_str(), "count" | "sum" | "max" | "min" | "avg")
        || ["count(", "sum(", "max(", "min(", "avg("]
            .iter()
            .any(|prefix| lower.starts_with(prefix))
}

pub(super) fn normalize_sql_literals_impl(sql: &str) -> Option<(String, Vec<String>)> {
    const MAX_NORMALIZED_PARAMS: usize = 32;

    let bytes = sql.as_bytes();
    if bytes.len() >= 6 && bytes[..6].eq_ignore_ascii_case(b"INSERT") {
        return None;
    }
    if !contains_ascii_icase(bytes, b"WHERE") {
        return None;
    }

    let mut out = String::with_capacity(sql.len() + MAX_NORMALIZED_PARAMS * 4);
    let mut params: Vec<String> = Vec::with_capacity(MAX_NORMALIZED_PARAMS);
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;

    while i < bytes.len() {
        if params.len() < MAX_NORMALIZED_PARAMS {
            let b = bytes[i];
            let is_number_start = b.is_ascii_digit()
                || (b == b'-' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit());
            if is_number_start && !in_single && !in_double {
                let prev = if i == 0 { b' ' } else { bytes[i - 1] };
                if is_prev_numeric_boundary(prev) {
                    let num_start = i;
                    if bytes[i] == b'-' {
                        i += 1;
                    }
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    if i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit() {
                        i += 1;
                        while i < bytes.len() && bytes[i].is_ascii_digit() {
                            i += 1;
                        }
                    }

                    if is_next_numeric_boundary(bytes, i) {
                        let lit = &sql[num_start..i];
                        params.push(lit.to_string());
                        out.push('$');
                        out.push_str(&(params.len()).to_string());
                        continue;
                    }
                    i = num_start;
                }
            }
        }

        let b = bytes[i];
        out.push(b as char);
        if b == b'\'' && !in_double {
            in_single = !in_single;
        } else if b == b'"' && !in_single {
            in_double = !in_double;
        }
        i += 1;
    }

    if params.is_empty() {
        return None;
    }
    Some((out, params))
}

pub(super) fn is_library_db_path_impl(path: &str) -> bool {
    let mut bytes = path.as_bytes();
    if bytes.len() > 4 && (bytes.ends_with(b"-wal") || bytes.ends_with(b"-shm")) {
        bytes = &bytes[..bytes.len() - 4];
    }
    contains_ascii_icase(bytes, b"com.plexapp.plugins.library.db")
        || contains_ascii_icase(bytes, b"com.plexapp.plugins.library.blobs.db")
}

pub(super) fn is_library_or_blobs_db_path_impl(path: &str) -> bool {
    contains_ascii_icase(path.as_bytes(), b"com.plexapp.plugins.library.db")
        || contains_ascii_icase(path.as_bytes(), b"com.plexapp.plugins.library.blobs.db")
}

pub(super) fn is_blobs_db_path_impl(path: &str) -> bool {
    contains_ascii_icase(path.as_bytes(), b"com.plexapp.plugins.library.blobs.db")
}

pub(super) fn contains_binary_bytes_impl(data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    for (i, c) in data.iter().copied().enumerate() {
        if c < 0x20 && c != 0x09 && c != 0x0A && c != 0x0D {
            return true;
        }
        if c == 0x7F || c == 0xC0 || c == 0xC1 || c >= 0xF5 {
            return true;
        }
        if i == 0 && data.len() >= 2 && c == 0x1F && data[1] == 0x8B {
            return true;
        }
    }
    false
}

pub(super) fn bytes_to_pg_hex_impl(data: &[u8]) -> String {
    if data.is_empty() {
        return String::new();
    }

    let mut out = String::with_capacity(2 + data.len() * 2);
    out.push('\\');
    out.push('x');
    for b in data {
        use std::fmt::Write;
        let _ = write!(&mut out, "{:02x}", b);
    }
    out
}

pub(super) fn is_related_items_query_impl(pg_sql: &str) -> bool {
    pg_sql.contains("taggings as related")
}

pub(super) fn should_mask_collection_metadata_type_impl(
    pg_sql: &str,
    col_name: &str,
    raw_val: i64,
) -> bool {
    raw_val == 18 && col_name.contains("metadata_type") && is_related_items_query_impl(pg_sql)
}

pub(super) fn find_insert_column_index_impl(sql: &str, column_name: &str) -> i32 {
    if column_name.is_empty() {
        return -1;
    }
    let bytes = sql.as_bytes();
    if !(contains_ascii_icase(bytes, b"INSERT") && contains_ascii_icase(bytes, b"INTO")) {
        return -1;
    }
    let Some(cols_open) = find_ascii_icase(bytes, b"(") else {
        return -1;
    };
    let Some(cols_close) = find_closing_paren(bytes, cols_open + 1) else {
        return -1;
    };
    let cols_section = &sql[cols_open + 1..cols_close];
    let cols = split_csv_simple(cols_section);
    for (i, c) in cols.iter().enumerate() {
        if normalize_ident_token(c).eq_ignore_ascii_case(column_name) {
            return i as i32;
        }
    }
    -1
}

pub(super) fn is_junk_metadata_insert_impl(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    if !(contains_ascii_icase(bytes, b"INSERT") && contains_ascii_icase(bytes, b"metadata_items")) {
        return false;
    }
    if contains_ascii_icase(bytes, b"metadata_item_settings")
        || contains_ascii_icase(bytes, b"metadata_item_views")
        || contains_ascii_icase(bytes, b"metadata_item_accounts")
        || contains_ascii_icase(bytes, b"metadata_item_clusters")
    {
        return false;
    }

    let Some(cols_open) = find_ascii_icase(bytes, b"(") else {
        return false;
    };
    let Some(cols_close) = find_closing_paren(bytes, cols_open + 1) else {
        return false;
    };
    let cols_section = &sql[cols_open + 1..cols_close];
    let cols = split_csv_simple(cols_section);
    if cols.is_empty() {
        return false;
    }

    let mut lib_idx: Option<usize> = None;
    let mut type_idx: Option<usize> = None;
    for (i, c) in cols.iter().enumerate() {
        let c = normalize_ident_token(c);
        if c.eq_ignore_ascii_case("library_section_id") {
            lib_idx = Some(i);
        }
        if c.eq_ignore_ascii_case("metadata_type") {
            type_idx = Some(i);
        }
    }
    let (Some(lib_idx), Some(type_idx)) = (lib_idx, type_idx) else {
        return false;
    };

    let Some(values_pos) = find_ascii_icase(bytes, b"VALUES") else {
        return false;
    };
    let values_bytes = &bytes[values_pos..];
    let Some(v_open_rel) = find_ascii_icase(values_bytes, b"(") else {
        return false;
    };
    let v_open = values_pos + v_open_rel;
    let Some(v_close) = find_closing_paren(bytes, v_open + 1) else {
        return false;
    };
    let values_section = &sql[v_open + 1..v_close];
    let vals = split_csv_simple(values_section);
    if lib_idx >= vals.len() || type_idx >= vals.len() {
        return false;
    }

    let lib_is_null = vals[lib_idx]
        .trim_start()
        .to_ascii_uppercase()
        .starts_with("NULL");
    let type_is_null = vals[type_idx]
        .trim_start()
        .to_ascii_uppercase()
        .starts_with("NULL");
    lib_is_null && type_is_null
}

pub(super) fn format_epoch_to_datetime_utc_impl(
    epoch: i64,
    out: *mut c_char,
    out_len: usize,
) -> c_int {
    if out.is_null() || out_len == 0 || epoch <= 0 {
        return 0;
    }

    let t = epoch as libc::time_t;
    let mut tm_utc: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::gmtime_r(&t, &mut tm_utc) };
    if ok.is_null() {
        return 0;
    }

    let fmt = b"%Y-%m-%d %H:%M:%S\0";
    let written = unsafe {
        libc::strftime(
            out as *mut libc::c_char,
            out_len,
            fmt.as_ptr() as *const libc::c_char,
            &tm_utc,
        )
    };
    i32::from(written != 0)
}
