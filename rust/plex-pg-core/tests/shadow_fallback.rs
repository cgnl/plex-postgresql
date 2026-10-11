use rusqlite::{Connection, Result};

fn count_sql_params(sql: Option<&str>) -> i32 {
    let sql = match sql {
        Some(s) => s,
        None => return 0,
    };
    let mut count = 0;
    let mut in_quote = false;
    let bytes = sql.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\'' {
            in_quote = !in_quote;
            i += 1;
            continue;
        }
        if in_quote {
            i += 1;
            continue;
        }
        if b == b'?' {
            count += 1;
            i += 1;
            continue;
        }
        if b == b':' {
            let prev_ok = i == 0 || matches!(bytes[i - 1], b' ' | b',' | b'(' | b'=');
            let next_ok =
                i + 1 < bytes.len() && (bytes[i + 1] == b'_' || bytes[i + 1].is_ascii_alphabetic());
            if prev_ok && next_ok {
                count += 1;
                i += 1;
                while i + 1 < bytes.len() {
                    let n = bytes[i + 1];
                    if n == b'_' || n.is_ascii_alphanumeric() {
                        i += 1;
                        continue;
                    }
                    break;
                }
            }
        }
        i += 1;
    }
    count
}

fn build_dummy_shadow_sql(param_count: i32) -> String {
    if param_count <= 0 {
        return "SELECT 1 WHERE 0".to_string();
    }
    let mut out = String::from("SELECT 1 WHERE ");
    for i in 0..param_count {
        if i > 0 {
            out.push_str(" AND ");
        }
        out.push_str("? IS NOT NULL");
    }
    out
}

fn count_question_marks(sql: &str) -> i32 {
    sql.as_bytes().iter().filter(|b| **b == b'?').count() as i32
}

#[test]
fn count_zero_params() {
    assert_eq!(count_sql_params(Some("SELECT * FROM metadata_items")), 0);
}

#[test]
fn count_question_mark_params() {
    assert_eq!(count_sql_params(Some("SELECT * FROM foo WHERE id = ?")), 1);
    assert_eq!(
        count_sql_params(Some("SELECT * FROM foo WHERE a = ? AND b = ?")),
        2
    );
    assert_eq!(
        count_sql_params(Some("INSERT INTO t VALUES (?, ?, ?, ?)")),
        4
    );
}

#[test]
fn count_named_params() {
    assert_eq!(
        count_sql_params(Some("SELECT * FROM foo WHERE id = :C1")),
        1
    );
    assert_eq!(
        count_sql_params(Some("SELECT * FROM foo WHERE a = :C1 AND b = :C2")),
        2
    );
}

#[test]
fn count_mixed_params() {
    assert_eq!(
        count_sql_params(Some("SELECT * FROM foo WHERE a = ? AND b = :C1")),
        2
    );
    assert_eq!(
        count_sql_params(Some(
            "SELECT * FROM foo WHERE a = :C1 AND b = ? AND c = :C2"
        )),
        3
    );
}

#[test]
fn count_params_in_quotes_ignored() {
    assert_eq!(count_sql_params(Some("SELECT * FROM foo WHERE a = '?'")), 0);
    assert_eq!(
        count_sql_params(Some(
            "UPDATE t SET guid=REPLACE(guid,'?lang=en','?lang=xn')"
        )),
        0
    );
    assert_eq!(
        count_sql_params(Some("SELECT * FROM foo WHERE a = '?' AND b = ?")),
        1
    );
}

#[test]
fn count_colon_not_param() {
    assert_eq!(
        count_sql_params(Some("SELECT 'http://example.com' FROM foo")),
        0
    );
    assert_eq!(count_sql_params(Some("SELECT '12:30:00' FROM foo")), 0);
    assert_eq!(count_sql_params(Some("SELECT id::text FROM foo")), 0);
}

#[test]
fn count_colon_after_valid_prefix() {
    assert_eq!(count_sql_params(Some("WHERE id = :id")), 1);
    assert_eq!(count_sql_params(Some("WHERE id =:id")), 1);
    assert_eq!(count_sql_params(Some("VALUES (:a, :b, :c)")), 3);
}

#[test]
fn count_real_plex_queries() {
    let sql1 = "select media_items.id as 'media_items_id' \
                from media_items \
                WHERE media_items.metadata_item_id = :C1 \
                AND media_items.library_section_id = :C2";
    assert_eq!(count_sql_params(Some(sql1)), 2);

    let sql2 = "SELECT * FROM metadata_items WHERE id IN (:C1, :C2, :C3, :C4, :C5)";
    assert_eq!(count_sql_params(Some(sql2)), 5);
}

#[test]
fn count_empty_and_null() {
    assert_eq!(count_sql_params(Some("")), 0);
    assert_eq!(count_sql_params(None), 0);
}

#[test]
fn dummy_sql_zero_params() {
    let sql = build_dummy_shadow_sql(0);
    assert_eq!(sql, "SELECT 1 WHERE 0");
    assert_eq!(count_question_marks(&sql), 0);
}

#[test]
fn dummy_sql_one_param() {
    let sql = build_dummy_shadow_sql(1);
    assert_eq!(count_question_marks(&sql), 1);
    assert!(sql.contains("? IS NOT NULL"));
}

#[test]
fn dummy_sql_five_params() {
    let sql = build_dummy_shadow_sql(5);
    assert_eq!(count_question_marks(&sql), 5);
    assert!(sql.starts_with("SELECT 1 WHERE "));
}

#[test]
fn dummy_sql_many_params() {
    let sql = build_dummy_shadow_sql(50);
    assert_eq!(count_question_marks(&sql), 50);
}

#[test]
fn shadow_elimination_memory_db_opens() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    let stmt = conn.prepare("SELECT 1 WHERE 0")?;
    assert_eq!(stmt.parameter_count(), 0);
    Ok(())
}

#[test]
fn shadow_elimination_dummy_with_params() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    let stmt = conn.prepare(
        "SELECT 1 WHERE ? IS NOT NULL AND ? IS NOT NULL AND ? IS NOT NULL AND ? IS NOT NULL AND ? IS NOT NULL",
    )?;
    assert_eq!(stmt.parameter_count(), 5);
    Ok(())
}

#[test]
fn shadow_elimination_multiple_dummy_stmts() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    let mut stmts = Vec::new();
    for i in 0..50 {
        let sql = build_dummy_shadow_sql(i % 10);
        let stmt = conn.prepare(&sql)?;
        stmts.push(stmt);
    }
    assert_eq!(stmts.len(), 50);
    Ok(())
}

#[test]
fn shadow_elimination_bind_absorption() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    let mut stmt = conn.prepare("SELECT 1 WHERE ? IS NOT NULL")?;
    stmt.raw_bind_parameter(1, 42i32)?;
    Ok(())
}
