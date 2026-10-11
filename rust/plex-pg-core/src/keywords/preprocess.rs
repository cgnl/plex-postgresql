use super::*;

pub(super) fn preprocess_sql(sql: &str) -> String {
    let sql = fix_placeholder_spacing(sql);
    let sql = rewrite_sqlite_utility_statements(&sql);
    let sql = rewrite_regexp_operator(&sql);
    let sql = rewrite_raise_function_calls(&sql);
    let sql = rewrite_or_conflict_prefix_statements(&sql);
    let sql = rewrite_limit_offset_comma_syntax(&sql);
    let sql = rewrite_update_delete_limit_statements(&sql);
    let sql = rewrite_transaction_control_statements(&sql);
    let sql = rewrite_pragma_statements(&sql);
    let sql = rewrite_sqlite_create_table_options(&sql);
    let sql = rewrite_virtual_tables(&sql);
    let sql = rewrite_glob(&sql);
    let sql = rewrite_indexed_by(&sql);
    let sql = rewrite_index_ddl_identifier_quotes(&sql);
    let sql = rewrite_sqlite_collations(&sql);
    let sql = rewrite_distinct_orderby_projection(&sql);
    rewrite_metadata_items_self_join(&sql)
}

fn rewrite_or_conflict_prefix_statements(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        let lower = t.to_ascii_lowercase();

        // UPDATE OR <algo> ...
        if lower.starts_with("update or ") {
            let tail = t["update or ".len()..].trim_start();
            let algos = ["ignore", "replace", "rollback", "abort", "fail"];
            let mut matched = false;
            for algo in algos {
                if tail.to_ascii_lowercase().starts_with(algo) {
                    let rest = tail[algo.len()..].trim_start();
                    out.push(format!("UPDATE {}", rest));
                    matched = true;
                    break;
                }
            }
            if matched {
                continue;
            }
        }

        // INSERT OR ABORT/FAIL/ROLLBACK INTO ...
        if lower.starts_with("insert or abort into ")
            || lower.starts_with("insert or fail into ")
            || lower.starts_with("insert or rollback into ")
        {
            if let Some((_, rest)) = t.split_once("INTO ") {
                out.push(format!("INSERT INTO {}", rest.trim_start()));
                continue;
            }
            if let Some((_, rest)) = t.split_once("into ") {
                out.push(format!("INSERT INTO {}", rest.trim_start()));
                continue;
            }
        }

        out.push(t.to_string());
    }
    out.join("; ")
}

/// Rewrite SQLite `UPDATE/DELETE ... [ORDER BY ...] LIMIT ...` forms to
/// PostgreSQL-compatible CTE+ctid targeting.
fn rewrite_update_delete_limit_statements(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(r) = rewrite_single_update_limit_stmt(t) {
            out.push(r);
            continue;
        }
        if let Some(r) = rewrite_single_delete_limit_stmt(t) {
            out.push(r);
            continue;
        }
        out.push(t.to_string());
    }
    out.join("; ")
}

fn rewrite_single_update_limit_stmt(stmt: &str) -> Option<String> {
    let lower = stmt.to_ascii_lowercase();
    if !lower.starts_with("update ") {
        return None;
    }

    let after_update = "update ".len();
    let set_pos = find_top_level_keyword_from(stmt, after_update, "set")?;
    let table_part = stmt[after_update..set_pos].trim();
    if table_part.is_empty() {
        return None;
    }

    let after_set = set_pos + "set".len();
    let where_pos = find_top_level_keyword_from(stmt, after_set, "where");
    let order_pos = find_top_level_keyword_from(stmt, after_set, "order by");
    let limit_pos = find_top_level_keyword_from(stmt, after_set, "limit")?;
    let returning_pos = find_top_level_keyword_from(stmt, after_set, "returning");

    let set_end = [where_pos, order_pos, Some(limit_pos), returning_pos]
        .into_iter()
        .flatten()
        .filter(|p| *p > after_set)
        .min()
        .unwrap_or(stmt.len());
    let set_part = stmt[after_set..set_end].trim();
    if set_part.is_empty() {
        return None;
    }

    let where_part = where_pos.and_then(|wp| {
        if wp >= limit_pos {
            return None;
        }
        let where_end = [order_pos, Some(limit_pos), returning_pos]
            .into_iter()
            .flatten()
            .filter(|p| *p > wp)
            .min()
            .unwrap_or(stmt.len());
        Some(stmt[wp + "where".len()..where_end].trim().to_string())
    });

    let order_part = order_pos.and_then(|op| {
        if op >= limit_pos {
            return None;
        }
        let order_end = [Some(limit_pos), returning_pos]
            .into_iter()
            .flatten()
            .filter(|p| *p > op)
            .min()
            .unwrap_or(stmt.len());
        Some(stmt[op + "order by".len()..order_end].trim().to_string())
    });

    let limit_end = returning_pos.unwrap_or(stmt.len());
    if limit_end <= limit_pos + "limit".len() {
        return None;
    }
    let limit_part = stmt[limit_pos + "limit".len()..limit_end].trim();
    if limit_part.is_empty() {
        return None;
    }

    let returning_tail = returning_pos
        .map(|rp| stmt[rp..].trim().to_string())
        .unwrap_or_default();

    let mut target_sel = format!("SELECT ctid FROM {}", table_part);
    if let Some(w) = where_part.as_deref().filter(|w| !w.is_empty()) {
        target_sel.push_str(" WHERE ");
        target_sel.push_str(w);
    }
    if let Some(o) = order_part.as_deref().filter(|o| !o.is_empty()) {
        target_sel.push_str(" ORDER BY ");
        target_sel.push_str(o);
    }
    target_sel.push_str(" LIMIT ");
    target_sel.push_str(limit_part);

    let mut out = format!(
        "WITH _plex_target AS ({}) UPDATE {} SET {} WHERE ctid IN (SELECT ctid FROM _plex_target)",
        target_sel, table_part, set_part
    );
    if !returning_tail.is_empty() {
        out.push(' ');
        out.push_str(&returning_tail);
    }
    Some(out)
}

fn rewrite_single_delete_limit_stmt(stmt: &str) -> Option<String> {
    let lower = stmt.to_ascii_lowercase();
    if !lower.starts_with("delete from ") {
        return None;
    }

    let after_delete_from = "delete from ".len();
    let where_pos = find_top_level_keyword_from(stmt, after_delete_from, "where");
    let order_pos = find_top_level_keyword_from(stmt, after_delete_from, "order by");
    let limit_pos = find_top_level_keyword_from(stmt, after_delete_from, "limit")?;
    let returning_pos = find_top_level_keyword_from(stmt, after_delete_from, "returning");

    let table_end = [where_pos, order_pos, Some(limit_pos), returning_pos]
        .into_iter()
        .flatten()
        .filter(|p| *p > after_delete_from)
        .min()
        .unwrap_or(stmt.len());
    let table_part = stmt[after_delete_from..table_end].trim();
    if table_part.is_empty() {
        return None;
    }

    let where_part = where_pos.and_then(|wp| {
        if wp >= limit_pos {
            return None;
        }
        let where_end = [order_pos, Some(limit_pos), returning_pos]
            .into_iter()
            .flatten()
            .filter(|p| *p > wp)
            .min()
            .unwrap_or(stmt.len());
        Some(stmt[wp + "where".len()..where_end].trim().to_string())
    });

    let order_part = order_pos.and_then(|op| {
        if op >= limit_pos {
            return None;
        }
        let order_end = [Some(limit_pos), returning_pos]
            .into_iter()
            .flatten()
            .filter(|p| *p > op)
            .min()
            .unwrap_or(stmt.len());
        Some(stmt[op + "order by".len()..order_end].trim().to_string())
    });

    let limit_end = returning_pos.unwrap_or(stmt.len());
    if limit_end <= limit_pos + "limit".len() {
        return None;
    }
    let limit_part = stmt[limit_pos + "limit".len()..limit_end].trim();
    if limit_part.is_empty() {
        return None;
    }

    let returning_tail = returning_pos
        .map(|rp| stmt[rp..].trim().to_string())
        .unwrap_or_default();

    let mut target_sel = format!("SELECT ctid FROM {}", table_part);
    if let Some(w) = where_part.as_deref().filter(|w| !w.is_empty()) {
        target_sel.push_str(" WHERE ");
        target_sel.push_str(w);
    }
    if let Some(o) = order_part.as_deref().filter(|o| !o.is_empty()) {
        target_sel.push_str(" ORDER BY ");
        target_sel.push_str(o);
    }
    target_sel.push_str(" LIMIT ");
    target_sel.push_str(limit_part);

    let mut out = format!(
        "WITH _plex_target AS ({}) DELETE FROM {} WHERE ctid IN (SELECT ctid FROM _plex_target)",
        target_sel, table_part
    );
    if !returning_tail.is_empty() {
        out.push(' ');
        out.push_str(&returning_tail);
    }
    Some(out)
}

