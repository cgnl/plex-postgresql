use super::*;

#[test]
fn type_bool_is_integer() {
    assert_eq!(oid_to_sqlite_type(16), SQLITE_INTEGER);
}

#[test]
fn type_int8_is_integer() {
    assert_eq!(oid_to_sqlite_type(20), SQLITE_INTEGER);
}

#[test]
fn type_int2_is_integer() {
    assert_eq!(oid_to_sqlite_type(21), SQLITE_INTEGER);
}

#[test]
fn type_int4_is_integer() {
    assert_eq!(oid_to_sqlite_type(23), SQLITE_INTEGER);
}

#[test]
fn type_oid_is_integer() {
    assert_eq!(oid_to_sqlite_type(26), SQLITE_INTEGER);
}

#[test]
fn type_float4_is_float() {
    assert_eq!(oid_to_sqlite_type(700), SQLITE_FLOAT);
}

#[test]
fn type_float8_is_float() {
    assert_eq!(oid_to_sqlite_type(701), SQLITE_FLOAT);
}

#[test]
fn type_numeric_is_float() {
    assert_eq!(oid_to_sqlite_type(1700), SQLITE_FLOAT);
}

#[test]
fn type_bytea_is_blob() {
    assert_eq!(oid_to_sqlite_type(17), SQLITE_BLOB);
}

#[test]
fn type_text_is_text() {
    assert_eq!(oid_to_sqlite_type(25), SQLITE_TEXT);
}

#[test]
fn type_bpchar_is_text() {
    assert_eq!(oid_to_sqlite_type(1042), SQLITE_TEXT);
}

#[test]
fn type_varchar_is_text() {
    assert_eq!(oid_to_sqlite_type(1043), SQLITE_TEXT);
}

#[test]
fn type_unknown_oid_is_text() {
    assert_eq!(oid_to_sqlite_type(9999), SQLITE_TEXT);
}

#[test]
fn decltype_int8_is_dt_integer_8() {
    assert_eq!(
        oid_to_sqlite_decltype(20).to_str().unwrap(),
        "dt_integer(8)"
    );
}

#[test]
fn decltype_bool_is_integer() {
    assert_eq!(oid_to_sqlite_decltype(16).to_str().unwrap(), "INTEGER");
}

#[test]
fn decltype_int2_is_integer() {
    assert_eq!(oid_to_sqlite_decltype(21).to_str().unwrap(), "INTEGER");
}

#[test]
fn decltype_int4_is_integer() {
    assert_eq!(oid_to_sqlite_decltype(23).to_str().unwrap(), "INTEGER");
}

#[test]
fn decltype_oid_is_integer() {
    assert_eq!(oid_to_sqlite_decltype(26).to_str().unwrap(), "INTEGER");
}

#[test]
fn decltype_float4_is_real() {
    assert_eq!(oid_to_sqlite_decltype(700).to_str().unwrap(), "REAL");
}

#[test]
fn decltype_float8_is_real() {
    assert_eq!(oid_to_sqlite_decltype(701).to_str().unwrap(), "REAL");
}

#[test]
fn decltype_numeric_is_real() {
    assert_eq!(oid_to_sqlite_decltype(1700).to_str().unwrap(), "REAL");
}

#[test]
fn decltype_bytea_is_blob() {
    assert_eq!(oid_to_sqlite_decltype(17).to_str().unwrap(), "BLOB");
}

#[test]
fn decltype_text_is_text() {
    assert_eq!(oid_to_sqlite_decltype(25).to_str().unwrap(), "TEXT");
}

#[test]
fn decltype_timestamp_is_integer() {
    // TIMESTAMP → 64-bit epoch int, matching Plex SOCI's integer(8) parsing.
    assert_eq!(
        oid_to_sqlite_decltype(1114).to_str().unwrap(),
        "dt_integer(8)"
    );
}

#[test]
fn decltype_timestamptz_is_integer() {
    // TIMESTAMPTZ → 64-bit epoch int, matching Plex SOCI's integer(8) parsing.
    assert_eq!(
        oid_to_sqlite_decltype(1184).to_str().unwrap(),
        "dt_integer(8)"
    );
}

