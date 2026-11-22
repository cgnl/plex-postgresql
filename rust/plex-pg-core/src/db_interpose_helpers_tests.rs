#![allow(non_snake_case)]

use super::*;
use std::ffi::{CStr, CString};

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn take_cstring(ptr: *mut c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CString::from_raw(ptr) }
        .to_string_lossy()
        .into_owned()
}

fn normalize_decltype(input: Option<&str>) -> String {
    let ptr = normalize_sqlite_decltype_impl(input);
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

fn djb2(s: &str) -> u32 {
    let mut hash: u32 = 5381;
    for b in s.as_bytes() {
        hash = ((hash << 5).wrapping_add(hash)).wrapping_add(*b as u32);
    }
    hash
}

#[test]
fn validate_utf8_accepts_valid_input() {
    let s = "Plex \u{1F4FA}";
    assert_eq!(rust_validate_utf8(s.as_ptr() as *const c_char, s.len()), 1);
}

#[test]
fn validate_utf8_rejects_invalid_input() {
    let invalid = [0xffu8, 0xfeu8];
    assert_eq!(
        rust_validate_utf8(invalid.as_ptr() as *const c_char, invalid.len()),
        0
    );
}

#[test]
fn rewrite_server_uri_rewrites_expected_prefix() {
    let input = CString::new("server://machine/com.plexapp.plugins.library/library/metadata/123")
        .expect("valid c string");
    let mut out = [0 as c_char; 256];

    assert_eq!(
        rust_rewrite_server_library_uri(input.as_ptr(), out.as_mut_ptr(), out.len()),
        1
    );

    let rewritten = unsafe { CStr::from_ptr(out.as_ptr()) }
        .to_str()
        .expect("utf8 output");
    assert_eq!(rewritten, "library://metadata/123");
}

#[test]
fn rewrite_server_uri_handles_multiple_matches() {
    let input = CString::new(
        "a=server://m1/com.plexapp.plugins.library/library/one;b=server://m2/com.plexapp.plugins.library/library/two",
    )
    .expect("valid c string");
    let mut out = [0 as c_char; 256];

    assert_eq!(
        rust_rewrite_server_library_uri(input.as_ptr(), out.as_mut_ptr(), out.len()),
        1
    );

    let rewritten = unsafe { CStr::from_ptr(out.as_ptr()) }
        .to_str()
        .expect("utf8 output");
    assert_eq!(rewritten, "a=library://one;b=library://two");
}

fn rewrite_with_buf(input: &str, out_len: usize) -> (i32, String) {
    let input = CString::new(input).expect("valid c string");
    let mut out = vec![0 as c_char; out_len];
    let ok = rust_rewrite_server_library_uri(input.as_ptr(), out.as_mut_ptr(), out.len());
    let rewritten = unsafe { CStr::from_ptr(out.as_ptr()) }
        .to_str()
        .unwrap_or("")
        .to_string();
    (ok, rewritten)
}

#[test]
fn rewrite_server_uri_standalone() {
    let (ok, out) = rewrite_with_buf(
        "server://71b2873061a562bf7541852f9a43087e88a63f9a/com.plexapp.plugins.library/library/sections/2/all?type=2",
        512,
    );
    assert_eq!(ok, 1);
    assert_eq!(out, "library://sections/2/all?type=2");
}

#[test]
fn rewrite_server_uri_json_embedded() {
    let (ok, out) = rewrite_with_buf(
        "{\"at:childCount\":\"1\",\"at:smart\":\"1\",\"pv:uri\":\"server://71b2873061a562bf7541852f9a43087e88a63f9a/com.plexapp.plugins.library/library/sections/2/all?type=2&sort=date\"}",
        1024,
    );
    assert_eq!(ok, 1);
    assert_eq!(
        out,
        "{\"at:childCount\":\"1\",\"at:smart\":\"1\",\"pv:uri\":\"library://sections/2/all?type=2&sort=date\"}"
    );
}

#[test]
fn rewrite_server_uri_no_server_prefix() {
    let (ok, out) = rewrite_with_buf("library://sections/1/all", 256);
    assert_eq!(ok, 0);
    assert!(out.is_empty());
}

#[test]
fn rewrite_server_uri_no_plugin_path() {
    let (ok, out) = rewrite_with_buf("server://abc123/some/other/path", 256);
    assert_eq!(ok, 0);
    assert!(out.is_empty());
}

#[test]
fn rewrite_server_uri_empty_string() {
    let (ok, out) = rewrite_with_buf("", 64);
    assert_eq!(ok, 0);
    assert!(out.is_empty());
}

#[test]
fn rewrite_server_uri_null_input() {
    let mut out = [0 as c_char; 64];
    let ok = rust_rewrite_server_library_uri(std::ptr::null(), out.as_mut_ptr(), out.len());
    assert_eq!(ok, 0);
}

#[test]
fn rewrite_server_uri_plain_text() {
    let (ok, out) = rewrite_with_buf(
        "{\"at:childCount\":\"5\",\"pv:thumbBlurHash\":\"abc123\"}",
        256,
    );
    assert_eq!(ok, 0);
    assert!(out.is_empty());
}

#[test]
fn rewrite_server_uri_multiple_in_json() {
    let (ok, out) = rewrite_with_buf(
        "{\"uri1\":\"server://aaa/com.plexapp.plugins.library/library/sections/1/all\",\"uri2\":\"server://aaa/com.plexapp.plugins.library/library/sections/2/all\"}",
        2048,
    );
    assert_eq!(ok, 1);
    assert_eq!(
        out,
        "{\"uri1\":\"library://sections/1/all\",\"uri2\":\"library://sections/2/all\"}"
    );
}

#[test]
fn rewrite_server_uri_output_shorter() {
    let input = "server://71b2873061a562bf7541852f9a43087e88a63f9a/com.plexapp.plugins.library/library/sections/2/all";
    let (ok, out) = rewrite_with_buf(input, 512);
    assert_eq!(ok, 1);
    assert!(out.len() < input.len());
}

#[test]
fn rewrite_server_uri_encoded_params() {
    let (ok, out) = rewrite_with_buf(
        "server://71b287/com.plexapp.plugins.library/library/sections/2/all?type=2&sort=originallyAvailableAt%3Adesc&push=1&show.network=248684&pop=1",
        1024,
    );
    assert_eq!(ok, 1);
    assert_eq!(
        out,
        "library://sections/2/all?type=2&sort=originallyAvailableAt%3Adesc&push=1&show.network=248684&pop=1"
    );
}

#[test]
fn rewrite_server_uri_small_buffer() {
    let (ok, out) = rewrite_with_buf(
        "server://abc/com.plexapp.plugins.library/library/sections/2/all?type=2&sort=date",
        32,
    );
    assert!(ok == 0 || out.len() < 32);
}

#[test]
fn rewrite_server_uri_tiny_buffer() {
    let (ok, _) = rewrite_with_buf("server://x", 8);
    assert_eq!(ok, 0);
}

#[test]
fn rewrite_server_uri_real_plex_blob() {
    let (ok, out) = rewrite_with_buf(
        "{\"at:childCount\":\"1\",\"at:smart\":\"1\",\"pv:blurHashesChangedAt\":\"277470\",\"pv:thumbBlurHash\":\"LJC?YqM{IVoz\",\"pv:uri\":\"server://71b2873061a562bf7541852f9a43087e88a63f9a/com.plexapp.plugins.library/library/sections/2/all?type=2&sort=originallyAvailableAt%3Adesc&push=1&show.genre=8966&pop=1\"}",
        2048,
    );
    assert_eq!(ok, 1);
    assert!(out.contains("library://sections/2/all"));
    assert!(!out.contains("server://"));
}

#[test]
fn normalize_sql_literals_extracts_two_params() {
    let sql = "SELECT * FROM t WHERE id = 123 AND score >= -4.5";
    let (normalized, params) =
        normalize_sql_literals_impl(sql).expect("expected normalized result");
    assert_eq!(normalized, "SELECT * FROM t WHERE id = $1 AND score >= $2");
    assert_eq!(params, vec!["123".to_string(), "-4.5".to_string()]);
}

#[test]
fn normalize_sql_literals_skips_insert() {
    assert!(normalize_sql_literals_impl("INSERT INTO t VALUES (1)").is_none());
}

#[test]
fn prepare_simple_hash_is_deterministic() {
    let a = prepare_simple_hash("SELECT * FROM t", 200);
    let b = prepare_simple_hash("SELECT * FROM t", 200);
    assert_eq!(a, b);
}

#[test]
fn compat_aliases__alias_collection_sync_aggregates_rewrites_select_list() {
    let sqlite =
        "select count(*), min(year), max(year) from tags join taggings on 1=1 group by tags.id";
    let pg =
        "SELECT count(*), min(year), max(year) FROM tags JOIN taggings ON true GROUP BY tags.id";
    let out = alias_collection_sync_aggregates(sqlite, pg).expect("should rewrite");
    assert!(out.contains("count(*) AS \"count(*)\""));
    assert!(out.contains("min(year) AS \"min(year)\""));
    assert!(out.contains("max(year) AS \"max(year)\""));
}

#[test]
fn compat_aliases__alias_collection_sync_aggregates_noop_for_other_queries() {
    let sqlite = "select id from tags";
    let pg = "SELECT id FROM tags";
    assert!(alias_collection_sync_aggregates(sqlite, pg).is_none());
}

#[test]
fn strip_collate_icu_root_removes_both_forms() {
    let sql = "SELECT * FROM t COLLATE icu_root WHERE x=1";
    let out = strip_collate_icu_root(sql).expect("should strip");
    assert!(!out.to_ascii_lowercase().contains("collate icu_root"));
}

#[test]
fn is_library_db_path_matches_suffix() {
    assert!(is_library_db_path_impl(
        "/x/y/com.plexapp.plugins.library.db"
    ));
    assert!(!is_library_db_path_impl("/x/y/other.db"));
}

#[test]
fn is_library_or_blobs_path_matches_both() {
    assert!(is_library_or_blobs_db_path_impl(
        "/x/y/com.plexapp.plugins.library.db"
    ));
    assert!(is_library_or_blobs_db_path_impl(
        "/x/y/com.plexapp.plugins.library.blobs.db"
    ));
    assert!(!is_library_or_blobs_db_path_impl("/x/y/other.db"));
}

#[test]
fn junk_metadata_insert_detects_null_pair() {
    let sql = "INSERT INTO metadata_items (library_section_id, metadata_type, title) VALUES (NULL, NULL, 'x')";
    assert!(is_junk_metadata_insert_impl(sql));
}

#[test]
fn junk_metadata_insert_ignores_non_null() {
    let sql = "INSERT INTO metadata_items (library_section_id, metadata_type) VALUES (1, NULL)";
    assert!(!is_junk_metadata_insert_impl(sql));
}

#[test]
fn trace_list_contains_idx_matches_values() {
    assert!(list_contains_idx("5,6; 7", 6));
    assert!(!list_contains_idx("5,6; 7", 4));
    assert!(list_contains_idx("all", 999));
}

#[test]
fn trace_list_any_token_in_haystack_matches_token() {
    assert!(list_any_token_in_haystack(
        "tags,collections",
        "from tags join x"
    ));
    assert!(!list_any_token_in_haystack("abc,def", "from tags join x"));
}

#[test]
fn subset_fts__simplify_fts_for_sqlite_rewrites_match_and_join() {
    let sql = "SELECT * FROM a JOIN fts4_metadata_titles t ON t.rowid=a.id WHERE fts4_metadata_titles.title MATCH 'foo''bar'";
    let out = simplify_fts_for_sqlite(sql).expect("should simplify");
    assert!(!out
        .to_ascii_lowercase()
        .contains("join fts4_metadata_titles"));
    assert!(out.contains("1=0"));
}

#[test]
fn subset_fts__simplify_fts_for_sqlite_noop_without_fts() {
    assert!(simplify_fts_for_sqlite("SELECT * FROM t").is_none());
}

#[test]
fn subset_ddl_lite__add_if_not_exists_for_sqlite_ddl_rewrites_create_table() {
    let sql = "CREATE TABLE tags (id INTEGER)";
    let out = add_if_not_exists_for_sqlite_ddl(sql).expect("should rewrite");
    assert!(out.contains("CREATE TABLE IF NOT EXISTS tags"));
}

#[test]
fn subset_ddl_lite__add_if_not_exists_for_sqlite_ddl_rewrites_create_unique_index() {
    let sql = "CREATE UNIQUE INDEX idx_tags ON tags(id)";
    let out = add_if_not_exists_for_sqlite_ddl(sql).expect("should rewrite");
    assert!(out.contains("CREATE UNIQUE INDEX IF NOT EXISTS idx_tags"));
}

#[test]
fn subset_ddl_lite__add_if_not_exists_for_sqlite_ddl_noop_if_already_present() {
    let sql = "CREATE INDEX IF NOT EXISTS idx_tags ON tags(id)";
    assert!(add_if_not_exists_for_sqlite_ddl(sql).is_none());
}

#[test]
fn binary_detection_and_hex_encoding_work() {
    assert!(contains_binary_bytes_impl(&[0x1f, 0x8b, 0x08]));
    assert!(!contains_binary_bytes_impl(b"hello"));
    assert_eq!(bytes_to_pg_hex_impl(&[0x41, 0x42, 0xff]), "\\x4142ff");
}

#[test]
fn related_items_and_mask_predicates_work() {
    let sql = "select * from taggings as related join x";
    assert!(is_related_items_query_impl(sql));
    assert!(should_mask_collection_metadata_type_impl(
        sql,
        "metadata_type",
        18
    ));
    assert!(!should_mask_collection_metadata_type_impl(
        sql,
        "other_col",
        18
    ));
    assert!(!should_mask_collection_metadata_type_impl(
        "select * from x",
        "metadata_type",
        18
    ));
}

#[test]
fn find_insert_column_index_handles_quoted_columns() {
    let sql = "INSERT INTO metadata_items (\"id\", `library_section_id`, metadata_type, title) VALUES ($1,$2,$3,$4)";
    assert_eq!(find_insert_column_index_impl(sql, "library_section_id"), 1);
    assert_eq!(find_insert_column_index_impl(sql, "metadata_type"), 2);
    assert_eq!(find_insert_column_index_impl(sql, "missing_col"), -1);
}

#[test]
fn pg_oid_to_sqlite_type_mapping_matches_expectations() {
    assert_eq!(
        pg_oid_to_sqlite_type_impl(20),
        crate::db_interpose_value_helpers::SQLITE_INTEGER_CONST
    );
    assert_eq!(
        pg_oid_to_sqlite_type_impl(701),
        crate::db_interpose_value_helpers::SQLITE_FLOAT_CONST
    );
    assert_eq!(
        pg_oid_to_sqlite_type_impl(17),
        crate::db_interpose_value_helpers::SQLITE_BLOB_CONST
    );
    assert_eq!(
        pg_oid_to_sqlite_type_impl(25),
        crate::db_interpose_value_helpers::SQLITE_TEXT_CONST
    );
}

#[test]
fn trim_first_line_trims_ws_and_newline() {
    assert_eq!(trim_first_line("  abc \r\n").as_deref(), Some("abc"));
    assert_eq!(trim_first_line("   \n"), None);
}

#[test]
fn common_helpers_is_library_db_path_null_returns_false() {
    assert_eq!(rust_is_library_db_path(std::ptr::null()), 0);
}

#[test]
fn common_helpers_is_library_db_path_empty_returns_false() {
    assert!(!is_library_db_path_impl(""));
}

#[test]
fn common_helpers_is_library_db_path_matches_known_paths() {
    assert!(is_library_db_path_impl(
        "/data/Databases/com.plexapp.plugins.library.db"
    ));
    assert!(is_library_db_path_impl(
        "/data/Databases/com.plexapp.plugins.library.blobs.db"
    ));
    assert!(is_library_db_path_impl("com.plexapp.plugins.library.db"));
    assert!(is_library_db_path_impl(
        "com.plexapp.plugins.library.blobs.db"
    ));
    assert!(is_library_db_path_impl("/Users/plex/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db"));
    assert!(is_library_db_path_impl("/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.blobs.db"));
}

#[test]
fn common_helpers_is_library_db_path_rejects_non_library_paths() {
    assert!(!is_library_db_path_impl(
        "com.plexapp.plugins.preferences.db"
    ));
    assert!(!is_library_db_path_impl("/tmp/test.db"));
    assert!(!is_library_db_path_impl("library.db"));
}

#[test]
fn common_helpers_is_library_db_path_accepts_wal_suffix() {
    assert!(is_library_db_path_impl(
        "com.plexapp.plugins.library.db-wal"
    ));
}

#[test]
fn bind_helpers_contains_binary_bytes_null_and_empty() {
    assert_eq!(rust_contains_binary_bytes(std::ptr::null(), 0), 0);
    assert_eq!(rust_contains_binary_bytes(b"".as_ptr(), 0), 0);
}

#[test]
fn bind_helpers_contains_binary_bytes_text_cases() {
    assert!(!contains_binary_bytes_impl(b"Hello, World!"));
    let utf8 = b"H\xc3\xa9llo W\xc3\xb6rld";
    assert!(!contains_binary_bytes_impl(utf8));
    assert!(!contains_binary_bytes_impl(b"col1\tcol2"));
    assert!(!contains_binary_bytes_impl(b"line1\nline2"));
    assert!(!contains_binary_bytes_impl(b"line1\r\nline2"));
}

#[test]
fn bind_helpers_contains_binary_bytes_detects_control_and_invalid() {
    assert!(contains_binary_bytes_impl(b"\x00"));
    assert!(contains_binary_bytes_impl(b"\x01"));
    assert!(contains_binary_bytes_impl(b"\x07test"));
    assert!(contains_binary_bytes_impl(b"\x7F"));
    assert!(contains_binary_bytes_impl(b"\xC0"));
    assert!(contains_binary_bytes_impl(b"\xC1"));
    assert!(contains_binary_bytes_impl(b"\xF5"));
    assert!(contains_binary_bytes_impl(&[0x1f, 0x8b, 0x08, 0x00]));
    let mixed = [b'H', b'e', b'l', b'l', b'o', 0x01, b'W'];
    assert!(contains_binary_bytes_impl(&mixed));
    let late_binary = [b'A', b'B', b'C', b'D', b'E', 0x02];
    assert!(contains_binary_bytes_impl(&late_binary));
}

#[test]
fn bind_helpers_bytes_to_pg_hex_null_and_empty() {
    let out = take_cstring(rust_bytes_to_pg_hex(std::ptr::null(), 0));
    assert_eq!(out, "");
    let out = take_cstring(rust_bytes_to_pg_hex(b"".as_ptr(), 0));
    assert_eq!(out, "");
}

#[test]
fn bind_helpers_bytes_to_pg_hex_known_values() {
    let out = take_cstring(rust_bytes_to_pg_hex([0xAB].as_ptr(), 1));
    assert_eq!(out, "\\xab");
    let out = take_cstring(rust_bytes_to_pg_hex([0xDE, 0xAD, 0xBE, 0xEF].as_ptr(), 4));
    assert_eq!(out, "\\xdeadbeef");
    let out = take_cstring(rust_bytes_to_pg_hex([0x00, 0x00, 0x00].as_ptr(), 3));
    assert_eq!(out, "\\x000000");
    let out = take_cstring(rust_bytes_to_pg_hex([0xFF, 0xFF].as_ptr(), 2));
    assert_eq!(out, "\\xffff");
    let out = take_cstring(rust_bytes_to_pg_hex(b"AB".as_ptr(), 2));
    assert_eq!(out, "\\x4142");
}

#[test]
fn bind_helpers_bytes_to_pg_hex_png_header() {
    let blob = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let out = take_cstring(rust_bytes_to_pg_hex(blob.as_ptr(), blob.len()));
    assert_eq!(out, "\\x89504e470d0a1a0a");
}

#[test]
fn decltype_preserves_original_sqlite_strings() {
    assert_eq!(normalize_decltype(Some("DT_INTEGER(8)")), "DT_INTEGER(8)");
    assert_eq!(normalize_decltype(Some("dt_integer(8)")), "dt_integer(8)");
    assert_eq!(normalize_decltype(Some("varchar(255)")), "varchar(255)");
    assert_eq!(normalize_decltype(Some("boolean")), "boolean");
    assert_eq!(normalize_decltype(Some("INTEGER")), "INTEGER");
}

#[test]
fn decltype_preserve_empty_returns_null() {
    assert_eq!(normalize_decltype(Some("")), "");
    assert_eq!(normalize_decltype(None), "");
}

#[test]
fn pg_udt_fallback_mapping_uses_sqlite_like_decltypes() {
    let cases = [
        ("int4", "integer"),
        ("int8", "dt_integer(8)"),
        ("timestamp", "dt_integer(8)"),
        ("timestamptz", "dt_integer(8)"),
        ("bool", "boolean"),
        ("varchar", "varchar(255)"),
        ("text", "varchar(255)"),
        ("float8", "float"),
        ("bytea", "blob"),
    ];

    for (input, expected) in cases {
        let ptr = pg_udt_to_sqlite_decltype_impl(Some(input));
        let actual = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap();
        assert_eq!(actual, expected);
    }
}

#[test]
fn decltype_cache_preserves_exact_decltype_string() {
    let key = c("accounts_auto_select_subtitle");
    let decl = c("varchar(255)");

    assert_eq!(rust_decltype_cache_insert(key.as_ptr(), decl.as_ptr()), 1);
    let cached = rust_decltype_cache_lookup(key.as_ptr());
    let cached = unsafe { CStr::from_ptr(cached) }.to_str().unwrap();
    assert_eq!(cached, "varchar(255)");
}

#[test]
fn decltype_cache_alias_lookup_accepts_full_table_column_key() {
    let key = c("metadata_item_settings_last_viewed_at");
    let decl = c("dt_integer(8)");

    assert_eq!(rust_decltype_cache_insert(key.as_ptr(), decl.as_ptr()), 1);
    let cached = rust_decltype_cache_lookup_alias(key.as_ptr());
    let cached = unsafe { CStr::from_ptr(cached) }.to_str().unwrap();
    assert_eq!(cached, "dt_integer(8)");
}

#[test]
fn decltype_cache_alias_lookup_resolves_self_join_parent_alias() {
    let key = c("metadata_items_added_at");
    let decl = c("dt_integer(8)");
    let alias = c("metadata_items_parents_added_at");

    assert_eq!(rust_decltype_cache_insert(key.as_ptr(), decl.as_ptr()), 1);
    let cached = rust_decltype_cache_lookup_alias(alias.as_ptr());
    let cached = unsafe { CStr::from_ptr(cached) }.to_str().unwrap();
    assert_eq!(cached, "dt_integer(8)");
}

#[test]
fn decltype_cache_alias_lookup_resolves_self_join_text_alias() {
    let key = c("metadata_items_user_square_art_url");
    let decl = c("varchar(255)");
    let alias = c("metadata_items_parents_user_square_art_url");

    assert_eq!(rust_decltype_cache_insert(key.as_ptr(), decl.as_ptr()), 1);
    let cached = rust_decltype_cache_lookup_alias(alias.as_ptr());
    let cached = unsafe { CStr::from_ptr(cached) }.to_str().unwrap();
    assert_eq!(cached, "varchar(255)");
}

#[test]
fn type_normalization_decltype_hash_matches_djb2() {
    let s = "dt_integer(8)";
    let cs = c(s);
    assert_eq!(rust_decltype_hash(cs.as_ptr()), djb2(s));
}

#[test]
fn type_normalization_decltype_hash_null_matches_empty() {
    assert_eq!(rust_decltype_hash(std::ptr::null()), djb2(""));
}

#[test]
fn type_normalization_decltype_hash_differs_for_different_strings() {
    let a = c("INTEGER");
    let b = c("TEXT");
    assert_ne!(
        rust_decltype_hash(a.as_ptr()),
        rust_decltype_hash(b.as_ptr())
    );
}

#[test]
fn subset_fts__fts_quotes_simple_query_rewrites() {
    let sql = "SELECT * FROM metadata_items \
               JOIN fts4_metadata_titles ON metadata_items.id = fts4_metadata_titles.id \
               WHERE fts4_metadata_titles.title match 'test'";
    let out = simplify_fts_for_sqlite(sql).expect("should simplify");
    assert!(!out.to_ascii_lowercase().contains("fts4_metadata_titles"));
    assert!(out.contains("1=0"));
}

#[test]
fn subset_fts__fts_quotes_handles_apostrophes() {
    let sql = "SELECT * FROM metadata_items \
               JOIN fts4_metadata_titles ON metadata_items.id = fts4_metadata_titles.id \
               WHERE fts4_metadata_titles.title match 'it''s a test'";
    let out = simplify_fts_for_sqlite(sql).expect("should simplify");
    let out_lower = out.to_ascii_lowercase();
    assert!(out.contains("1=0"));
    assert!(!out_lower.contains(" match "));
}

#[test]
fn subset_fts__fts_quotes_handles_escaped_quote_pairs() {
    let sql = "SELECT * FROM items \
               JOIN fts4_metadata_titles ON items.id = fts4_metadata_titles.id \
               WHERE fts4_metadata_titles.title match 'can''t stop'";
    let out = simplify_fts_for_sqlite(sql).expect("should simplify");
    assert!(out.contains("1=0"));
    assert!(!out.to_ascii_lowercase().contains(" match "));
}

#[test]
fn expected_sqlite_type_for_decltype_maps_basic_types() {
    let int = c("INTEGER");
    let real = c("REAL");
    let text = c("TEXT");
    let blob = c("BLOB");
    let dt_int8 = c("DT_INTEGER(8)");

    assert_eq!(
        rust_expected_sqlite_type_for_decltype(int.as_ptr()),
        SQLITE_INTEGER_CONST
    );
    assert_eq!(
        rust_expected_sqlite_type_for_decltype(real.as_ptr()),
        SQLITE_FLOAT_CONST
    );
    assert_eq!(
        rust_expected_sqlite_type_for_decltype(text.as_ptr()),
        SQLITE_TEXT_CONST
    );
    assert_eq!(
        rust_expected_sqlite_type_for_decltype(blob.as_ptr()),
        SQLITE_BLOB_CONST
    );
    assert_eq!(
        rust_expected_sqlite_type_for_decltype(dt_int8.as_ptr()),
        SQLITE_INTEGER_CONST
    );
}

#[test]
fn expected_sqlite_type_for_decltype_unknown_returns_negative() {
    let numeric = c("NUMERIC");
    let unknown = c("WHAT");
    assert!(rust_expected_sqlite_type_for_decltype(numeric.as_ptr()) < 0);
    assert!(rust_expected_sqlite_type_for_decltype(unknown.as_ptr()) < 0);
    assert!(rust_expected_sqlite_type_for_decltype(std::ptr::null()) < 0);
}

fn text_transform(oid: u32, src: &str) -> (c_int, String) {
    let col = c("exists");
    let sql = c("SELECT EXISTS (SELECT 1 FROM t)");
    let src_c = c(src);
    let mut out = [0 as c_char; 32];
    let rc = rust_column_text_transform(
        col.as_ptr(),
        oid,
        sql.as_ptr(),
        src_c.as_ptr(),
        src.len(),
        out.as_mut_ptr(),
        out.len(),
    );
    let out_s = unsafe { CStr::from_ptr(out.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    (rc, out_s)
}

#[test]
fn column_text_transform_converts_pg_bool_to_sqlite_integer_text() {
    assert_eq!(text_transform(16, "t"), (1, "1".to_string()));
    assert_eq!(text_transform(16, "f"), (1, "0".to_string()));
}

#[test]
fn column_text_transform_leaves_non_bool_t_and_f_alone() {
    // A text column (OID 25) holding the letter "t" must not become "1".
    assert_eq!(text_transform(25, "t").0, 0);
    assert_eq!(text_transform(25, "f").0, 0);
}

#[test]
fn column_text_reformat_aggregate_int8() {
    let col = c("count");
    let sql = c("select count(*) from t");
    let src = c("123");
    let mut out = [0 as c_char; 32];

    let rc = rust_column_text_reformat_aggregate(
        col.as_ptr(),
        20,
        sql.as_ptr(),
        src.as_ptr(),
        out.as_mut_ptr(),
        out.len(),
    );

    assert_eq!(rc, 1);
    let out_s = unsafe { CStr::from_ptr(out.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    assert_eq!(out_s, "123");
}

#[test]
fn column_text_reformat_aggregate_max_of_epoch_stays_decimal() {
    // Plex's SOCI layer parses integer columns from column_text(); SQLite
    // returns max(viewed_at) as plain digits. A datetime string here makes
    // Plex read the year (2026) as an epoch and drop every show from
    // Continue Watching. The SQL mentions "_at" elsewhere (it selects
    // originally_available_at), which used to trigger the datetime rendering.
    let col = c("max");
    let sql = c(
        "SELECT grandparents.id, metadata_item_views.originally_available_at, max(viewed_at) \
         FROM metadata_item_views GROUP BY 1, 2",
    );
    let src = c("1785372260");
    let mut out = [0 as c_char; 32];

    let rc = rust_column_text_reformat_aggregate(
        col.as_ptr(),
        20,
        sql.as_ptr(),
        src.as_ptr(),
        out.as_mut_ptr(),
        out.len(),
    );

    assert_eq!(rc, 1);
    let out_s = unsafe { CStr::from_ptr(out.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    assert_eq!(out_s, "1785372260");
}

#[test]
fn column_text_reformat_aggregate_alias_expression_int8() {
    let col = c("max(year)");
    let sql = c("select max(year) from t");
    let src = c("2026");
    let mut out = [0 as c_char; 32];

    let rc = rust_column_text_reformat_aggregate(
        col.as_ptr(),
        20,
        sql.as_ptr(),
        src.as_ptr(),
        out.as_mut_ptr(),
        out.len(),
    );

    assert_eq!(rc, 1);
    let out_s = unsafe { CStr::from_ptr(out.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    assert_eq!(out_s, "2026");
}

#[test]
fn column_text_reformat_aggregate_non_match_returns_zero() {
    let col = c("id");
    let sql = c("select id from t");
    let src = c("456");
    let mut out = [0 as c_char; 32];

    let rc = rust_column_text_reformat_aggregate(
        col.as_ptr(),
        20,
        sql.as_ptr(),
        src.as_ptr(),
        out.as_mut_ptr(),
        out.len(),
    );
    assert_eq!(rc, 0);
}