/// Rewrite SQLite `LIMIT <offset>, <count>` to PostgreSQL `LIMIT <count> OFFSET <offset>`.
fn rewrite_limit_offset_comma_syntax(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        out.push(rewrite_single_limit_offset_comma_stmt(t));
    }
    out.join("; ")
}

fn rewrite_single_limit_offset_comma_stmt(stmt: &str) -> String {
    let Some(limit_start) = find_top_level_keyword(stmt, "limit") else {
        return stmt.to_string();
    };
    let limit_kw_end = limit_start + "limit".len();

    let mut i = limit_kw_end;
    let bytes = stmt.as_bytes();
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() {
        return stmt.to_string();
    }

    if find_top_level_keyword_from(stmt, i, "offset").is_some() {
        return stmt.to_string();
    }

    let clause_end =
        find_top_level_keyword_after_any(stmt, i, &["fetch", "union", "except", "intersect"])
            .unwrap_or(stmt.len());

    let mut j = i;
    let mut depth = 0usize;
    let mut comma_idx: Option<usize> = None;
    while j < clause_end {
        match bytes[j] {
            b'\'' => {
                j += 1;
                while j < clause_end {
                    if bytes[j] == b'\'' {
                        j += 1;
                        if j < clause_end && bytes[j] == b'\'' {
                            j += 1;
                            continue;
                        }
                        break;
                    }
                    j += 1;
                }
            }
            b'(' => {
                depth += 1;
                j += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                j += 1;
            }
            b',' if depth == 0 => {
                comma_idx = Some(j);
                break;
            }
            _ => j += 1,
        }
    }

    let Some(comma) = comma_idx else {
        return stmt.to_string();
    };

    let offset_expr = stmt[i..comma].trim();
    let count_expr = stmt[comma + 1..clause_end].trim();
    if offset_expr.is_empty() || count_expr.is_empty() {
        return stmt.to_string();
    }

    let mut rewritten = String::with_capacity(stmt.len() + 16);
    rewritten.push_str(&stmt[..limit_start]);
    rewritten.push_str("LIMIT ");
    rewritten.push_str(count_expr);
    rewritten.push_str(" OFFSET ");
    rewritten.push_str(offset_expr);
    if clause_end < stmt.len() {
        rewritten.push_str(&stmt[clause_end..]);
    }
    rewritten
}

fn find_top_level_keyword(stmt: &str, keyword: &str) -> Option<usize> {
    find_top_level_keyword_from(stmt, 0, keyword)
}

