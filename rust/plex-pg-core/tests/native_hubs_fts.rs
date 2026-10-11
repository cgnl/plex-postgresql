use plex_pg_core::translate;

#[test]
fn native_hubs_search_prefixes_keep_following_filters_outside_match() {
    let sql = "SELECT metadata_items.id FROM metadata_items JOIN fts4_metadata_titles_icu ON metadata_items.id=fts4_metadata_titles_icu.rowid JOIN metadata_item_accounts ON metadata_item_accounts.metadata_item_id=metadata_items.id WHERE fts4_metadata_titles_icu.title MATCH 'Big* Buck* Bunny*' AND metadata_items.metadata_type=15 AND metadata_item_accounts.account_id=1 ORDER BY metadata_items.`index` DESC,metadata_items.title_sort ASC LIMIT 4 OFFSET 0";
    let translated = translate(sql).unwrap().sql;
    assert!(
        translated.contains("to_tsquery('simple', E'Big:* & Buck:* & Bunny:*')"),
        "{translated}"
    );
    assert!(
        translated.contains(
            "AND metadata_items.metadata_type = 15 AND metadata_item_accounts.account_id = 1"
        ),
        "{translated}"
    );
}

#[test]
#[ignore = "requires isolated PostgreSQL via TEST_HUBS_PG_URL"]
fn native_hubs_search_filter_results_match_sqlite() {
    let sqlite = rusqlite::Connection::open_in_memory().unwrap();
    let fixtures = "CREATE TABLE metadata_items(id integer,title text,metadata_type integer,title_sort text,`index` integer);
        CREATE TABLE metadata_item_accounts(metadata_item_id integer,account_id integer);
        INSERT INTO metadata_items VALUES(1,'Big Buck Bunny',15,'Big Buck Bunny',1),(2,'Big Buck Bunny',15,'Big Buck Bunny',2),(3,'Big Buck Bunny',1,'Big Buck Bunny',3),(4,'Missing',15,'Missing',4),(5,'Big Buck Extra',15,'Big Buck Extra',5);
        INSERT INTO metadata_item_accounts VALUES(1,1),(2,2),(3,1),(4,1),(5,1);";
    sqlite.execute_batch(fixtures).unwrap();
    sqlite.execute_batch("CREATE VIRTUAL TABLE fts4_metadata_titles_icu USING fts4(title); INSERT INTO fts4_metadata_titles_icu(rowid,title) SELECT id,title FROM metadata_items;").unwrap();
    let mut pg =
        postgres::Client::connect(&std::env::var("TEST_HUBS_PG_URL").unwrap(), postgres::NoTls)
            .unwrap();
    let mut tx = pg.transaction().unwrap();
    tx.batch_execute(
        &fixtures
            .replace("CREATE TABLE", "CREATE TEMP TABLE")
            .replace("`index`", "\"index\""),
    )
    .unwrap();
    tx.batch_execute("CREATE TEMP VIEW fts4_metadata_titles_icu AS SELECT id AS rowid,title FROM metadata_items;").unwrap();
    for selection in [
        "fts4_metadata_titles_icu.title MATCH 'Big* Buck* Bunny*' AND metadata_items.metadata_type=15 AND metadata_item_accounts.account_id=1",
        "metadata_items.metadata_type=15 AND fts4_metadata_titles_icu.title MATCH 'Big* Buck* Bunny*' AND metadata_item_accounts.account_id=1",
        "metadata_items.metadata_type=15 AND metadata_item_accounts.account_id=1 AND fts4_metadata_titles_icu.title MATCH 'Big* Buck* Bunny*'",
        "fts4_metadata_titles_icu.title MATCH 'Big* Buck* Bunny*' AND (metadata_items.metadata_type=15 OR metadata_items.metadata_type=1) AND metadata_item_accounts.account_id=1",
    ] {
        let sql=format!("SELECT metadata_items.id FROM metadata_items JOIN fts4_metadata_titles_icu ON metadata_items.id=fts4_metadata_titles_icu.rowid JOIN metadata_item_accounts ON metadata_item_accounts.metadata_item_id=metadata_items.id WHERE {selection} ORDER BY metadata_items.`index` DESC,metadata_items.title_sort ASC LIMIT 4 OFFSET 0");
        let expected=sqlite.prepare(&sql).unwrap().query_map([],|row| row.get::<_,i32>(0)).unwrap().collect::<Result<Vec<_>,_>>().unwrap();
        let translated=translate(&sql).unwrap().sql;
        let actual=tx.query(&translated,&[]).unwrap_or_else(|e|panic!("{translated}: {e}")).iter().map(|row|row.get::<_,i32>(0)).collect::<Vec<_>>();
        assert_eq!(actual,expected,"{sql}");
    }
    let ranking_fixtures = "ALTER TABLE metadata_items ADD library_section_id integer DEFAULT 1;
        UPDATE metadata_items SET metadata_type=1;
        CREATE TABLE tags(id integer,tag text,tag_type integer);
        CREATE TABLE taggings(metadata_item_id integer,tag_id integer);
        INSERT INTO tags VALUES(10,'Big Buck Bunny',6),(20,'Big Buck Bunny Alternate',6),(30,'Big Buck Bunny',7);
        INSERT INTO taggings VALUES(1,10),(3,10),(5,10),(2,20),(1,30),(3,30);";
    sqlite.execute_batch(ranking_fixtures).unwrap();
    sqlite.execute_batch("CREATE VIRTUAL TABLE fts4_tag_titles_icu USING fts4(tag); INSERT INTO fts4_tag_titles_icu(rowid,tag) SELECT id,tag FROM tags;").unwrap();
    tx.batch_execute(&ranking_fixtures.replace("CREATE TABLE", "CREATE TEMP TABLE"))
        .unwrap();
    tx.batch_execute("CREATE TEMP VIEW fts4_tag_titles_icu AS SELECT id AS rowid,tag FROM tags;")
        .unwrap();
    let ranking_sql="SELECT DISTINCT(tags.id) FROM metadata_items JOIN taggings ON taggings.metadata_item_id=metadata_items.id JOIN tags ON tags.id=taggings.tag_id JOIN fts4_tag_titles_icu ON fts4_tag_titles_icu.rowid=tags.id WHERE fts4_tag_titles_icu.tag MATCH 'Big* Buck* Bunny*' AND tag_type=6 AND metadata_items.library_section_id IN (1) AND metadata_items.metadata_type=1 GROUP BY tags.id ORDER BY count(*) DESC LIMIT 3";
    let expected = sqlite
        .prepare(ranking_sql)
        .unwrap()
        .query_map([], |row| row.get::<_, i32>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(expected, vec![10, 20]);
    let translated = translate(ranking_sql).unwrap().sql;
    let actual = tx
        .query(&translated, &[])
        .unwrap()
        .iter()
        .map(|row| row.get::<_, i32>(0))
        .collect::<Vec<_>>();
    assert_eq!(actual, expected, "{translated}");
}

#[test]
fn native_hubs_tag_ranking_preserves_group_by() {
    let sql="SELECT DISTINCT(tags.id) FROM metadata_items JOIN taggings ON taggings.metadata_item_id=metadata_items.id JOIN tags ON tags.id=taggings.tag_id JOIN fts4_tag_titles_icu ON fts4_tag_titles_icu.rowid=tags.id WHERE fts4_tag_titles_icu.tag MATCH 'Big* Buck* Bunny*' AND tag_type=6 AND metadata_items.library_section_id IN (1) AND metadata_items.metadata_type=1 GROUP BY tags.id ORDER BY count(*) DESC LIMIT 3";
    let translated = translate(sql).unwrap().sql;
    assert!(translated.contains("GROUP BY tags.id"), "{translated}");
}
