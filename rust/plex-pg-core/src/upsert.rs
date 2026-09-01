/// Module: upsert
///
/// Rewrites SQLite INSERT OR REPLACE / INSERT OR IGNORE / REPLACE INTO
/// to PostgreSQL ON CONFLICT syntax:
///
///   INSERT OR REPLACE INTO t (cols) VALUES (vals)
///     → INSERT INTO t (cols) VALUES (vals) ON CONFLICT DO UPDATE SET col1=EXCLUDED.col1, ...
///
///   INSERT OR IGNORE INTO t (cols) VALUES (vals)
///     → INSERT INTO t (cols) VALUES (vals) ON CONFLICT DO NOTHING
///
///   REPLACE INTO t (cols) VALUES (vals)
///     → same as OR REPLACE
use sqlparser::ast::*;

pub fn transform(stmt: &mut Statement) {
    if let Statement::Insert(insert) = stmt {
        transform_insert(insert);
    }
}

fn transform_insert(insert: &mut Insert) {
    // Skip if already has ON CONFLICT clause
    if insert.on.is_some() {
        return;
    }

    // Look up conflict target columns from table name
    let table_name = match &insert.table {
        TableObject::TableName(name) => name
            .0
            .last()
            .and_then(|p| match p {
                ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                _ => None,
            })
            .unwrap_or_default(),
        _ => String::new(),
    };
    let conflict_cols = get_conflict_columns(&table_name);
    let mut conflict_target = conflict_cols
        .as_ref()
        .map(|cols| ConflictTarget::Columns(cols.iter().map(|c| Ident::new(*c)).collect()));

    // Handle both INSERT OR REPLACE and REPLACE INTO (replace_into flag)
    let is_replace = matches!(insert.or, Some(SqliteOnConflict::Replace)) || insert.replace_into;
    let is_ignore = matches!(insert.or, Some(SqliteOnConflict::Ignore));

    if is_replace {
        // Fallback for unknown tables: if insert list contains id, use ON CONFLICT(id).
        if conflict_target.is_none()
            && insert
                .columns
                .iter()
                .any(|c| c.value.eq_ignore_ascii_case("id"))
        {
            conflict_target = Some(ConflictTarget::Columns(vec![Ident::new("id")]));
        }

        // INSERT OR REPLACE / REPLACE INTO → ON CONFLICT (target) DO UPDATE SET col=EXCLUDED.col, ...
        let columns = insert.columns.clone();
        let mut conflict_col_names: Vec<String> = conflict_cols
            .as_ref()
            .map(|cols| cols.iter().map(|c| c.to_lowercase()).collect())
            .unwrap_or_else(|| vec!["id".to_string()]);
        if conflict_col_names.is_empty() {
            conflict_col_names.push("id".to_string());
        }
        insert.on = Some(OnInsert::OnConflict(make_do_update(
            columns,
            conflict_target,
            &conflict_col_names,
        )));
        insert.or = None;
        insert.replace_into = false;
        // Add RETURNING id when conflict target contains "id" (matches C behavior)
        if should_add_returning_id(&conflict_cols) {
            insert.returning = Some(vec![SelectItem::UnnamedExpr(Expr::Identifier(Ident::new(
                "id",
            )))]);
        }
    } else if is_ignore {
        // INSERT OR IGNORE → ON CONFLICT DO NOTHING
        insert.on = Some(OnInsert::OnConflict(OnConflict {
            conflict_target,
            action: OnConflictAction::DoNothing,
        }));
        insert.or = None;
    } else if conflict_cols.is_some() {
        // Plain INSERT into a table where the shim has added a UNIQUE
        // constraint that SQLite did not have (see plex_schema.sql vs
        // sqlite_schema.sql for statistics_bandwidth, statistics_media,
        // metadata_item_settings, etc.). Without this branch, plain
        // `INSERT INTO statistics_bandwidth (...) VALUES (...)` succeeds
        // on SQLite (no UNIQUE) but raises `unique_violation` on PG when
        // the row already exists. Plex's caller does not catch that
        // and it propagates as `DB::Exception: std::exception`, crashing
        // PMS — observed on startup roll-up for statistics_bandwidth /
        // statistics_media on a freshly migrated PG database.
        //
        // Use ON CONFLICT DO UPDATE (not DO NOTHING). DO NOTHING returns
        // zero rows from the trailing `RETURNING id` that
        // `rust_step_cached_write_build_exec_sql` appends to every INSERT;
        // Plex's SOCI wrapper expects a row back to populate
        // `last_insert_rowid()` and throws DB::Exception on the empty
        // result, leaving the server stuck in startup Maintenance with
        // an "Uncaught exception running async task" log spam every 5s.
        // DO UPDATE preserves the row id (RETURNING id returns it) and
        // overwrites the non-key columns, which matches what Plex would
        // have done after its SELECT-or-INSERT pattern anyway: store the
        // latest measurement for this (account, device, timespan, at,
        // lan) tuple.
        let columns = insert.columns.clone();
        let conflict_col_names: Vec<String> = conflict_cols
            .as_ref()
            .map(|cols| cols.iter().map(|c| c.to_lowercase()).collect())
            .unwrap_or_default();
        insert.on = Some(OnInsert::OnConflict(make_do_update(
            columns,
            conflict_target,
            &conflict_col_names,
        )));
        if should_add_returning_id(&conflict_cols) {
            insert.returning = Some(vec![SelectItem::UnnamedExpr(Expr::Identifier(Ident::new(
                "id",
            )))]);
        }
    }
}