fn find_top_level_keyword_from(stmt: &str, start: usize, keyword: &str) -> Option<usize> {
    if start >= stmt.len() {
        return None;
    }
    let bytes = stmt.as_bytes();
    let kw = keyword.as_bytes();
    let mut i = start;
    let mut depth = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\'' {
                        i += 1;
                        if i < bytes.len() && bytes[i] == b'\'' {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'"' {
                        i += 1;
                        if i < bytes.len() && bytes[i] == b'"' {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'`' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'`' {
                        i += 1;
                        if i < bytes.len() && bytes[i] == b'`' {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'-' if i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ if depth == 0 => {
                if i + kw.len() <= bytes.len()
                    && starts_with_ascii_icase_at(bytes, i, kw)
                    && (i == 0 || !is_ident_char(bytes[i - 1]))
                    && (i + kw.len() == bytes.len() || !is_ident_char(bytes[i + kw.len()]))
                {
                    return Some(i);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

fn find_top_level_keyword_after_any(stmt: &str, start: usize, keywords: &[&str]) -> Option<usize> {
    keywords
        .iter()
        .filter_map(|kw| find_top_level_keyword_from(stmt, start, kw))
        .min()
}

/// Normalize SQLite utility statements that are unsupported or differ in PostgreSQL.
///
/// - `EXPLAIN QUERY PLAN <stmt>` -> `EXPLAIN <stmt>`
/// - `VACUUM` / `REINDEX` -> `SELECT 1` (safe no-op compatibility)
/// - `ANALYZE sqlite_*` -> `SELECT 1` (SQLite-internal analyze target)
/// - `ATTACH/DETACH DATABASE` -> `SELECT 1` (safe no-op compatibility)
fn rewrite_sqlite_utility_statements(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        let lower = t.to_ascii_lowercase();
        if lower.starts_with("explain query plan ") {
            let prefix_len = "explain query plan ".len();
            out.push(format!("EXPLAIN {}", t[prefix_len..].trim()));
            continue;
        }
        if lower == "vacuum" || lower.starts_with("vacuum ") {
            out.push("SELECT 1".to_string());
            continue;
        }
        if lower == "reindex" || lower.starts_with("reindex ") {
            out.push("SELECT 1".to_string());
            continue;
        }
        if lower.starts_with("attach database ") || lower.starts_with("detach database ") {
            out.push("SELECT 1".to_string());
            continue;
        }
        if lower.starts_with("analyze sqlite_")
            || lower.starts_with("analyze main.sqlite_")
            || lower.starts_with("analyze temp.sqlite_")
        {
            out.push("SELECT 1".to_string());
            continue;
        }
        out.push(t.to_string());
    }
    out.join("; ")
}

// Byte-oriented SQL scanners recognize ASCII syntax but must copy UTF-8 text.
// Continuation bytes have already been emitted with their leading character.
fn push_scanned_char(out: &mut String, text: &str, byte_index: usize) {
    if let Some(rest) = text.get(byte_index..) {
        if let Some(ch) = rest.chars().next() {
            out.push(ch);
        }
    }
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || !b.is_ascii()
}

fn starts_with_ascii_icase_at(haystack: &[u8], at: usize, needle: &[u8]) -> bool {
    if at + needle.len() > haystack.len() {
        return false;
    }
    haystack[at..at + needle.len()]
        .iter()
        .zip(needle.iter())
        .all(|(a, n)| a.eq_ignore_ascii_case(n))
}

/// Rewrite SQLite `REGEXP` / `NOT REGEXP` operators to PostgreSQL `~` / `!~`.
/// This is a lexical rewrite outside of quoted strings.
fn rewrite_regexp_operator(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0usize;
    let not_regexp = b"not regexp";
    let regexp = b"regexp";

    while i < bytes.len() {
        // Keep quoted strings untouched
        if bytes[i] == b'\'' {
            out.push('\'');
            i += 1;
            while i < bytes.len() {
                push_scanned_char(&mut out, sql, i);
                if bytes[i] == b'\'' {
                    i += 1;
                    if i < bytes.len() && bytes[i] == b'\'' {
                        out.push('\'');
                        i += 1;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            continue;
        }

        let prev_is_ident = i > 0 && is_ident_char(bytes[i - 1]);
        if !prev_is_ident && starts_with_ascii_icase_at(bytes, i, not_regexp) {
            let end = i + not_regexp.len();
            let next_is_ident = end < bytes.len() && is_ident_char(bytes[end]);
            if !next_is_ident {
                out.push_str("!~");
                i = end;
                continue;
            }
        }
        if !prev_is_ident && starts_with_ascii_icase_at(bytes, i, regexp) {
            let end = i + regexp.len();
            let next_is_ident = end < bytes.len() && is_ident_char(bytes[end]);
            if !next_is_ident {
                out.push('~');
                i = end;
                continue;
            }
        }

        push_scanned_char(&mut out, sql, i);
        i += 1;
    }
    out
}

/// Rewrite SQLite trigger-only `RAISE(...)` calls to `NULL` so parsing remains
/// PostgreSQL-compatible when trigger bodies are translated.
fn rewrite_raise_function_calls(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            out.push('\'');
            i += 1;
            while i < bytes.len() {
                push_scanned_char(&mut out, sql, i);
                if bytes[i] == b'\'' {
                    i += 1;
                    if i < bytes.len() && bytes[i] == b'\'' {
                        out.push('\'');
                        i += 1;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            continue;
        }

        if starts_with_ascii_icase_at(bytes, i, b"raise(") {
            let mut j = i + "raise(".len();
            let mut depth = 1usize;
            while j < bytes.len() {
                if bytes[j] == b'\'' {
                    j += 1;
                    while j < bytes.len() {
                        if bytes[j] == b'\'' {
                            j += 1;
                            if j < bytes.len() && bytes[j] == b'\'' {
                                j += 1;
                                continue;
                            }
                            break;
                        }
                        j += 1;
                    }
                    continue;
                }
                if bytes[j] == b'(' {
                    depth += 1;
                } else if bytes[j] == b')' {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        j += 1;
                        break;
                    }
                }
                j += 1;
            }
            out.push_str("NULL");
            i = j;
            continue;
        }

        push_scanned_char(&mut out, sql, i);
        i += 1;
    }
    out
}

/// Strip SQLite-only table options from CREATE TABLE statements:
/// - WITHOUT ROWID
/// - STRICT
///   including combined forms like `... ) WITHOUT ROWID, STRICT`.
fn rewrite_sqlite_create_table_options(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        let lower = t.to_ascii_lowercase();
        if !lower.starts_with("create table ") {
            out.push(t.to_string());
            continue;
        }
        let no_conflict = strip_create_table_on_conflict_clauses(t);
        out.push(strip_create_table_tail_options(&no_conflict));
    }
    out.join("; ")
}

fn strip_create_table_on_conflict_clauses(stmt: &str) -> String {
    let bytes = stmt.as_bytes();
    let mut out = String::with_capacity(stmt.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            out.push('\'');
            i += 1;
            while i < bytes.len() {
                push_scanned_char(&mut out, stmt, i);
                if bytes[i] == b'\'' {
                    i += 1;
                    if i < bytes.len() && bytes[i] == b'\'' {
                        out.push('\'');
                        i += 1;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            continue;
        }

        if starts_with_ascii_icase_at(bytes, i, b"on conflict") {
            let prev_ok = i == 0 || !is_ident_char(bytes[i - 1]);
            if prev_ok {
                let mut j = i + "on conflict".len();
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                let rest = &stmt[j..].to_ascii_lowercase();
                let actions = ["rollback", "abort", "fail", "ignore", "replace"];
                let mut matched = false;
                for action in actions {
                    if rest.starts_with(action) {
                        j += action.len();
                        matched = true;
                        break;
                    }
                }
                if matched {
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    i = j;
                    continue;
                }
            }
        }

        push_scanned_char(&mut out, stmt, i);
        i += 1;
    }
    out
}

fn strip_create_table_tail_options(stmt: &str) -> String {
    let mut s = stmt.trim().to_string();
    loop {
        let lower = s.to_ascii_lowercase();
        let trimmed = lower.trim_end();
        if trimmed.ends_with(", strict") {
            s.truncate(s.trim_end().len() - ", strict".len());
            s = s.trim_end().to_string();
            continue;
        }
        if trimmed.ends_with(", without rowid") {
            s.truncate(s.trim_end().len() - ", without rowid".len());
            s = s.trim_end().to_string();
            continue;
        }
        if trimmed.ends_with("strict") {
            s.truncate(s.trim_end().len() - "strict".len());
            s = s.trim_end().trim_end_matches(',').trim_end().to_string();
            continue;
        }
        if trimmed.ends_with("without rowid") {
            s.truncate(s.trim_end().len() - "without rowid".len());
            s = s.trim_end().trim_end_matches(',').trim_end().to_string();
            continue;
        }
        break;
    }
    s
}

/// Normalize SQLite transaction/control statement variants to PostgreSQL-friendly forms.
///
/// Examples:
/// - `BEGIN IMMEDIATE` / `BEGIN EXCLUSIVE` / `BEGIN TRANSACTION` -> `BEGIN`
/// - `END` / `END TRANSACTION` -> `COMMIT`
/// - `ROLLBACK TRANSACTION TO x` / `ROLLBACK TO x` -> `ROLLBACK TO SAVEPOINT x`
/// - `RELEASE x` -> `RELEASE SAVEPOINT x`
fn rewrite_transaction_control_statements(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        let lower = t.to_ascii_lowercase();

        let rewritten = if lower.starts_with("begin immediate")
            || lower.starts_with("begin deferred")
            || lower.starts_with("begin exclusive")
            || lower.starts_with("begin transaction")
            || lower == "begin"
        {
            "BEGIN".to_string()
        } else if lower == "end"
            || lower.starts_with("end transaction")
            || lower.starts_with("commit transaction")
        {
            "COMMIT".to_string()
        } else if let Some((_, rest)) = lower.split_once("rollback transaction to savepoint ") {
            format!("ROLLBACK TO SAVEPOINT {}", t[t.len() - rest.len()..].trim())
        } else if let Some((_, rest)) = lower.split_once("rollback transaction to ") {
            format!("ROLLBACK TO SAVEPOINT {}", t[t.len() - rest.len()..].trim())
        } else if let Some((_, rest)) = lower.split_once("rollback to savepoint ") {
            format!("ROLLBACK TO SAVEPOINT {}", t[t.len() - rest.len()..].trim())
        } else if let Some((_, rest)) = lower.split_once("rollback to ") {
            format!("ROLLBACK TO SAVEPOINT {}", t[t.len() - rest.len()..].trim())
        } else if let Some((_, rest)) = lower.split_once("release savepoint ") {
            format!("RELEASE SAVEPOINT {}", t[t.len() - rest.len()..].trim())
        } else if let Some((_, rest)) = lower.split_once("release ") {
            format!("RELEASE SAVEPOINT {}", t[t.len() - rest.len()..].trim())
        } else if let Some((_, rest)) = lower.split_once("savepoint ") {
            format!("SAVEPOINT {}", t[t.len() - rest.len()..].trim())
        } else {
            t.to_string()
        };

        out.push(rewritten);
    }
    out.join("; ")
}

// Temporary compatibility shim: keep preprocess pipeline stable while
// DISTINCT/ORDER BY projection rewrite is disabled on this branch.
fn rewrite_distinct_orderby_projection(sql: &str) -> String {
    sql.to_string()
}

/// Rewrite common PRAGMA statements to safe PostgreSQL-compatible no-op/select forms.
///
/// This keeps more behavior stable than dropping all PRAGMAs:
/// - assignment-like pragmas become `SELECT 1`
/// - read-like pragmas become a single-row `SELECT ... AS <pragma_name>`
///   Unknown pragmas are still stripped.
fn rewrite_pragma_statements(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.len() >= 6 && t.as_bytes()[..6].eq_ignore_ascii_case(b"pragma") {
            if let Some(mapped) = rewrite_single_pragma_stmt(t) {
                if !mapped.trim().is_empty() {
                    out.push(mapped);
                }
            }
            continue;
        }
        if !t.is_empty() {
            out.push(t.to_string());
        }
    }
    out.join("; ")
}

fn rewrite_single_pragma_stmt(stmt: &str) -> Option<String> {
    let original_stmt = stmt.trim().to_string();
    let mut body = stmt.trim();
    if body.len() < 6 || !body.as_bytes()[..6].eq_ignore_ascii_case(b"pragma") {
        return None;
    }
    body = body[6..].trim();
    if body.is_empty() {
        return None;
    }

    // Drop optional schema qualifier (e.g. `main.journal_mode`)
    let body = if let Some((_, rest)) = body.rsplit_once('.') {
        rest.trim()
    } else {
        body
    };

    let (name_raw, is_set, value_opt) = if let Some((lhs, rhs)) = body.split_once('=') {
        (lhs.trim(), true, Some(rhs.trim()))
    } else if let Some(open) = body.find('(') {
        if body.ends_with(')') && open < body.len() - 1 {
            (
                body[..open].trim(),
                true,
                Some(body[open + 1..body.len() - 1].trim()),
            )
        } else {
            (body.trim(), false, None)
        }
    } else {
        (body.trim(), false, None)
    };

    if name_raw.is_empty() {
        return None;
    }
    let name = name_raw
        .trim_matches('`')
        .trim_matches('"')
        .to_ascii_lowercase();
    let normalized_value = value_opt.map(|v| {
        v.trim_matches('\'')
            .trim_matches('"')
            .trim()
            .to_ascii_lowercase()
    });

    let (mapped, outcome) = match (name.as_str(), is_set) {
        // Assignment-like pragma calls: safe no-op.
        ("foreign_keys" | "case_sensitive_like", true) => {
            let _ = value_opt;
            ("SELECT 1".to_string(), "noop_set")
        }

        // busy_timeout maps best to PostgreSQL lock_timeout (session-level).
        ("busy_timeout", true) => {
            if let Some(raw) = value_opt {
                if let Ok(ms) = raw
                    .trim_matches('\'')
                    .trim_matches('"')
                    .trim()
                    .parse::<i64>()
                {
                    let clamped = ms.max(0);
                    (
                        format!("SELECT set_config('lock_timeout', '{}ms', true)", clamped),
                        "mapped_set",
                    )
                } else {
                    ("SELECT 1".to_string(), "noop_set")
                }
            } else {
                ("SELECT 1".to_string(), "noop_set")
            }
        }
        ("busy_timeout", false) => {
            (
                "SELECT current_setting('lock_timeout') AS busy_timeout".to_string(),
                "mapped_read",
            )
        }

        // foreign_keys ON/OFF has no safe direct PostgreSQL equivalent in this shim.
        ("foreign_keys", false) => ("SELECT 1 AS foreign_keys".to_string(), "mapped_read"),

        // journal_mode has no direct equivalent; keep it as session metadata.
        ("journal_mode", true) => {
            let mapped = normalized_value
                .map(|v| match v.as_str() {
                    "delete" | "truncate" | "persist" | "memory" | "wal" | "off" => v,
                    _ => String::new(),
                })
                .unwrap_or_default();
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!(
                        "SELECT set_config('plex.sqlite.journal_mode', '{}', true)",
                        mapped
                    ),
                    "mapped_set",
                )
            }
        }
        ("journal_mode", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.journal_mode', true), 'wal') AS journal_mode"
                .to_string(),
            "mapped_read",
        ),

        // synchronous maps to PostgreSQL synchronous_commit where safe.
        ("synchronous", true) => {
            let mapped = normalized_value
                .map(|v| match v.as_str() {
                    "0" | "off" => "off",
                    "1" | "normal" | "2" | "full" | "3" | "extra" | "on" => "on",
                    _ => "",
                })
                .unwrap_or("");
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!("SELECT set_config('synchronous_commit', '{}', true)", mapped),
                    "mapped_set",
                )
            }
        }
        ("synchronous", false) => {
            (
                "SELECT current_setting('synchronous_commit') AS synchronous".to_string(),
                "mapped_read",
            )
        }

        // temp_store has no direct PG equivalent; preserve intent via custom session setting.
        ("temp_store", true) => {
            let mapped = normalized_value
                .map(|v| match v.as_str() {
                    "0" | "default" => "default",
                    "1" | "file" => "file",
                    "2" | "memory" => "memory",
                    _ => "",
                })
                .unwrap_or("");
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!(
                        "SELECT set_config('plex.sqlite.temp_store', '{}', true)",
                        mapped
                    ),
                    "mapped_set",
                )
            }
        }
        ("temp_store", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.temp_store', true), 'memory') AS temp_store"
                .to_string(),
            "mapped_read",
        ),

        // cache_size has no exact PG equivalent; keep a custom session value for compatibility.
        ("cache_size", true) => {
            let mapped = normalized_value
                .filter(|v| v.parse::<i64>().is_ok())
                .unwrap_or_default();
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!(
                        "SELECT set_config('plex.sqlite.cache_size', '{}', true)",
                        mapped
                    ),
                    "mapped_set",
                )
            }
        }
        ("cache_size", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.cache_size', true), '-2000') AS cache_size"
                .to_string(),
            "mapped_read",
        ),

        // page_size has no session-level equivalent; keep compatibility metadata.
        ("page_size", true) => {
            let mapped = normalized_value
                .filter(|v| v.parse::<i64>().is_ok())
                .unwrap_or_default();
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!("SELECT set_config('plex.sqlite.page_size', '{}', true)", mapped),
                    "mapped_set",
                )
            }
        }
        ("page_size", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.page_size', true), '4096') AS page_size"
                .to_string(),
            "mapped_read",
        ),

        // auto_vacuum has no session-level equivalent; keep compatibility metadata.
        ("auto_vacuum", true) => {
            let mapped = normalized_value
                .map(|v| match v.as_str() {
                    "0" | "none" => "none",
                    "1" | "full" => "full",
                    "2" | "incremental" => "incremental",
                    _ => "",
                })
                .unwrap_or("");
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!(
                        "SELECT set_config('plex.sqlite.auto_vacuum', '{}', true)",
                        mapped
                    ),
                    "mapped_set",
                )
            }
        }
        ("auto_vacuum", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.auto_vacuum', true), 'none') AS auto_vacuum"
                .to_string(),
            "mapped_read",
        ),

        // locking_mode has no direct PG equivalent; persist as session metadata.
        ("locking_mode", true) => {
            let mapped = normalized_value
                .map(|v| match v.as_str() {
                    "normal" => "normal",
                    "exclusive" => "exclusive",
                    _ => "",
                })
                .unwrap_or("");
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!(
                        "SELECT set_config('plex.sqlite.locking_mode', '{}', true)",
                        mapped
                    ),
                    "mapped_set",
                )
            }
        }
        ("locking_mode", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.locking_mode', true), 'normal') AS locking_mode"
                .to_string(),
            "mapped_read",
        ),

        // wal_autocheckpoint has no PG analog at session-level; persist as compatibility metadata.
        ("wal_autocheckpoint", true) => {
            let mapped = normalized_value
                .filter(|v| v.parse::<i64>().is_ok())
                .unwrap_or_default();
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!(
                        "SELECT set_config('plex.sqlite.wal_autocheckpoint', '{}', true)",
                        mapped
                    ),
                    "mapped_set",
                )
            }
        }
        ("wal_autocheckpoint", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.wal_autocheckpoint', true), '1000') AS wal_autocheckpoint"
                .to_string(),
            "mapped_read",
        ),

        // mmap_size has no PG analog at session-level; persist as compatibility metadata.
        ("mmap_size", true) => {
            let mapped = normalized_value
                .filter(|v| v.parse::<i64>().is_ok())
                .unwrap_or_default();
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!(
                        "SELECT set_config('plex.sqlite.mmap_size', '{}', true)",
                        mapped
                    ),
                    "mapped_set",
                )
            }
        }
        ("mmap_size", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.mmap_size', true), '0') AS mmap_size"
                .to_string(),
            "mapped_read",
        ),
        ("case_sensitive_like", false) => ("SELECT 0 AS case_sensitive_like".to_string(), "mapped_read"),

        // Unknown pragma: strip, unless strict mode is enabled.
        _ if pragma_strict_mode_enabled() => (STRICT_PRAGMA_ERROR_SQL.to_string(), "strict_fail"),
        _ => (String::new(), "stripped"),
    };
    pragma_trace_mapping(&original_stmt, &name, &mapped, outcome);
    Some(mapped)
}