#[test]
fn column_type_and_decltype_are_consistent() {
    // Invariant: column_type and decltype must agree on the SOCI type class.
    // If decltype says INTEGER, column_type must return SQLITE_INTEGER.
    use crate::db_interpose_value_helpers::pg_oid_to_sqlite_type_impl;
    let oids = [
        16, 20, 21, 23, 26, 700, 701, 1700, 17, 25, 1043, 1114, 1184, 1082,
    ];
    for oid in oids {
        let ty = pg_oid_to_sqlite_type_impl(oid);
        let decl = oid_to_sqlite_decltype(oid).to_str().unwrap();
        let expected_ty = match decl {
            "INTEGER" | "dt_integer(8)" => 1, // SQLITE_INTEGER
            "REAL" => 2,                      // SQLITE_FLOAT
            "BLOB" => 4,                      // SQLITE_BLOB
            _ => 3,                           // SQLITE_TEXT
        };
        assert_eq!(
            ty, expected_ty,
            "OID {oid}: column_type={ty} but decltype='{decl}' expects {expected_ty}"
        );
    }
}

#[test]
fn decltype_date_is_text() {
    assert_eq!(oid_to_sqlite_decltype(1082).to_str().unwrap(), "TEXT");
}

#[test]
fn decltype_time_is_text() {
    assert_eq!(oid_to_sqlite_decltype(1083).to_str().unwrap(), "TEXT");
}

#[test]
fn decltype_unknown_oid_is_text() {
    assert_eq!(oid_to_sqlite_decltype(9999).to_str().unwrap(), "TEXT");
}

#[test]
fn upsert_non_matching_sql_returns_none() {
    assert_eq!(
        convert_metadata_settings_upsert("SELECT * FROM some_table"),
        None
    );
}

#[test]
fn upsert_insert_without_table_returns_none() {
    assert_eq!(
        convert_metadata_settings_upsert("INSERT INTO other_table VALUES (1)"),
        None
    );
}

#[test]
fn upsert_plain_metadata_insert_preserves_source_semantics() {
    for sql in [
        "INSERT INTO plex.metadata_item_settings (account_id, guid) VALUES (1, 'x')",
        "INSERT INTO plex.metadata_item_settings (id, account_id, guid) VALUES (7, 1, 'x')",
        "INSERT INTO plex.metadata_item_settings DEFAULT VALUES",
    ] {
        assert_eq!(convert_metadata_settings_upsert(sql), None);
    }
}

#[test]
fn upsert_already_has_on_conflict_returns_none() {
    let sql = "INSERT INTO plex.metadata_item_settings (account_id, guid) VALUES (1, 'x') \
                   ON CONFLICT (account_id, guid) DO NOTHING";
    assert_eq!(convert_metadata_settings_upsert(sql), None);
}

#[test]
fn upsert_already_has_returning_returns_none() {
    let sql = "INSERT INTO plex.metadata_item_settings (account_id, guid) VALUES (1, 'x') \
                   RETURNING id";
    assert_eq!(convert_metadata_settings_upsert(sql), None);
}

#[test]
fn upsert_empty_string_returns_none() {
    assert_eq!(convert_metadata_settings_upsert(""), None);
}

#[test]
fn upsert_case_insensitive_match() {
    let sql = "insert into METADATA_ITEM_SETTINGS (account_id, guid) values (1, 'x')";
    let result = convert_metadata_settings_upsert(sql);
    assert_eq!(result, None);
}

#[test]
fn upsert_ffi_null_returns_null() {
    let ptr = rust_convert_metadata_settings_upsert(std::ptr::null());
    assert!(ptr.is_null());
}

#[test]
fn upsert_ffi_non_matching_returns_null() {
    let input = cs("SELECT 1");
    let ptr = rust_convert_metadata_settings_upsert(input.as_ptr());
    assert!(ptr.is_null());
}

#[test]
fn upsert_ffi_plain_metadata_insert_returns_null() {
    let input = cs("INSERT INTO plex.metadata_item_settings (account_id, guid) VALUES (1, 'x')");
    let ptr = rust_convert_metadata_settings_upsert(input.as_ptr());
    assert!(
        ptr.is_null(),
        "plain INSERT must stay on the normal write path"
    );
}

#[test]
fn extract_url_encoded_pattern_returns_id() {
    let sql = "INSERT INTO play_queue_generators (uri) VALUES ('server://x%2Fmetadata%2F12345%2F')";
    assert_eq!(extract_metadata_id(sql), 12345);
}

#[test]
fn extract_plain_slash_pattern_returns_id() {
    let sql = "INSERT INTO play_queue_generators (uri) VALUES ('server://x/metadata/67890/other')";
    assert_eq!(extract_metadata_id(sql), 67890);
}

#[test]
fn extract_not_a_play_queue_insert_returns_zero() {
    let sql = "INSERT INTO some_other_table (uri) VALUES ('/metadata/999')";
    assert_eq!(extract_metadata_id(sql), 0);
}

#[test]
fn extract_no_metadata_pattern_returns_zero() {
    let sql = "INSERT INTO play_queue_generators (uri) VALUES ('something-else')";
    assert_eq!(extract_metadata_id(sql), 0);
}