/// Known Plex table conflict target columns (for ON CONFLICT).
/// Returns a list of column names that form the conflict target.
/// This matches the C translator's conflict_targets[] array, which uses
/// UNIQUE constraints rather than always using the PK.
fn get_conflict_columns(table_name: &str) -> Option<Vec<&'static str>> {
    match table_name.to_lowercase().as_str() {
        // Tables with simple id PRIMARY KEY
        "tags"
        | "taggings"
        | "metadata_items"
        | "media_items"
        | "media_parts"
        | "media_streams"
        | "settings"
        | "accounts"
        | "directories"
        | "library_sections"
        | "statistics_media"
        | "statistics_resources"
        | "devices"
        | "play_queue_items"
        | "play_queue_generators"
        | "play_queues"
        | "activities"
        | "locations"
        | "plugins"
        | "media_grabs"
        | "versioned_metadata_items"
        | "external_metadata_items"
        | "external_metadata_sources"
        | "metadata_item_views"
        | "metadata_item_accounts"
        | "metadata_item_clusterings"
        | "media_item_settings"
        | "media_provider_resources"
        | "media_subscriptions"
        | "metadata_relations"
        | "metadata_subscription_desired_items"
        | "sync_schema_versions"
        | "spellfix_metadata_titles"
        | "section_locations"
        | "hub_templates"
        | "blobs" => Some(vec!["id"]),

        // Tables with UNIQUE constraints (not PK)
        "statistics_bandwidth" => Some(vec!["account_id", "device_id", "timespan", "at", "lan"]),
        "metadata_item_settings" => Some(vec!["account_id", "guid"]),
        "locatables" => Some(vec!["location_id", "locatable_id", "locatable_type"]),
        "location_places" => Some(vec!["location_id", "guid"]),
        "media_stream_settings" => Some(vec!["media_stream_id", "account_id"]),
        "preferences" => Some(vec!["name"]),
        "schema_migrations" => Some(vec!["version"]),

        _ => None,
    }
}

/// Check if RETURNING id should be added for this table's upsert.
/// Matches C behavior: add RETURNING id when "id" appears anywhere in
/// the conflict target columns string (substring match, like the C code's
/// strcasestr check). This means tables with conflict targets containing "id"
/// as a substring (e.g. "account_id") also get RETURNING id.
fn should_add_returning_id(conflict_cols: &Option<Vec<&str>>) -> bool {
    if let Some(cols) = conflict_cols {
        // Check if any conflict column contains "id" as a substring
        // (matches C behavior: strcasestr(conflict_columns, "id"))
        let joined = cols.join(", ");
        joined.to_lowercase().contains("id")
    } else {
        false
    }
}