fn pragma_trace_enabled() -> bool {
    env_utils::env_string("PLEX_PG_TRACE_PRAGMA")
        .map(|v| {
            let s = v.trim().to_ascii_lowercase();
            matches!(s.as_str(), "1" | "true" | "yes" | "on" | "debug")
        })
        .unwrap_or(false)
}

fn pragma_metrics_enabled() -> bool {
    env_utils::env_string("PLEX_PG_TRACE_PRAGMA_METRICS")
        .map(|v| {
            let s = v.trim().to_ascii_lowercase();
            matches!(s.as_str(), "1" | "true" | "yes" | "on" | "debug")
        })
        .unwrap_or(false)
}

fn pragma_strict_mode_enabled() -> bool {
    #[cfg(test)]
    {
        match TEST_STRICT_PRAGMA_OVERRIDE.load(Ordering::Relaxed) {
            0 => return false,
            1 => return true,
            _ => {}
        }
    }
    env_utils::env_string("PLEX_PG_STRICT_PRAGMA")
        .map(|v| {
            let s = v.trim().to_ascii_lowercase();
            matches!(s.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

fn pragma_trace_mapping(
    original_stmt: &str,
    pragma_name: &str,
    rewritten_sql: &str,
    outcome: &str,
) {
    match outcome {
        "mapped_set" => {
            PRAGMA_MAPPED_SET.fetch_add(1, Ordering::Relaxed);
        }
        "mapped_read" => {
            PRAGMA_MAPPED_READ.fetch_add(1, Ordering::Relaxed);
        }
        "noop_set" => {
            PRAGMA_NOOP_SET.fetch_add(1, Ordering::Relaxed);
        }
        "strict_fail" => {
            PRAGMA_STRICT_FAIL.fetch_add(1, Ordering::Relaxed);
        }
        _ => {
            PRAGMA_STRIPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
    let total = PRAGMA_TOTAL.fetch_add(1, Ordering::Relaxed) + 1;

    if pragma_trace_enabled() {
        if rewritten_sql.trim().is_empty() {
            eprintln!(
                "[PRAGMA_MAP] {} [{}:{}] -> <stripped>",
                original_stmt, pragma_name, outcome
            );
        } else {
            eprintln!(
                "[PRAGMA_MAP] {} [{}:{}] -> {}",
                original_stmt, pragma_name, outcome, rewritten_sql
            );
        }
    }

    if pragma_metrics_enabled() && total.is_multiple_of(50) {
        eprintln!(
            "[PRAGMA_MAP] stats total={} mapped_set={} mapped_read={} noop_set={} stripped={} strict_fail={}",
            total,
            PRAGMA_MAPPED_SET.load(Ordering::Relaxed),
            PRAGMA_MAPPED_READ.load(Ordering::Relaxed),
            PRAGMA_NOOP_SET.load(Ordering::Relaxed),
            PRAGMA_STRIPPED.load(Ordering::Relaxed),
            PRAGMA_STRICT_FAIL.load(Ordering::Relaxed),
        );
    }
}

/// Rewrite SQLite virtual table DDL to PostgreSQL-compatible CREATE TABLE.
///
/// - `CREATE VIRTUAL TABLE x USING fts5(a,b)` ->
///   `CREATE TABLE x (id BIGSERIAL PRIMARY KEY, a TEXT, b TEXT, _fts TSVECTOR)`
///
/// - `CREATE VIRTUAL TABLE x USING rtree(id,minX,maxX,...)` ->
///   `CREATE TABLE x (id DOUBLE PRECISION, minX DOUBLE PRECISION, ...)`
fn rewrite_virtual_tables(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(rewritten) = rewrite_single_virtual_table_stmt(t) {
            out.push(rewritten);
        } else {
            out.push(t.to_string());
        }
    }
    out.join("; ")
}

fn rewrite_single_virtual_table_stmt(stmt: &str) -> Option<String> {
    let lower = stmt.to_ascii_lowercase();
    let prefix = "create virtual table ";
    if !lower.starts_with(prefix) {
        return None;
    }

    let using_fts = " using fts5(";
    let using_rtree = " using rtree(";
    let (using_idx, mode, using_len) = if let Some(idx) = lower.find(using_fts) {
        (idx, "fts5", using_fts.len())
    } else {
        let idx = lower.find(using_rtree)?;
        (idx, "rtree", using_rtree.len())
    };

    let table_name = stmt[prefix.len()..using_idx].trim();
    if table_name.is_empty() {
        return None;
    }

    let cols_start = using_idx + using_len;
    let cols_end = stmt.rfind(')')?;
    if cols_end < cols_start {
        return None;
    }
    let cols_raw = &stmt[cols_start..cols_end];
    let cols = split_csv_top_level(cols_raw);

    let mut defs: Vec<String> = Vec::new();
    if mode == "fts5" {
        defs.push("id BIGSERIAL PRIMARY KEY".to_string());
        for c in cols {
            let col = extract_ident_token(&c);
            if col.is_empty() || col.to_ascii_lowercase().starts_with("tokenize") {
                continue;
            }
            defs.push(format!("{} TEXT", col));
        }
        defs.push("_fts TSVECTOR".to_string());
    } else {
        for c in cols {
            let col = extract_ident_token(&c);
            if col.is_empty() {
                continue;
            }
            defs.push(format!("{} DOUBLE PRECISION", col));
        }
    }

    if defs.is_empty() {
        return None;
    }
    Some(format!("CREATE TABLE {} ({})", table_name, defs.join(", ")))
}

fn split_sql_statements(sql: &str) -> Vec<String> {
    // SQLite trigger bodies contain internal semicolons inside BEGIN...END blocks.
    // Treat CREATE TRIGGER statements as a single unit to avoid splitting mid-body.
    if sql.to_ascii_lowercase().contains("create trigger") {
        let t = sql.trim();
        if t.is_empty() {
            return Vec::new();
        }
        return vec![t.to_string()];
    }

    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;

    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        match ch {
            '\'' if !in_double => {
                cur.push(ch);
                if in_single && i + 1 < chars.len() && chars[i + 1] == '\'' {
                    cur.push(chars[i + 1]);
                    i += 1;
                } else {
                    in_single = !in_single;
                }
            }
            '"' if !in_single => {
                cur.push(ch);
                if in_double && i + 1 < chars.len() && chars[i + 1] == '"' {
                    cur.push(chars[i + 1]);
                    i += 1;
                } else {
                    in_double = !in_double;
                }
            }
            ';' if !in_single && !in_double => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(ch),
        }
        i += 1;
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn rewrite_index_ddl_identifier_quotes(sql: &str) -> String {
    let mut out = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        out.push(rewrite_single_index_ddl_identifier_quotes(t));
    }
    out.join("; ")
}

fn rewrite_single_index_ddl_identifier_quotes(stmt: &str) -> String {
    let start = skip_leading_ws_and_sql_comments_offset(stmt);
    let core = &stmt[start..];
    if core.is_empty() {
        return stmt.to_string();
    }

    if is_create_index_stmt(core) {
        let rewritten = rewrite_create_index_identifier_quotes_core(core);
        if rewritten != core {
            return format!("{}{}", &stmt[..start], rewritten);
        }
        return stmt.to_string();
    }

    if is_drop_index_stmt(core) {
        let rewritten = rewrite_drop_index_identifier_quotes_core(core);
        if rewritten != core {
            return format!("{}{}", &stmt[..start], rewritten);
        }
    }

    stmt.to_string()
}

fn is_create_index_stmt(stmt: &str) -> bool {
    let lower = stmt.to_ascii_lowercase();
    lower.starts_with("create index") || lower.starts_with("create unique index")
}

fn is_drop_index_stmt(stmt: &str) -> bool {
    stmt.to_ascii_lowercase().starts_with("drop index")
}

fn skip_leading_ws_and_sql_comments_offset(stmt: &str) -> usize {
    let bytes = stmt.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 < bytes.len() {
                i += 2;
            }
            continue;
        }
        break;
    }
    i
}

fn skip_ascii_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn consume_keyword_ci(stmt: &str, i: usize, keyword: &str) -> Option<usize> {
    let bytes = stmt.as_bytes();
    let start = skip_ascii_ws(bytes, i);
    let end = start + keyword.len();
    if end > bytes.len() || !starts_with_ascii_icase_at(bytes, start, keyword.as_bytes()) {
        return None;
    }
    if start > 0 && is_ident_char(bytes[start - 1]) {
        return None;
    }
    if end < bytes.len() && is_ident_char(bytes[end]) {
        return None;
    }
    Some(end)
}

fn find_token_end(stmt: &str, start: usize) -> usize {
    let bytes = stmt.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == quote {
                        i += 1;
                        if i < bytes.len() && bytes[i] == quote {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b if b.is_ascii_whitespace() => break,
            _ => i += 1,
        }
    }
    i
}

fn find_next_unquoted_char(stmt: &str, start: usize, target: u8) -> Option<usize> {
    let bytes = stmt.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == quote {
                        i += 1;
                        if i < bytes.len() && bytes[i] == quote {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b if b == target => return Some(i),
            _ => i += 1,
        }
    }
    None
}

fn find_matching_paren(stmt: &str, open_pos: usize) -> Option<usize> {
    let bytes = stmt.as_bytes();
    if open_pos >= bytes.len() || bytes[open_pos] != b'(' {
        return None;
    }

    let mut depth = 0usize;
    let mut i = open_pos;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == quote {
                        i += 1;
                        if i < bytes.len() && bytes[i] == quote {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }

    None
}

fn parse_quoted_token(stmt: &str, start: usize) -> Option<(char, String, usize)> {
    let bytes = stmt.as_bytes();
    if start >= bytes.len() {
        return None;
    }

    let quote = bytes[start] as char;
    if quote != '\'' && quote != '"' && quote != '`' {
        return None;
    }

    let mut out = String::new();
    let mut i = start + 1;
    while i < bytes.len() {
        let ch = bytes[i] as char;
        if ch == quote {
            i += 1;
            if i < bytes.len() && bytes[i] as char == quote {
                out.push(quote);
                i += 1;
                continue;
            }
            return Some((quote, out, i));
        }
        push_scanned_char(&mut out, stmt, i);
        i += 1;
    }

    None
}

fn is_simple_identifier(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn rewrite_identifier_quotes_in_segment(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = String::with_capacity(segment.len());
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'\'' || bytes[i] == b'`' {
            if let Some((_, content, next)) = parse_quoted_token(segment, i) {
                if is_simple_identifier(&content) {
                    out.push('"');
                    out.push_str(&content);
                    out.push('"');
                    i = next;
                    continue;
                }
            }
        }

        push_scanned_char(&mut out, segment, i);
        i += 1;
    }

    out
}

fn rewrite_last_quoted_identifier_token(segment: &str) -> String {
    let trimmed_end = segment
        .trim_end_matches(|c: char| c.is_ascii_whitespace())
        .len();
    if trimmed_end == 0 {
        return segment.to_string();
    }

    let token_start = segment[..trimmed_end]
        .rfind(|c: char| c.is_ascii_whitespace())
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let token = &segment[token_start..trimmed_end];
    let rewritten = rewrite_identifier_quotes_in_segment(token);
    if rewritten == token {
        return segment.to_string();
    }

    format!(
        "{}{}{}",
        &segment[..token_start],
        rewritten,
        &segment[trimmed_end..]
    )
}

fn rewrite_index_column_item(item: &str) -> String {
    let trimmed = item.trim_start();
    let leading = &item[..item.len() - trimmed.len()];
    let bytes = trimmed.as_bytes();
    if bytes.is_empty() || (bytes[0] != b'\'' && bytes[0] != b'`') {
        return item.to_string();
    }

    let Some((_, content, next)) = parse_quoted_token(trimmed, 0) else {
        return item.to_string();
    };
    if !is_simple_identifier(&content) {
        return item.to_string();
    }

    let rest = &trimmed[next..];
    let rest_trimmed = rest.trim_start();
    let allowed_suffix = rest_trimmed.is_empty()
        || rest_trimmed
            .split_whitespace()
            .next()
            .map(|kw| {
                kw.eq_ignore_ascii_case("collate")
                    || kw.eq_ignore_ascii_case("asc")
                    || kw.eq_ignore_ascii_case("desc")
                    || kw.eq_ignore_ascii_case("nulls")
            })
            .unwrap_or(false);
    if !allowed_suffix {
        return item.to_string();
    }

    format!("{leading}\"{content}\"{rest}")
}

fn rewrite_index_column_list(columns: &str) -> String {
    split_csv_top_level(columns)
        .into_iter()
        .map(|item| rewrite_index_column_item(&item))
        .collect::<Vec<_>>()
        .join(", ")
}

fn rewrite_create_index_identifier_quotes_core(stmt: &str) -> String {
    let bytes = stmt.as_bytes();
    let mut pos = 0usize;
    let Some(next) = consume_keyword_ci(stmt, pos, "create") else {
        return stmt.to_string();
    };
    pos = next;
    if let Some(next) = consume_keyword_ci(stmt, pos, "unique") {
        pos = next;
    }
    let Some(next) = consume_keyword_ci(stmt, pos, "index") else {
        return stmt.to_string();
    };
    pos = next;
    if let Some(next_if) = consume_keyword_ci(stmt, pos, "if") {
        if let Some(next_not) = consume_keyword_ci(stmt, next_if, "not") {
            if let Some(next_exists) = consume_keyword_ci(stmt, next_not, "exists") {
                pos = next_exists;
            }
        }
    }

    let name_start = skip_ascii_ws(bytes, pos);
    if name_start >= bytes.len() {
        return stmt.to_string();
    }
    let name_end = find_token_end(stmt, name_start);
    let Some(on_pos) = find_top_level_keyword_from(stmt, name_end, "on") else {
        return stmt.to_string();
    };

    let table_start = skip_ascii_ws(bytes, on_pos + 2);
    let Some(open_paren) = find_next_unquoted_char(stmt, table_start, b'(') else {
        return stmt.to_string();
    };
    let Some(close_paren) = find_matching_paren(stmt, open_paren) else {
        return stmt.to_string();
    };

    let mut out = String::with_capacity(stmt.len());
    out.push_str(&stmt[..name_start]);
    out.push_str(&rewrite_last_quoted_identifier_token(
        &stmt[name_start..name_end],
    ));
    out.push_str(&stmt[name_end..table_start]);
    out.push_str(&rewrite_identifier_quotes_in_segment(
        &stmt[table_start..open_paren],
    ));
    out.push('(');
    out.push_str(&rewrite_index_column_list(
        &stmt[open_paren + 1..close_paren],
    ));
    out.push_str(&stmt[close_paren..]);
    out
}

fn rewrite_drop_index_identifier_quotes_core(stmt: &str) -> String {
    let bytes = stmt.as_bytes();
    let mut pos = 0usize;
    let Some(next) = consume_keyword_ci(stmt, pos, "drop") else {
        return stmt.to_string();
    };
    pos = next;
    let Some(next) = consume_keyword_ci(stmt, pos, "index") else {
        return stmt.to_string();
    };
    pos = next;
    if let Some(next_if) = consume_keyword_ci(stmt, pos, "if") {
        if let Some(next_exists) = consume_keyword_ci(stmt, next_if, "exists") {
            pos = next_exists;
        }
    }

    let name_start = skip_ascii_ws(bytes, pos);
    if name_start >= bytes.len() {
        return stmt.to_string();
    }
    let name_end = find_token_end(stmt, name_start);

    let mut out = String::with_capacity(stmt.len());
    out.push_str(&stmt[..name_start]);
    out.push_str(&rewrite_last_quoted_identifier_token(
        &stmt[name_start..name_end],
    ));
    out.push_str(&stmt[name_end..]);
    out
}

fn split_csv_top_level(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        match ch {
            '\'' if !in_double => {
                cur.push(ch);
                if in_single && i + 1 < chars.len() && chars[i + 1] == '\'' {
                    cur.push(chars[i + 1]);
                    i += 1;
                } else {
                    in_single = !in_single;
                }
            }
            '"' if !in_single => {
                cur.push(ch);
                if in_double && i + 1 < chars.len() && chars[i + 1] == '"' {
                    cur.push(chars[i + 1]);
                    i += 1;
                } else {
                    in_double = !in_double;
                }
            }
            '(' if !in_single && !in_double => {
                depth += 1;
                cur.push(ch);
            }
            ')' if !in_single && !in_double => {
                depth = depth.saturating_sub(1);
                cur.push(ch);
            }
            ',' if !in_single && !in_double && depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(ch),
        }
        i += 1;
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn extract_ident_token(raw: &str) -> String {
    let t = raw.trim();
    if t.is_empty() {
        return String::new();
    }
    let tok = t.split_whitespace().next().unwrap_or("");
    tok.trim_matches(',').to_string()
}

// Plex self-join compatibility:
//
// Plex emits two related query shapes that PostgreSQL rejects:
//
// Shape A – metadata_item_settings as root, metadata_items joined twice (one aliased, one NOT):
//   SELECT ... FROM metadata_item_settings
//   JOIN metadata_items AS parents  ON parents.id = metadata_items.parent_id
//   JOIN metadata_items             ON metadata_items.id = metadata_item_settings.metadata_item_id
//   WHERE metadata_items.col = ...
//
//   Problem: `metadata_items` appears as alias-target (`AS parents`) AND as an
//   unaliased join + bare column qualifier. PostgreSQL raises:
//     "invalid reference to FROM-clause entry for table metadata_items"
//
//   Fix: add `AS mi` to the unaliased join and replace every
//        `metadata_items.<col>` reference with `mi.<col>`.
//
// Shape B – metadata_items as root, self-joined via parents / grandparents:
//   SELECT metadata_items.id FROM metadata_items
//   JOIN metadata_items AS parents      ON parents.id = metadata_items.parent_id
//   JOIN metadata_items AS grandparents ON grandparents.id = parents.parent_id
//   WHERE metadata_items.library_section_id IN (...)
//   ORDER BY grandparents.title_sort ...
//
//   In this shape the base table IS `metadata_items` (unaliased), so bare
//   `metadata_items.col` refs are valid in PostgreSQL.  Nothing to rewrite.
//   (The COLLATE icu_root / parse-error is handled by rewrite_sqlite_collations
//   which runs before this function.)
fn rewrite_metadata_items_self_join(sql: &str) -> String {
    let lower = sql.to_ascii_lowercase();

    // ── Shape A detection ──────────────────────────────────────────────────────
    // Plex emits queries like:
    //   SELECT metadata_items.id FROM metadata_item_settings
    //   JOIN metadata_items AS parents ON parents.id = metadata_items.parent_id
    //   JOIN metadata_items ON metadata_items.id = metadata_item_settings.metadata_item_id
    //   WHERE metadata_items.col = ...
    //
    // PostgreSQL rejects this because `metadata_items` is both an alias target
    // and used as a bare qualifier.
    //
    // Fix strategy: reorder the joins so the unaliased metadata_items join comes
    // FIRST (renamed to AS mi), then the aliased joins (parents, etc.) can refer
    // to mi.col in their ON clauses. Then replace metadata_items.col → mi.col
    // throughout SELECT/WHERE/HAVING/ORDER BY.
    if lower.contains("from metadata_item_settings") && lower.contains("join metadata_items") {
        let total_joins = count_word_occurrences(&lower, "join metadata_items");
        let aliased_joins = count_word_occurrences(&lower, "join metadata_items as ");
        let unaliased_joins = total_joins.saturating_sub(aliased_joins);

        if unaliased_joins > 0 && aliased_joins > 0 {
            return reorder_and_alias_self_join(sql);
        }
    }

    sql.to_string()
}

/// Rewrite Shape A self-join query by:
/// 1. Extracting the unaliased "JOIN metadata_items ON ..." fragment
/// 2. Placing it first in the join list as "JOIN metadata_items AS mi ON ..."
///    (rewriting its ON clause: metadata_items.col → mi.col)
/// 3. Rewriting the ON clauses of subsequent aliased joins: metadata_items.col → mi.col
/// 4. Replacing all remaining metadata_items.col references in SELECT/WHERE/etc.
fn reorder_and_alias_self_join(sql: &str) -> String {
    let lower = sql.to_ascii_lowercase();

    // Split into: prefix (everything up to the first JOIN), list of join fragments,
    // and suffix (WHERE + rest after all JOINs).
    //
    // We tokenise by " join " (case-insensitive).
    // The prefix is the part before the first join.
    let join_needle = " join ";
    let first_join = lower.find(join_needle);
    if first_join.is_none() {
        return sql.to_string();
    }
    let first_join_pos = first_join.unwrap();

    // prefix: "SELECT ... FROM metadata_item_settings"
    let prefix = &sql[..first_join_pos];

    // Split the rest into join fragments. Each fragment starts after " join " and
    // ends just before the next " join " (or at WHERE/end).
    let rest = &sql[first_join_pos + join_needle.len()..];
    let rest_lower = &lower[first_join_pos + join_needle.len()..];

    // Find WHERE, HAVING, ORDER BY, LIMIT, GROUP BY at top level (not inside parens)
    let suffix_start = find_top_level_clause(rest_lower);
    let joins_str = &rest[..suffix_start];
    let suffix = &rest[suffix_start..];

    // Split joins_str into individual join fragments by " join "
    let mut join_fragments: Vec<String> = Vec::new();
    let joins_lower = joins_str.to_ascii_lowercase();
    let mut pos = 0usize;
    loop {
        match joins_lower[pos..].find(join_needle) {
            None => {
                join_fragments.push(joins_str[pos..].to_string());
                break;
            }
            Some(rel) => {
                join_fragments.push(joins_str[pos..pos + rel].to_string());
                pos += rel + join_needle.len();
            }
        }
    }

    // Identify which fragment(s) are unaliased metadata_items joins.
    // An unaliased fragment starts with "metadata_items " or "metadata_items\n"
    // but NOT "metadata_items as".
    let mut unaliased_idxs: Vec<usize> = Vec::new();
    let mut aliased_idxs: Vec<usize> = Vec::new();
    for (idx, frag) in join_fragments.iter().enumerate() {
        let fl = frag.to_ascii_lowercase();
        let fl = fl.trim_start();
        if let Some(after) = fl.strip_prefix("metadata_items") {
            let after = after.trim_start();
            if after.starts_with("as ") || after.starts_with("as\t") {
                aliased_idxs.push(idx);
            } else {
                unaliased_idxs.push(idx);
            }
        } else {
            aliased_idxs.push(idx);
        }
    }

    if unaliased_idxs.is_empty() {
        return sql.to_string();
    }

    // Build the new join list: unaliased first (as mi), then aliased
    let mut new_joins: Vec<String> = Vec::new();

    for &idx in &unaliased_idxs {
        let frag = &join_fragments[idx];
        // Strip leading "metadata_items" and get the rest (ON clause etc.)
        let rest_of_frag = &frag
            [frag.to_ascii_lowercase().find("metadata_items").unwrap() + "metadata_items".len()..];
        // Rewrite metadata_items.col → mi.col in the ON clause of this fragment
        let on_fixed = rest_of_frag.replace("metadata_items.", "mi.");
        new_joins.push(format!("metadata_items AS mi{}", on_fixed));
    }

    for &idx in &aliased_idxs {
        let frag = &join_fragments[idx];
        // In aliased join ON clauses, replace metadata_items.col → mi.col
        let fixed = frag.replace("metadata_items.", "mi.");
        new_joins.push(fixed);
    }

    // Reconstruct: prefix + JOIN fragment1 JOIN fragment2 ... + suffix
    let joins_part = new_joins.join(" JOIN ");
    let reconstructed = format!("{} JOIN {} {}", prefix, joins_part, suffix);

    // Finally replace remaining metadata_items.col in SELECT/WHERE/etc.
    replace_metadata_items_refs(&reconstructed, "mi")
}

/// Find the byte offset in `s` (lowercase) where a top-level SQL clause starts
/// (WHERE, HAVING, ORDER BY, GROUP BY, LIMIT, UNION). Returns s.len() if none found.
fn find_top_level_clause(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b'\'' => {
                // skip string literal
                i += 1;
                while i < bytes.len() && bytes[i] != b'\'' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            }
            _ if depth == 0 => {
                for kw in &[
                    "where ",
                    "having ",
                    "order by ",
                    "group by ",
                    "limit ",
                    "union ",
                ] {
                    if starts_with_ascii_icase_at(bytes, i, kw.as_bytes()) {
                        return i;
                    }
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    s.len()
}

/// Count non-overlapping occurrences of `needle` in `haystack` (both lowercase).
fn count_word_occurrences(haystack: &str, needle: &str) -> usize {
    let mut count = 0;
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        count += 1;
        start += pos + needle.len();
    }
    count
}

/// Replace every occurrence of `metadata_items.<identifier>` with
/// `<new_alias>.<identifier>` throughout the SQL, being careful not to
/// touch occurrences inside quoted string literals.
fn replace_metadata_items_refs(sql: &str, new_alias: &str) -> String {
    let needle = "metadata_items.";
    let needle_lower = needle.to_ascii_lowercase();
    let sql_lower = sql.to_ascii_lowercase();

    let mut result = String::with_capacity(sql.len());
    let mut i = 0usize;
    let bytes = sql.as_bytes();

    while i < bytes.len() {
        // Skip single-quoted string literals verbatim
        if bytes[i] == b'\'' {
            result.push('\'');
            i += 1;
            while i < bytes.len() {
                let ch = bytes[i];
                push_scanned_char(&mut result, sql, i);
                i += 1;
                if ch == b'\'' {
                    // escaped '' or end of string
                    if i < bytes.len() && bytes[i] == b'\'' {
                        result.push('\'');
                        i += 1;
                    } else {
                        break;
                    }
                }
            }
            continue;
        }

        // Check for "metadata_items." at current position
        if starts_with_ascii_icase_at(sql_lower.as_bytes(), i, needle_lower.as_bytes()) {
            // Emit the alias instead
            result.push_str(new_alias);
            result.push('.');
            i += needle.len();
            continue;
        }

        push_scanned_char(&mut result, sql, i);
        i += 1;
    }

    result
}

/// Fix `?identifier` placeholders that sqlparser can't handle.
/// SQLite allows `?left` (positional placeholder followed by identifier without space),
/// but sqlparser chokes on it because `left` is a keyword.
/// We strip the trailing identifier part: `?left` → `?`.
fn fix_placeholder_spacing(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut result = String::with_capacity(sql.len());
    let mut i = 0;
    let mut in_string = false;
    let mut string_char: u8 = 0;

    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            push_scanned_char(&mut result, sql, i);
            if b == string_char {
                if i + 1 < bytes.len() && bytes[i + 1] == string_char {
                    result.push(bytes[i + 1] as char);
                    i += 2;
                    continue;
                }
                in_string = false;
            }
            i += 1;
            continue;
        }

        if b == b'\'' || b == b'"' {
            in_string = true;
            string_char = b;
            push_scanned_char(&mut result, sql, i);
            i += 1;
            continue;
        }

        if b == b'?' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_alphabetic() {
            // `?keyword` — SQLite allows a placeholder immediately followed by a SQL keyword
            // with no space (e.g. `=?group by`). sqlparser fails to parse this.
            // Read the word that follows the `?` and check if it's a SQL keyword.
            // If it is, insert a space so the keyword is recognised: `?group` → `? group`.
            // If it's just an identifier suffix (e.g. `?left` used as a named param),
            // strip the identifier to keep it as a single `?`.
            let word_start = i + 1;
            let mut word_end = word_start;
            while word_end < bytes.len()
                && (bytes[word_end].is_ascii_alphanumeric() || bytes[word_end] == b'_')
            {
                word_end += 1;
            }
            let word = std::str::from_utf8(&bytes[word_start..word_end])
                .unwrap_or("")
                .to_ascii_uppercase();
            // SQL clause/operator keywords that can legally follow a placeholder value
            // but are never valid SQLite named-parameter suffixes.
            // Excludes things like LEFT/RIGHT/JOIN/FROM which Plex uses as param names.
            const SQL_KEYWORDS: &[&str] = &[
                "GROUP",
                "ORDER",
                "HAVING",
                "LIMIT",
                "UNION",
                "EXCEPT",
                "INTERSECT",
                "WHERE",
                "AND",
                "OR",
                "NOT",
                "BETWEEN",
                "GLOB",
                "THEN",
                "ELSE",
                "END",
                "WHEN",
                "CASE",
            ];
            result.push('?');
            if SQL_KEYWORDS.contains(&word.as_str()) {
                // Insert space so the keyword is separately tokenised
                result.push(' ');
                i += 1; // just skip the `?`, keep the keyword as-is
            } else {
                // Non-keyword suffix: strip it (was a SQLite named param like ?left)
                i = word_end;
            }
            continue;
        }

        push_scanned_char(&mut result, sql, i);
        i += 1;
    }
    result
}

/// Replace `GLOB '<pattern>'` with `ILIKE '<pg_pattern>'`.
/// Uses a simple state machine to avoid touching string literals.
fn rewrite_glob(sql: &str) -> String {
    // We do a case-insensitive scan for the word GLOB followed by a quoted string.
    // Strategy: tokenise by single-quoted strings to avoid false positives inside literals.
    let upper = sql.to_ascii_uppercase();
    // Fast path – nothing to do
    if !upper.contains("GLOB") {
        return sql.to_string();
    }

    let bytes = sql.as_bytes();
    let mut result = String::with_capacity(sql.len());
    let mut i = 0;

    while i < bytes.len() {
        // Inside a single-quoted string → copy verbatim
        if bytes[i] == b'\'' {
            result.push('\'');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    result.push('\'');
                    i += 1;
                    // Escaped quote ''
                    if i < bytes.len() && bytes[i] == b'\'' {
                        result.push('\'');
                        i += 1;
                    }
                    break;
                }
                push_scanned_char(&mut result, sql, i);
                i += 1;
            }
            continue;
        }

        // Check for GLOB keyword (case-insensitive, word boundary)
        let rest = &sql[i..];
        let rest_upper = upper.get(i..).unwrap_or_default();
        if rest_upper.starts_with("GLOB") {
            // Must be followed by whitespace / quote (word boundary)
            let after = i + 4;
            let boundary = after >= sql.len()
                || !sql[after..].starts_with(|c: char| c.is_alphanumeric() || c == '_');
            if boundary {
                // Skip "GLOB"
                i += 4;
                // Skip whitespace
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                // Now we should be at the opening quote of the pattern
                if i < bytes.len() && bytes[i] == b'\'' {
                    i += 1; // skip opening quote
                    let mut pattern = String::new();
                    while i < bytes.len() {
                        if bytes[i] == b'\'' {
                            i += 1;
                            if i < bytes.len() && bytes[i] == b'\'' {
                                pattern.push('\'');
                                i += 1;
                            } else {
                                break;
                            }
                        } else {
                            push_scanned_char(&mut pattern, sql, i);
                            i += 1;
                        }
                    }
                    // Translate wildcards: * → %, ? → _
                    let pg_pattern: String = pattern
                        .chars()
                        .map(|c| match c {
                            '*' => '%',
                            '?' => '_',
                            other => other,
                        })
                        .collect();
                    result.push_str(&format!("ILIKE '{}'", pg_pattern));
                } else {
                    // No quote found – emit ILIKE as-is and continue
                    result.push_str("ILIKE ");
                }
                continue;
            }
        }

        result.push(rest.chars().next().unwrap());
        i += rest.chars().next().map_or(1, |c| c.len_utf8());
    }

    result
}

/// Remove SQLite index hints:
/// - `INDEXED BY <identifier>`
/// - `NOT INDEXED`
fn rewrite_indexed_by(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    if !upper.contains("INDEXED") {
        return sql.to_string();
    }

    // Find all occurrences of INDEXED BY <ident> and remove them.
    let mut result = String::with_capacity(sql.len());
    let mut i = 0;
    let bytes = sql.as_bytes();

    while i < bytes.len() {
        // Skip single-quoted strings verbatim
        if bytes[i] == b'\'' {
            result.push('\'');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    result.push('\'');
                    i += 1;
                    if i < bytes.len() && bytes[i] == b'\'' {
                        result.push('\'');
                        i += 1;
                    }
                    break;
                }
                push_scanned_char(&mut result, sql, i);
                i += 1;
            }
            continue;
        }

        let rest_upper = upper.get(i..).unwrap_or_default();
        if rest_upper.starts_with("NOT INDEXED") {
            let after = i + "NOT INDEXED".len();
            let boundary = after >= sql.len()
                || !sql[after..].starts_with(|c: char| c.is_alphanumeric() || c == '_');
            if boundary {
                i = after;
                continue;
            }
        }
        if rest_upper.starts_with("INDEXED") {
            let after = i + 7;
            let boundary = after >= sql.len()
                || !sql[after..].starts_with(|c: char| c.is_alphanumeric() || c == '_');
            if boundary {
                // Skip "INDEXED"
                let mut j = i + 7;
                // Skip whitespace
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                // Check for BY
                if sql[j..].to_ascii_uppercase().starts_with("BY") {
                    j += 2;
                    // Skip whitespace
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    // Skip identifier
                    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_')
                    {
                        j += 1;
                    }
                    i = j;
                    continue;
                }
            }
        }

        push_scanned_char(&mut result, sql, i);
        i += 1;
    }

    result
}