#[test]
fn extract_empty_string_returns_zero() {
    assert_eq!(extract_metadata_id(""), 0);
}

#[test]
fn extract_not_an_insert_returns_zero() {
    let sql = "SELECT * FROM play_queue_generators WHERE uri LIKE '%/metadata/1%'";
    assert_eq!(extract_metadata_id(sql), 0);
}

#[test]
fn extract_single_digit_id() {
    let sql = "INSERT INTO play_queue_generators (uri) VALUES ('/metadata/7')";
    assert_eq!(extract_metadata_id(sql), 7);
}

#[test]
fn extract_large_id() {
    let sql = "INSERT INTO play_queue_generators (uri) VALUES ('/metadata/9876543210')";
    assert_eq!(extract_metadata_id(sql), 9_876_543_210);
}

#[test]
fn extract_ffi_null_returns_zero() {
    assert_eq!(rust_extract_metadata_id(std::ptr::null()), 0);
}

#[test]
fn extract_ffi_url_encoded_returns_id() {
    let input = cs("INSERT INTO play_queue_generators (uri) VALUES ('x%2Fmetadata%2F42')");
    assert_eq!(rust_extract_metadata_id(input.as_ptr()), 42);
}

#[test]
fn extract_ffi_non_matching_returns_zero() {
    let input = cs("INSERT INTO other_table VALUES (1)");
    assert_eq!(rust_extract_metadata_id(input.as_ptr()), 0);
}

#[test]
fn decltype_special_case_dt_integer_for_timestamp_column() {
    let col = cs("created_at");
    let sql = cs("select created_at from t");
    let rc = rust_decltype_special_case(20, col.as_ptr(), sql.as_ptr(), 42);
    assert_eq!(rc, DECLTYPE_CASE_DT_INTEGER_8);
}

#[test]
fn decltype_special_case_dt_integer_for_greatest_metadata_refresh() {
    let col = cs("greatest");
    let sql = cs(
        "select GREATEST(max(metadata_items.changed_at), max(metadata_items.resources_changed_at))",
    );
    let rc = rust_decltype_special_case(20, col.as_ptr(), sql.as_ptr(), 42);
    assert_eq!(rc, DECLTYPE_CASE_DT_INTEGER_8);
}

#[test]
fn decltype_special_case_expression_uses_oid_mapping() {
    let col = cs("count");
    let sql = cs("select count(*) from t");
    let rc = rust_decltype_special_case(23, col.as_ptr(), sql.as_ptr(), 0);
    assert_eq!(rc, DECLTYPE_CASE_NULL);
}

#[test]
fn decltype_special_case_aggregate_alias_returns_null_case() {
    let col = cs("max(year)");
    let sql = cs("select max(year) from t");
    let rc = rust_decltype_special_case(23, col.as_ptr(), sql.as_ptr(), 0);
    assert_eq!(rc, DECLTYPE_CASE_NULL);
}

#[test]
fn decltype_special_case_count_star_alias_returns_null_case() {
    let col = cs("count(*)");
    let sql = cs("select count(*) as \"count(*)\" from t");
    let rc = rust_decltype_special_case(20, col.as_ptr(), sql.as_ptr(), 0);
    assert_eq!(rc, DECLTYPE_CASE_NULL);
}

#[test]
fn decltype_special_case_sum_alias_returns_null_case() {
    let col = cs("sum");
    let sql = cs("select sum(duration) as sum from t");
    let rc = rust_decltype_special_case(20, col.as_ptr(), sql.as_ptr(), 0);
    assert_eq!(rc, DECLTYPE_CASE_NULL);
}

#[test]
fn decltype_special_case_regular_column_without_table_oid_uses_oid_mapping() {
    let col = cs("identifier");
    let sql = cs("select identifier from plugins where id = $1");
    let rc = rust_decltype_special_case(25, col.as_ptr(), sql.as_ptr(), 0);
    assert_eq!(rc, DECLTYPE_CASE_NONE);
}

#[test]
fn decltype_special_case_regular_int8_without_table_oid_uses_oid_mapping() {
    let col = cs("id");
    let sql = cs("select id from plugins where id = $1");
    let rc = rust_decltype_special_case(20, col.as_ptr(), sql.as_ptr(), 0);
    assert_eq!(rc, DECLTYPE_CASE_NONE);
}

#[test]
fn decltype_special_case_none_for_regular_column() {
    let col = cs("id");
    let sql = cs("select id from t");
    let rc = rust_decltype_special_case(23, col.as_ptr(), sql.as_ptr(), 123);
    assert_eq!(rc, DECLTYPE_CASE_NONE);
}