/// Build `ON CONFLICT (target_cols) DO UPDATE SET col1 = EXCLUDED.col1, col2 = EXCLUDED.col2, ...`
/// Excludes:
///   - the `id` column (always — it's the PK and shouldn't be updated)
///   - the conflict target columns (they define the conflict, can't be updated)
fn make_do_update(
    columns: Vec<Ident>,
    conflict_target: Option<ConflictTarget>,
    exclude_cols: &[String],
) -> OnConflict {
    let assignments: Vec<Assignment> = columns
        .iter()
        .filter(|col| {
            let col_lower = col.value.to_lowercase();
            // Always skip `id` column
            if col_lower == "id" {
                return false;
            }
            // Skip conflict target columns
            !exclude_cols.iter().any(|ex| ex.to_lowercase() == col_lower)
        })
        .map(|col| Assignment {
            target: AssignmentTarget::ColumnName(ObjectName(vec![ObjectNamePart::Identifier(
                col.clone(),
            )])),
            value: Expr::CompoundIdentifier(vec![Ident::new("excluded"), col.clone()]),
        })
        .collect();

    OnConflict {
        conflict_target,
        action: OnConflictAction::DoUpdate(DoUpdate {
            assignments,
            selection: None,
        }),
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use crate::translate;

    fn sql(q: &str) -> String {
        translate(q).unwrap().sql
    }

    #[test]
    fn subset_core__upsert_insert_or_replace() {
        let r = translate("INSERT OR REPLACE INTO settings(id, value) VALUES(?, ?)").unwrap();
        assert!(r.sql.to_uppercase().contains("ON CONFLICT"));
        assert!(r.sql.to_uppercase().contains("DO UPDATE"));
        assert!(!r.sql.to_uppercase().contains("OR REPLACE"));
    }

    #[test]
    fn subset_core__upsert_insert_or_ignore() {
        let r = translate("INSERT OR IGNORE INTO tags(tag) VALUES(?)").unwrap();
        assert!(r.sql.to_uppercase().contains("ON CONFLICT"));
        assert!(r.sql.to_uppercase().contains("DO NOTHING"));
        assert!(!r.sql.to_uppercase().contains("OR IGNORE"));
    }

    #[test]
    fn subset_core__upsert_replace_into() {
        let r = translate("REPLACE INTO settings(id, value) VALUES(?, ?)").unwrap();
        assert!(r.sql.to_uppercase().contains("ON CONFLICT"));
        assert!(r.sql.to_uppercase().contains("DO UPDATE"));
        assert!(!r.sql.to_uppercase().contains("REPLACE INTO"));
    }

    #[test]
    fn subset_core__upsert_normal_insert_unchanged() {
        let r = translate("INSERT INTO t (a, b) VALUES (1, 2)").unwrap();
        assert!(!r.sql.to_uppercase().contains("ON CONFLICT"));
    }

    #[test]
    fn subset_core__upsert_on_conflict_already_present_unchanged() {
        let r = translate(
            "INSERT INTO settings(id, value) VALUES(?, ?) ON CONFLICT(id) DO UPDATE SET value = excluded.value",
        )
        .unwrap();
        assert!(r.sql.to_uppercase().contains("ON CONFLICT"));
        let count = r.sql.to_uppercase().matches("ON CONFLICT").count();
        assert_eq!(count, 1, "ON CONFLICT should appear exactly once");
    }

    #[test]
    fn subset_core__upsert_preferences_conflict_name_no_returning() {
        let out = sql("INSERT OR REPLACE INTO preferences (id, name, value) VALUES (1, 'k', 'v')");
        let low = out.to_lowercase();
        assert!(
            low.contains("on conflict (name)") || low.contains("on conflict(name)"),
            "expected ON CONFLICT(name), got: {}",
            out
        );
        assert!(low.contains("value = excluded.value"));
        assert!(!low.contains("name = excluded.name"));
        assert!(!low.contains("id = excluded.id"));
        assert!(!low.contains("returning"));
    }

    #[test]
    fn subset_core__upsert_schema_migrations_conflict_version_no_returning() {
        let out = sql("INSERT OR REPLACE INTO schema_migrations (version) VALUES ('20240101')");
        let low = out.to_lowercase();
        assert!(
            low.contains("on conflict (version)") || low.contains("on conflict(version)"),
            "expected ON CONFLICT(version), got: {}",
            out
        );
        assert!(!low.contains("returning"));
    }

    #[test]
    fn subset_core__upsert_statistics_bandwidth_conflict_and_set_exclusions() {
        let out = sql("INSERT OR REPLACE INTO statistics_bandwidth \
             (id, account_id, device_id, timespan, at, lan, bytes) \
             VALUES (1, 2, 3, 4, 5, 1, 1024)");
        let low = out.to_lowercase();
        assert!(
            low.contains("on conflict (account_id, device_id, timespan, at, lan)")
                || low.contains("on conflict(account_id, device_id, timespan, at, lan)"),
            "expected composite conflict target, got: {}",
            out
        );
        assert!(low.contains("bytes = excluded.bytes"));
        assert!(!low.contains("account_id = excluded.account_id"));
        assert!(!low.contains("device_id = excluded.device_id"));
        assert!(!low.contains("timespan = excluded.timespan"));
        assert!(!low.contains("at = excluded.at"));
        assert!(!low.contains("lan = excluded.lan"));
        assert!(!low.contains("id = excluded.id"));
        assert!(low.contains("returning id"));
    }

    #[test]
    fn subset_core__upsert_metadata_item_settings_conflict_and_returning() {
        let out = sql("INSERT OR REPLACE INTO metadata_item_settings \
             (id, account_id, guid, rating, view_count) \
             VALUES (1, 1, 'plex://movie/abc', 8.0, 5)");
        let low = out.to_lowercase();
        assert!(
            low.contains("on conflict (account_id, guid)")
                || low.contains("on conflict(account_id, guid)"),
            "expected ON CONFLICT(account_id, guid), got: {}",
            out
        );
        assert!(low.contains("rating = excluded.rating"));
        assert!(low.contains("view_count = excluded.view_count"));
        assert!(!low.contains("account_id = excluded.account_id"));
        assert!(!low.contains("guid = excluded.guid"));
        assert!(!low.contains("id = excluded.id"));
        assert!(low.contains("returning id"));
    }

    #[test]
    fn subset_core__upsert_locatables_conflict_target() {
        let out = sql("INSERT OR REPLACE INTO locatables \
             (id, location_id, locatable_id, locatable_type, created_at) \
             VALUES (1, 10, 20, 'MediaItem', 12345)");
        let low = out.to_lowercase();
        assert!(
            low.contains("on conflict (location_id, locatable_id, locatable_type)")
                || low.contains("on conflict(location_id, locatable_id, locatable_type)"),
            "expected locatables conflict target, got: {}",
            out
        );
        assert!(low.contains("created_at = excluded.created_at"));
        assert!(low.contains("returning id"));
    }

    #[test]
    fn subset_core__upsert_location_places_conflict_target() {
        let out = sql(
            "INSERT OR REPLACE INTO location_places (id, location_id, guid, name) VALUES (1, 10, 'abc', 'Home')",
        );
        let low = out.to_lowercase();
        assert!(
            low.contains("on conflict (location_id, guid)")
                || low.contains("on conflict(location_id, guid)"),
            "expected location_places conflict target, got: {}",
            out
        );
        assert!(low.contains("name = excluded.name"));
        assert!(low.contains("returning id"));
    }

    #[test]
    fn subset_core__upsert_media_stream_settings_conflict_target() {
        let out = sql("INSERT OR REPLACE INTO media_stream_settings \
             (id, media_stream_id, account_id, selected) \
             VALUES (1, 100, 1, 1)");
        let low = out.to_lowercase();
        assert!(
            low.contains("on conflict (media_stream_id, account_id)")
                || low.contains("on conflict(media_stream_id, account_id)"),
            "expected media_stream_settings conflict target, got: {}",
            out
        );
        assert!(low.contains("selected = excluded.selected"));
        assert!(low.contains("returning id"));
    }

    #[test]
    fn subset_core__upsert_schema_prefix_table_resolution() {
        let tags =
            sql("INSERT OR REPLACE INTO plex.tags (id, tag, tag_type) VALUES (1, 'Action', 0)");
        let low_tags = tags.to_lowercase();
        assert!(
            low_tags.contains("on conflict (id)") || low_tags.contains("on conflict(id)"),
            "expected schema-qualified tags to resolve to id conflict target, got: {}",
            tags
        );

        let prefs =
            sql("INSERT OR REPLACE INTO plex.preferences (id, name, value) VALUES (1, 'k', 'v')");
        let low_prefs = prefs.to_lowercase();
        assert!(
            low_prefs.contains("on conflict (name)") || low_prefs.contains("on conflict(name)"),
            "expected schema-qualified preferences to resolve to name conflict target, got: {}",
            prefs
        );
    }

    #[test]
    fn subset_core__upsert_unknown_table_fallback_no_returning() {
        let out = sql(
            "INSERT OR REPLACE INTO some_unknown_table (id, data, value) VALUES (1, 'test', 42)",
        );
        let low = out.to_lowercase();
        assert!(
            low.contains("on conflict") && low.contains("do update set"),
            "expected ON CONFLICT DO UPDATE fallback, got: {}",
            out
        );
        assert!(low.contains("data = excluded.data"));
        assert!(low.contains("value = excluded.value"));
        assert!(!low.contains("id = excluded.id"));
        assert!(!low.contains("returning"));
    }

    #[test]
    fn subset_core__upsert_unknown_table_with_id_uses_on_conflict_id_target() {
        let out = sql("INSERT OR REPLACE INTO ur (id, name, v) VALUES (1, 'a2', 99)");
        let low = out.to_lowercase();
        assert!(
            low.contains("on conflict (id)") || low.contains("on conflict(id)"),
            "expected fallback ON CONFLICT(id), got: {}",
            out
        );
        assert!(low.contains("name = excluded.name"));
        assert!(low.contains("v = excluded.v"));
    }

    #[test]
    fn subset_core__upsert_unknown_table_ignore_uses_do_nothing() {
        let out = sql("INSERT OR IGNORE INTO unknown_tbl (id, data) VALUES (1, 'test')");
        let low = out.to_lowercase();
        assert!(low.contains("on conflict"));
        assert!(low.contains("do nothing"));
        assert!(!low.contains("or ignore"));
    }

    #[test]
    fn subset_core__upsert_quoted_columns_and_trailing_semicolon() {
        let out =
            sql("INSERT OR REPLACE INTO tags (\"id\", tag, \"tag_type\") VALUES (1, 'Action', 0);");
        let low = out.to_lowercase();
        assert!(
            low.contains("on conflict (id)") || low.contains("on conflict(id)"),
            "expected ON CONFLICT(id), got: {}",
            out
        );
        assert!(low.contains("excluded.tag"));
        assert!(low.contains("excluded.\"tag_type\"") || low.contains("excluded.tag_type"));
    }

    #[test]
    fn subset_core__upsert_trailing_whitespace() {
        let out = sql("INSERT OR REPLACE INTO tags (id, tag) VALUES (1, 'Action')   ");
        let low = out.to_lowercase();
        assert!(
            low.contains("on conflict (id)") || low.contains("on conflict(id)"),
            "expected ON CONFLICT(id), got: {}",
            out
        );
        assert!(low.contains("do update set"));
    }

    #[test]
    fn subset_core__upsert_no_column_list_generates_conflict_clause() {
        let out = sql("INSERT OR REPLACE INTO tags VALUES (1, 'test', 0)");
        let low = out.to_lowercase();
        assert!(low.contains("on conflict"));
        assert!(!low.contains("or replace"));
    }

    #[test]
    fn subset_core__upsert_case_insensitive_keyword_and_table() {
        let mixed = sql("insert or replace INTO METADATA_ITEMS (id, title) VALUES (1, 'Test')");
        let low = mixed.to_lowercase();
        assert!(
            low.contains("on conflict (id)") || low.contains("on conflict(id)"),
            "expected ON CONFLICT(id), got: {}",
            mixed
        );
        assert!(low.contains("do update set"));
        assert!(!low.contains("or replace"));
    }

    #[test]
    fn subset_core__upsert_ignore_unknown_table_do_nothing() {
        let out = sql("INSERT OR IGNORE INTO unknown_tbl (id, data) VALUES (1, 'x')");
        let low = out.to_lowercase();
        assert!(low.contains("on conflict"));
        assert!(low.contains("do nothing"));
        assert!(!low.contains("or ignore"));
    }

    #[test]
    fn subset_core__upsert_regular_columns_included_and_id_excluded_in_set() {
        let out = sql("INSERT OR REPLACE INTO tags (id, tag, tag_type) VALUES (1, 'Action', 0)");
        let low = out.to_lowercase();
        assert!(low.contains("tag = excluded.tag"));
        assert!(
            low.contains("tag_type = excluded.tag_type")
                || low.contains("\"tag_type\" = excluded.\"tag_type\"")
        );
        assert!(!low.contains("id = excluded.id"));
        assert!(low.contains("returning id"));
    }

    // Regression: backtick-quoted identifiers in an explicit ON CONFLICT DO UPDATE SET clause
    // must be converted to double-quotes.  The upsert pass runs *before* the quotes pass, so
    // when the quotes pass visits the Insert statement it must walk the DoUpdate assignments.
    #[test]
    fn compat_backticks__upsert_on_conflict_set_backticks_translated() {
        // Exact form reported as broken:
        // INSERT INTO preferences (`name`,`value`) VALUES (:U1,:U2)
        //   ON CONFLICT(`name`) DO UPDATE SET `value`=excluded.`value` RETURNING `id`
        let out = sql("INSERT INTO preferences (`name`,`value`) VALUES (:U1,:U2) \
             ON CONFLICT(`name`) DO UPDATE SET `value`=excluded.`value` RETURNING `id`");
        assert!(
            !out.contains('`'),
            "all backticks should be converted to double-quotes, got: {}",
            out
        );
        // The SET target and the EXCLUDED reference should be properly double-quoted
        assert!(
            out.contains("\"value\"") || out.to_lowercase().contains("value"),
            "value column should survive translation, got: {}",
            out
        );
        assert!(
            out.to_lowercase().contains("excluded."),
            "EXCLUDED reference should be present, got: {}",
            out
        );
    }

    // Additional variant: INSERT OR REPLACE with backtick columns — the synthesised
    // DO UPDATE SET assignments must also have their backticks removed.
    #[test]
    fn compat_backticks__upsert_or_replace_backtick_columns_set_clause_translated() {
        let out = sql("INSERT OR REPLACE INTO preferences (`name`, `value`) VALUES (:U1, :U2)");
        assert!(
            !out.contains('`'),
            "backticks in synthesised DO UPDATE SET should be converted, got: {}",
            out
        );
    }

    #[test]
    fn subset_core__upsert_or_replace_or_ignore_and_replace_tokens_removed() {
        let r1 = sql("INSERT OR REPLACE INTO tags (id, tag) VALUES (1, 'test')");
        let r2 = sql("INSERT OR IGNORE INTO tags (id, tag) VALUES (1, 'test')");
        let r3 = sql("REPLACE INTO tags (id, tag) VALUES (1, 'test')");
        assert!(!r1.to_lowercase().contains("or replace"));
        assert!(!r2.to_lowercase().contains("or ignore"));
        assert!(!r3.to_lowercase().contains("replace into"));
    }
}