/// Remove `COLLATE <name>` clauses for SQLite-specific collations that either
/// sqlparser (SQLiteDialect) cannot parse, or that have no PostgreSQL equivalent.
///
/// ICU extension names like `icu_root` and `icu_und`, as well as `UNICODE` /
/// `UNICODE_CI`, are not in sqlparser's keyword list and cause a hard parse error:
///   "Expected: end of statement, found: collate"
///
/// `RTRIM` and `BINARY` parse fine but PostgreSQL has no matching collation, so
/// they are dropped here as well.
///
/// `NOCASE` is intentionally left intact; the AST-level transform in query.rs
/// converts it to LOWER(…) which is the correct PostgreSQL semantic.
fn rewrite_sqlite_collations(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    // Fast path — nothing to do
    if !upper.contains("COLLATE") {
        return sql.to_string();
    }

    let bytes = sql.as_bytes();
    let mut result = String::with_capacity(sql.len());
    let mut i = 0;

    while i < bytes.len() {
        // Skip single-quoted string literals verbatim
        if bytes[i] == b'\'' {
            result.push('\'');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    result.push('\'');
                    i += 1;
                    // Escaped quote: ''
                    if i < bytes.len() && bytes[i] == b'\'' {
                        result.push('\'');
                        i += 1;
                    }
                    break;
                }
                push_scanned_char(&mut result, sql, i);
                i += 1;
            }
            continue;
        }

        // Skip double-quoted identifiers verbatim
        if bytes[i] == b'"' {
            result.push('"');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'"' {
                    result.push('"');
                    i += 1;
                    // Escaped double-quote: ""
                    if i < bytes.len() && bytes[i] == b'"' {
                        result.push('"');
                        i += 1;
                    }
                    break;
                }
                push_scanned_char(&mut result, sql, i);
                i += 1;
            }
            continue;
        }

        // Skip backtick-quoted identifiers verbatim
        if bytes[i] == b'`' {
            result.push('`');
            i += 1;
            while i < bytes.len() {
                let ch = bytes[i] as char;
                push_scanned_char(&mut result, sql, i);
                i += 1;
                if ch == '`' {
                    break;
                }
            }
            continue;
        }

        // Check for COLLATE keyword (case-insensitive, word boundary)
        let rest_upper = upper.get(i..).unwrap_or_default();
        if rest_upper.starts_with("COLLATE") {
            let after = i + 7;
            let boundary = after >= sql.len()
                || !sql[after..].starts_with(|c: char| c.is_alphanumeric() || c == '_');
            if boundary {
                // Skip optional whitespace after COLLATE
                let mut j = after;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                // Read the collation name (letters, digits, underscores, hyphens)
                let name_start = j;
                while j < bytes.len()
                    && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'-')
                {
                    j += 1;
                }
                let collation_name = sql[name_start..j].to_ascii_uppercase();

                // Strip SQLite-only collations that have no direct PostgreSQL equivalent.
                //
                // ICU names (icu_root, icu_und, …) and unicode/unicode_ci are not
                // recognised by sqlparser/SQLiteDialect and cause a hard parse error:
                //   "Expected: end of statement, found: collate"
                //
                // RTRIM and BINARY parse fine, but PostgreSQL has no matching collation
                // so they are stripped here too.  NOCASE is intentionally left for the
                // AST-level handler in query.rs which converts it to LOWER(…).
                let should_strip = collation_name.starts_with("ICU")
                    || collation_name == "UNICODE"
                    || collation_name == "UNICODE_CI"
                    || collation_name == "RTRIM"
                    || collation_name == "BINARY";

                if should_strip {
                    // Skip the entire `COLLATE <name>` token.
                    // Also consume one leading space before COLLATE (if any) to avoid
                    // leaving a double-space artefact.
                    if result.ends_with(' ') {
                        result.pop();
                    }
                    i = j;
                    continue;
                }
            }
        }

        push_scanned_char(&mut result, sql, i);
        i += 1;
    }

    result
}

#[cfg(test)]
mod unicode_preprocess_tests {
    use super::*;

    #[test]
    fn preserves_unicode_literals_identifiers_and_order_by() {
        for sql in [
            "SELECT 'München 東京 🐇', 'd''été' FROM \"Straße\" ORDER BY \"日本語\"",
            "SELECT title FROM metadata_items WHERE title MATCH 'mün*' ORDER BY title",
            "SELECT 'Straße' FROM metadata_items WHERE title REGEXP 'mün.*' ORDER BY title",
            "SELECT 'München' FROM metadata_items WHERE title GLOB '東*' ORDER BY title",
            "SELECT 'München' FROM metadata_items NOT INDEXED ORDER BY title COLLATE NOCASE",
            "CREATE TABLE \"Straße\" (\"日本語\" TEXT DEFAULT '🐇', id INT UNIQUE ON CONFLICT IGNORE)",
            "SELECT DISTINCT \"日本語\" FROM \"Straße\" ORDER BY \"日本語\"",
        ] {
            let result = preprocess_sql(sql);
            for preserved in ["München", "東京 🐇", "d''été", "Straße", "日本語", "mün", "東", "🐇"] {
                if sql.contains(preserved) {
                    assert!(result.contains(preserved), "lost {preserved}: {result}");
                }
            }
        }
    }

    #[test]
    fn unicode_scanners_preserve_exact_text() {
        let sql = "SELECT 'München d''été 🐇', \"Straße\" ORDER BY \"日本語\"";
        assert_eq!(fix_placeholder_spacing(sql), sql);
        assert_eq!(rewrite_regexp_operator(sql), sql);
        assert_eq!(rewrite_raise_function_calls(sql), sql);
        assert_eq!(
            find_top_level_keyword(sql, "order by"),
            sql.find("ORDER BY")
        );
        assert_eq!(
            parse_quoted_token("'München d''été 🐇'", 0).unwrap().1,
            "München d'été 🐇"
        );
    }
}
