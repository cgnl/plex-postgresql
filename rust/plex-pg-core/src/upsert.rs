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
    // Supported explicit REPLACE statements are emitted atomically after all
    // ordinary AST rewrites; their flags identify this bounded path.
    if matches!(insert.or, Some(SqliteOnConflict::Replace)) || insert.replace_into {
        if supported_replace_table(insert).is_some() {
            return;
        }
    }
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
    let conflict_cols = if table_name == "blobs" {
        // Two source UNIQUE indexes exist. Select a natural target only when
        // the INSERT supplies exactly one of their distinguishing keys.
        let has_id = insert
            .columns
            .iter()
            .any(|column| column.value.eq_ignore_ascii_case("linked_id"));
        let has_guid = insert
            .columns
            .iter()
            .any(|column| column.value.eq_ignore_ascii_case("linked_guid"));
        match (has_id, has_guid) {
            (true, false) => Some(vec!["linked_type", "linked_id", "blob_type"]),
            (false, true) => Some(vec!["linked_type", "linked_guid", "blob_type"]),
            _ => get_conflict_columns(&table_name),
        }
    } else {
        get_conflict_columns(&table_name)
    };
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
        // URI is a genuine natural key, but this table still has an id PK.
        // Preserve its previous rowid publication after selecting the URI key.
        if should_add_returning_id(&conflict_cols) || table_name == "external_metadata_sources" {
            insert.returning = Some(vec![SelectItem::UnnamedExpr(Expr::Identifier(Ident::new(
                "id",
            )))]);
        }
    } else if is_ignore {
        // INSERT OR IGNORE → ON CONFLICT DO NOTHING
        insert.on = Some(OnInsert::OnConflict(OnConflict {
            // SQLite OR IGNORE suppresses every unique conflict, including PK
            // collisions when a table also has a natural UNIQUE constraint.
            conflict_target: None,
            action: OnConflictAction::DoNothing,
        }));
        insert.or = None;
    }
}

const REPLACE_TABLES: &[&str] = &[
    "play_queues",
    "media_grabs",
    "media_provider_resources",
    "external_metadata_sources",
    "metadata_agent_providers",
    "metadata_agent_provider_group_items",
    "blobs",
    "metadata_item_settings",
    "statistics_bandwidth",
];

fn supported_replace_table(insert: &Insert) -> Option<String> {
    let TableObject::TableName(name) = &insert.table else {
        return None;
    };
    let ObjectNamePart::Identifier(table) = name.0.last()? else {
        return None;
    };
    let table = table.value.to_ascii_lowercase();
    REPLACE_TABLES.contains(&table.as_str()).then_some(table)
}

/// Emit only the explicitly supported single-row REPLACE forms. The executor
/// must acquire the marked table write lock BEFORE running this statement and
/// protect caller-owned transactions with a savepoint on failure.
pub fn emit_supported_replace(stmt: &Statement) -> Result<Option<String>, String> {
    let Statement::Insert(insert) = stmt else {
        return Ok(None);
    };
    if !matches!(insert.or, Some(SqliteOnConflict::Replace)) && !insert.replace_into {
        return Ok(None);
    }
    let Some(table) = supported_replace_table(insert) else {
        return Ok(None);
    };
    if insert.on.is_some() {
        return Err("REPLACE with an explicit conflict clause is unsupported".into());
    }
    if insert.columns.is_empty() {
        return Err("Supported REPLACE requires explicit columns".into());
    }
    let source = insert
        .source
        .as_ref()
        .ok_or("Supported REPLACE requires a single VALUES row")?;
    let SetExpr::Values(values) = source.body.as_ref() else {
        return Err("Supported REPLACE requires a single VALUES row".into());
    };
    if source.with.is_some()
        || values.rows.len() != 1
        || values.rows[0].len() != insert.columns.len()
    {
        return Err(
            "Supported REPLACE requires exactly one VALUES row matching its columns".into(),
        );
    }
    if insert.returning.as_ref().is_some_and(|items| items.len() != 1 || !matches!(&items[0], SelectItem::UnnamedExpr(Expr::Identifier(name)) if name.value.eq_ignore_ascii_case("id"))) {
        return Err("Supported REPLACE currently returns only id".into());
    }
    let relation = insert.table.to_string();
    let table_pattern = format!("(?s)CREATE TABLE plex\\.{table} \\((.*?)\n\\);");
    let table_regex = regex::Regex::new(&table_pattern).map_err(|error| error.to_string())?;
    let bundled = table_regex
        .captures(include_str!("../../../schema/plex_schema.sql"))
        .ok_or_else(|| format!("Missing verified REPLACE schema for {table}"))?;
    let column_regex = regex::Regex::new(
        r#"^\s*(?:"([^"]+)"|([a-z_][a-z0-9_]*))\s+([a-z]+(?: precision)?)(?:\s|,|$)"#,
    )
    .unwrap();
    let types: std::collections::HashMap<String, String> = bundled[1]
        .lines()
        .filter_map(|line| {
            let capture = column_regex.captures(line)?;
            Some((
                capture
                    .get(1)
                    .or_else(|| capture.get(2))?
                    .as_str()
                    .to_owned(),
                capture[3].to_owned(),
            ))
        })
        .collect();
    let mut supplied = std::collections::HashSet::new();
    let mut projected = Vec::new();
    let mut columns = Vec::new();
    let sequence_name = format!(
        "pg_get_serial_sequence('{}', 'id')",
        relation.replace('\'', "''")
    );
    // Preserve source AUTOINCREMENT after explicit IDs or earlier populated
    // imports without allocating against the rows that REPLACE is about to delete.
    let sequence = format!("GREATEST(nextval({sequence_name}), (SELECT COALESCE(max(id)::bigint, 0) + 1 FROM {relation}))");
    for (column, value) in insert.columns.iter().zip(&values.rows[0]) {
        let name = column.value.to_ascii_lowercase();
        if !supplied.insert(name.clone()) {
            return Err("Repeated REPLACE column is unsupported".into());
        }
        if matches!(value, Expr::Identifier(identifier) if identifier.value.eq_ignore_ascii_case("default"))
        {
            return Err("DEFAULT in REPLACE VALUES is unsupported; omit that column".into());
        }
        let datatype = types
            .get(&name)
            .ok_or_else(|| format!("Unverified REPLACE column {table}.{name}"))?;
        let expression = if name == "id" {
            format!("COALESCE(({value})::{datatype}, {sequence})::{datatype}")
        } else {
            format!("({value})::{datatype}")
        };
        projected.push(format!("{expression} AS {column}"));
        columns.push(column.to_string());
    }
    if !supplied.contains("id") {
        projected.insert(0, format!("{sequence}::integer AS id"));
        columns.insert(0, "id".into());
    }
    let mut keys: Vec<Vec<&str>> = vec![vec!["id"]];
    match table.as_str() {
        "play_queues" => keys.push(vec!["client_identifier", "account_id", "metadata_type"]),
        "media_grabs" | "media_provider_resources" => keys.push(vec!["uuid"]),
        "external_metadata_sources" => keys.push(vec!["uri"]),
        "metadata_agent_providers" => keys.push(vec!["identifier"]),
        "metadata_agent_provider_group_items" => {
            if ![
                "metadata_agent_provider_group_id",
                "metadata_agent_provider_id",
            ]
            .iter()
            .all(|key| supplied.contains(*key))
            {
                return Err("REPLACE requires both non-nullable provider group keys".into());
            }
            keys.push(vec![
                "metadata_agent_provider_group_id",
                "metadata_agent_provider_id",
            ]);
        }
        "blobs" => {
            keys.push(vec!["linked_type", "linked_id", "blob_type"]);
            keys.push(vec!["linked_type", "linked_guid", "blob_type"]);
        }
        _ => {}
    }
    // Every omitted natural key here has a verified source NULL default. A
    // partial key cannot conflict: normal equality also leaves NULLs distinct.
    let predicates = keys
        .into_iter()
        .filter(|key| {
            key.as_slice() == ["id"] || key.iter().all(|column| supplied.contains(*column))
        })
        .map(|key| {
            format!(
                "({})",
                key.iter()
                    .map(|column| format!("old.\"{column}\" = proposed.\"{column}\""))
                    .collect::<Vec<_>>()
                    .join(" AND ")
            )
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    let table_hex = relation
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let select_columns = columns
        .iter()
        .map(|column| format!("proposed.{column}"))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(Some(format!("/*PLEX_PG_REPLACE tablehex={table_hex}*/ WITH __plex_replace_input AS MATERIALIZED (SELECT {}), __plex_replace_deleted AS (DELETE FROM {relation} AS old USING __plex_replace_input AS proposed WHERE {predicates} RETURNING old.id), __plex_replace_barrier AS MATERIALIZED (SELECT count(*) FROM __plex_replace_deleted), __plex_replace_inserted AS (INSERT INTO {relation} ({}) SELECT {select_columns} FROM __plex_replace_input AS proposed CROSS JOIN __plex_replace_barrier RETURNING id), __plex_replace_sequence AS MATERIALIZED (SELECT CASE WHEN inserted.id > COALESCE(pg_sequence_last_value({sequence_name}::regclass), 0) THEN setval({sequence_name}::regclass, inserted.id, true) ELSE inserted.id::bigint END FROM __plex_replace_inserted AS inserted) SELECT inserted.id FROM __plex_replace_inserted AS inserted CROSS JOIN __plex_replace_sequence", projected.join(", "), columns.join(", "))))
}

/// Decode only generated, bounded table markers for the executor's lock guard.
pub fn replacement_lock_relation(sql: &str) -> Option<String> {
    let hex = sql
        .strip_prefix("/*PLEX_PG_REPLACE tablehex=")?
        .split_once("*/")?
        .0;
    if hex.len() % 2 != 0 || hex.len() > 1024 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).ok())
        .collect::<Option<Vec<_>>>()?;
    let relation = String::from_utf8(bytes).ok()?;
    let parsed = sqlparser::parser::Parser::parse_sql(
        &sqlparser::dialect::PostgreSqlDialect {},
        &format!("INSERT INTO {relation} (id) VALUES (1)"),
    )
    .ok()?;
    if parsed.len() != 1 {
        return None;
    }
    let Statement::Insert(insert) = &parsed[0] else {
        return None;
    };
    supported_replace_table(insert)?;
    Some(insert.table.to_string())
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
        | "statistics_bandwidth"
        | "metadata_item_settings"
        | "statistics_media"
        | "statistics_resources"
        | "devices"
        | "play_queue_items"
        | "play_queue_generators"
        | "activities"
        | "locations"
        | "plugins"
        | "versioned_metadata_items"
        | "external_metadata_items"
        | "metadata_item_views"
        | "metadata_item_accounts"
        | "metadata_item_clusterings"
        | "media_item_settings"
        | "media_subscriptions"
        | "metadata_relations"
        | "metadata_subscription_desired_items"
        | "sync_schema_versions"
        | "spellfix_metadata_titles"
        | "section_locations"
        | "hub_templates"
        | "blobs" => Some(vec!["id"]),

        // Tables with UNIQUE constraints (not PK)
        "locatables" => Some(vec!["location_id", "locatable_id", "locatable_type"]),
        "location_places" => Some(vec!["location_id", "guid"]),
        "media_stream_settings" => Some(vec!["media_stream_id", "account_id"]),
        "preferences" => Some(vec!["name"]),
        "schema_migrations" => Some(vec!["version"]),
        "play_queues" => Some(vec!["client_identifier", "account_id", "metadata_type"]),
        "media_grabs" | "media_provider_resources" => Some(vec!["uuid"]),
        "external_metadata_sources" => Some(vec!["uri"]),
        "metadata_agent_providers" => Some(vec!["identifier"]),
        "metadata_agent_provider_group_items" => Some(vec![
            "metadata_agent_provider_group_id",
            "metadata_agent_provider_id",
        ]),

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
/// Key-only inserts use one unchanged target key as a valid no-op assignment.
fn make_do_update(
    columns: Vec<Ident>,
    conflict_target: Option<ConflictTarget>,
    exclude_cols: &[String],
) -> OnConflict {
    let mut assignments: Vec<Assignment> = columns
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

    // Key-only INSERTs still need a valid DO UPDATE assignment and RETURNING
    // behavior. Keeping the key unchanged avoids invalid `DO UPDATE SET` SQL.
    if assignments.is_empty() {
        if let Some(ConflictTarget::Columns(target)) = &conflict_target {
            if let Some(column) = target.first() {
                assignments.push(Assignment {
                    target: AssignmentTarget::ColumnName(ObjectName(vec![
                        ObjectNamePart::Identifier(column.clone()),
                    ])),
                    value: Expr::CompoundIdentifier(vec![Ident::new("excluded"), column.clone()]),
                });
            }
        }
    }

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
    fn source_schema__ignore_covers_primary_and_natural_unique_conflicts() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        let source = include_str!("../../../schema/sqlite_schema.sql");
        for line in source.lines().filter(|line| {
            (line.starts_with("CREATE TABLE") || line.starts_with("CREATE UNIQUE INDEX"))
                && line.contains("\"preferences\"")
        }) {
            db.execute_batch(line).unwrap();
        }
        db.execute_batch("INSERT INTO preferences (id, name, value) VALUES (1, 'a', 'original')")
            .unwrap();
        // One conflict is on the PK with a different natural key, the other
        // on the source unique index with a different PK. Both must be ignored.
        for statement in [
            "INSERT OR IGNORE INTO preferences (id, name, value) VALUES (1, 'b', 'wrong')",
            "INSERT OR IGNORE INTO preferences (id, name, value) VALUES (2, 'a', 'wrong')",
        ] {
            let output = sql(statement).replace("plex.", "");
            assert!(
                output.to_lowercase().contains("on conflict do nothing"),
                "{output}"
            );
            db.execute_batch(&output).unwrap();
        }
        assert_eq!(
            db.query_row("SELECT count(*) FROM preferences", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            db.query_row("SELECT value FROM preferences WHERE id = 1", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            "original"
        );
    }

    #[test]
    #[ignore = "requires isolated PostgreSQL: PLEX_REPLACE_TEST_PG_URL"]
    fn source_schema__replace_differential_conflict_matrix() {
        let url = std::env::var("PLEX_REPLACE_TEST_PG_URL")
            .expect("Set PLEX_REPLACE_TEST_PG_URL to an isolated PostgreSQL 15/18 test database");
        let mut pg = postgres::Client::connect(&url, postgres::NoTls).unwrap();
        let schema = format!("replace_test_{}", std::process::id());
        pg.batch_execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema}; SET search_path={schema},public")).unwrap();
        let source = include_str!("../../../schema/sqlite_schema.sql");
        let mut failures = Vec::new();
        for (table, keys, payload) in [
            (
                "play_queues",
                vec!["client_identifier", "account_id", "metadata_type"],
                "extra_data",
            ),
            ("media_grabs", vec!["uuid"], "extra_data"),
            ("media_provider_resources", vec!["uuid"], "extra_data"),
            ("external_metadata_sources", vec!["uri"], "source_title"),
            ("metadata_agent_providers", vec!["identifier"], "title"),
            (
                "metadata_agent_provider_group_items",
                vec![
                    "metadata_agent_provider_group_id",
                    "metadata_agent_provider_id",
                ],
                "\"order\"",
            ),
            (
                "blobs",
                vec!["linked_type", "linked_id", "blob_type"],
                "created_at",
            ),
            (
                "blobs",
                vec!["linked_type", "linked_guid", "blob_type"],
                "created_at",
            ),
            (
                "blobs",
                vec!["linked_type", "linked_id", "linked_guid", "blob_type"],
                "created_at",
            ),
            (
                "metadata_item_settings",
                vec!["account_id", "guid"],
                "rating",
            ),
            (
                "statistics_bandwidth",
                vec!["account_id", "device_id", "timespan", "at", "lan"],
                "bytes",
            ),
        ] {
            let values = |number: i64| {
                keys.iter()
                    .map(|key| {
                        if table == "blobs" && *key == "linked_type" {
                            "'type'".to_string()
                        } else if table == "blobs" && *key == "blob_type" {
                            "7".to_string()
                        } else if key.ends_with("_id")
                            || ["metadata_type", "blob_type", "timespan", "at", "lan"].contains(key)
                        {
                            number.to_string()
                        } else {
                            format!("'key{number}'")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let fields = keys.join(", ");
            let extra = if table == "metadata_agent_providers" {
                (", created_at, updated_at", ", 1, 1")
            } else {
                ("", "")
            };
            let numeric_payload = ["\"order\"", "created_at", "rating", "bytes"].contains(&payload);
            let before = if numeric_payload { "9" } else { "'original'" };
            let after = if numeric_payload { "7" } else { "'changed'" };
            let mut statements = vec![
                ("PK changed natural", format!("INSERT OR REPLACE INTO {table} (id, {fields}, {payload}{}) VALUES (1, {}, {after}{})", extra.0, values(3), extra.1)),
                ("natural collision explicit new ID", format!("INSERT OR REPLACE INTO {table} (id, {fields}, {payload}{}) VALUES (7, {}, {after}{})", extra.0, values(1), extra.1)),
                ("natural collision allocated ID", format!("INSERT OR REPLACE INTO {table} ({fields}, {payload}{}) VALUES ({}, {after}{})", extra.0, values(1), extra.1)),
                ("simultaneous PK and natural collision", format!("INSERT OR REPLACE INTO {table} (id, {fields}, {payload}{}) VALUES (1, {}, {after}{})", extra.0, values(2), extra.1)),
                ("NULL ID", format!("INSERT OR REPLACE INTO {table} (id, {fields}, {payload}{}) VALUES (NULL, {}, {after}{})", extra.0, values(1), extra.1)),
                ("omitted payload resets default", format!("INSERT OR REPLACE INTO {table} (id, {fields}{}) VALUES (1, {}{})", extra.0, values(1), extra.1)),
                ("NULL natural keys", format!("INSERT OR REPLACE INTO {table} (id, {fields}, {payload}{}) VALUES (1, {}, {after}{})", extra.0, keys.iter().map(|_| "NULL").collect::<Vec<_>>().join(", "), extra.1)),
            ];
            if table == "blobs" && keys.contains(&"linked_id") && keys.contains(&"linked_guid") {
                statements.push(("simultaneous two blob natural keys", "INSERT OR REPLACE INTO blobs (id, linked_type, linked_id, linked_guid, blob_type, created_at) VALUES (7, 'type', 1, 'key2', 7, 7)".to_string()));
            }
            for (scenario, statement) in statements {
                let original = rusqlite::Connection::open_in_memory().unwrap();
                for db in [&original] {
                    for line in source.lines().filter(|line| {
                        line.starts_with("CREATE") && line.contains(&format!("\"{table}\""))
                    }) {
                        db.execute_batch(line).unwrap();
                    }
                    for id in [1, 2] {
                        db.execute_batch(&format!("INSERT INTO {table} (id, {fields}, {payload}{}) VALUES ({id}, {}, {before}{})", extra.0, values(id), extra.1)).unwrap();
                    }
                }
                let pattern =
                    regex::Regex::new(&format!("(?s)CREATE TABLE plex\\.{table} \\((.*?)\n\\);"))
                        .unwrap();
                let bundled = pattern
                    .captures(include_str!("../../../schema/plex_schema.sql"))
                    .unwrap();
                pg.batch_execute(&format!("DROP TABLE IF EXISTS {schema}.{table} CASCADE; CREATE TABLE {schema}.{table} ({}); ALTER TABLE {schema}.{table} ADD PRIMARY KEY(id); CREATE SEQUENCE {schema}.{table}_id_seq; ALTER SEQUENCE {schema}.{table}_id_seq OWNED BY {schema}.{table}.id; ALTER TABLE {schema}.{table} ALTER COLUMN id SET DEFAULT nextval('{schema}.{table}_id_seq')", &bundled[1])).unwrap();
                let natural: Vec<Vec<&str>> = match table {
                    "play_queues" => vec![vec!["client_identifier", "account_id", "metadata_type"]],
                    "media_grabs" | "media_provider_resources" => vec![vec!["uuid"]],
                    "external_metadata_sources" => vec![vec!["uri"]],
                    "metadata_agent_providers" => vec![vec!["identifier"]],
                    "metadata_agent_provider_group_items" => vec![vec![
                        "metadata_agent_provider_group_id",
                        "metadata_agent_provider_id",
                    ]],
                    "blobs" => vec![
                        vec!["linked_type", "linked_id", "blob_type"],
                        vec!["linked_type", "linked_guid", "blob_type"],
                    ],
                    _ => vec![],
                };
                for key in natural {
                    pg.batch_execute(&format!(
                        "ALTER TABLE {schema}.{table} ADD UNIQUE({})",
                        key.join(", ")
                    ))
                    .unwrap();
                }
                for id in [1, 2] {
                    pg.batch_execute(&format!("INSERT INTO {schema}.{table} (id, {fields}, {payload}{}) VALUES ({id}, {}, {before}{})", extra.0, values(id), extra.1)).unwrap();
                }
                pg.batch_execute(&format!("SELECT setval('{schema}.{table}_id_seq',2,true)"))
                    .unwrap();
                let expected = original.execute_batch(&statement);
                let output = sql(&statement).replace("plex.", "");
                let mut returned_id = None;
                pg.batch_execute(&format!("BEGIN; LOCK TABLE {schema}.{table} IN SHARE ROW EXCLUSIVE MODE; SAVEPOINT replace_statement;")).unwrap();
                let actual = pg.query(&output, &[]).map(|rows| {
                    returned_id = rows.first().map(|row| row.get::<_, i32>(0) as i64);
                });
                if actual.is_err() {
                    pg.batch_execute("ROLLBACK TO SAVEPOINT replace_statement")
                        .unwrap();
                }
                pg.batch_execute("RELEASE SAVEPOINT replace_statement; COMMIT")
                    .unwrap();
                let snapshot = |db: &rusqlite::Connection| {
                    let mut query = db
                        .prepare(&format!("SELECT * FROM {table} ORDER BY id"))
                        .unwrap();
                    let count = query.column_count();
                    query
                        .query_map([], |row| {
                            (0..count)
                                .map(|index| row.get::<_, rusqlite::types::Value>(index))
                                .collect::<rusqlite::Result<Vec<_>>>()
                        })
                        .unwrap()
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .unwrap()
                };
                let columns = original
                    .prepare(&format!("SELECT * FROM {table}"))
                    .unwrap()
                    .column_names()
                    .iter()
                    .map(|column| format!("\"{column}\"::text"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let actual_rows = pg
                    .query(
                        &format!("SELECT {columns} FROM {schema}.{table} ORDER BY id"),
                        &[],
                    )
                    .unwrap()
                    .iter()
                    .map(|row| {
                        (0..row.len())
                            .map(|index| row.get::<_, Option<String>>(index))
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>();
                let expected_rows = snapshot(&original)
                    .iter()
                    .map(|row| {
                        row.iter()
                            .map(|value| match value {
                                rusqlite::types::Value::Null => None,
                                rusqlite::types::Value::Integer(value) => Some(value.to_string()),
                                rusqlite::types::Value::Real(value) => Some(value.to_string()),
                                rusqlite::types::Value::Text(value) => Some(value.clone()),
                                rusqlite::types::Value::Blob(value) => Some(format!(
                                    "\\x{}",
                                    value
                                        .iter()
                                        .map(|byte| format!("{byte:02x}"))
                                        .collect::<String>()
                                )),
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>();
                if expected.is_ok() != actual.is_ok()
                    || expected_rows != actual_rows
                    || (expected.is_ok() && Some(original.last_insert_rowid()) != returned_id)
                {
                    failures.push(format!("{table} ({fields}): {scenario}; source={:?}; translated={:?}; source rowid={}; returned rowid={:?}; source rows={expected_rows:?}; translated rows={actual_rows:?}", expected.as_ref().err(), actual.as_ref().err(), original.last_insert_rowid(), returned_id));
                }
                if expected.is_ok() && actual.is_ok() {
                    let followup = format!(
                        "REPLACE INTO {table} ({fields}, {payload}{}) VALUES ({}, {after}{})",
                        extra.0,
                        values(4),
                        extra.1
                    );
                    original.execute_batch(&followup).unwrap();
                    pg.batch_execute(&format!(
                        "BEGIN; LOCK TABLE {schema}.{table} IN SHARE ROW EXCLUSIVE MODE"
                    ))
                    .unwrap();
                    let generated =
                        pg.query_one(&sql(&followup), &[]).unwrap().get::<_, i32>(0) as i64;
                    pg.batch_execute("COMMIT").unwrap();
                    if generated != original.last_insert_rowid() {
                        failures.push(format!(
                            "{table}: {scenario} follow-up identity {}; source {}",
                            generated,
                            original.last_insert_rowid()
                        ));
                    }
                }
            }
        }
        // Source trigger policy is explicit: no source triggers in this corpus,
        // recursive_triggers OFF, FK tests opt in to foreign_keys ON.
        let sqlite = rusqlite::Connection::open_in_memory().unwrap();
        sqlite.execute_batch("PRAGMA foreign_keys=ON; PRAGMA recursive_triggers=OFF; CREATE TABLE media_grabs(id INTEGER PRIMARY KEY AUTOINCREMENT, uuid TEXT UNIQUE, extra_data TEXT CHECK(extra_data <> 'invalid')); CREATE TABLE child(parent_id INTEGER REFERENCES media_grabs(id) ON DELETE CASCADE); CREATE TABLE earlier_work(value INTEGER); INSERT INTO media_grabs(id,uuid,extra_data) VALUES(1,'a','original'); INSERT INTO child VALUES(1);").unwrap();
        pg.batch_execute(&format!("DROP TABLE {schema}.media_grabs CASCADE; CREATE TABLE {schema}.media_grabs(id serial PRIMARY KEY,uuid text UNIQUE,extra_data text CHECK(extra_data <> 'invalid')); CREATE TABLE {schema}.child(parent_id integer REFERENCES {schema}.media_grabs(id) ON DELETE CASCADE); CREATE TABLE {schema}.earlier_work(value integer); INSERT INTO {schema}.media_grabs(id,uuid,extra_data) VALUES(1,'a','original'); INSERT INTO {schema}.child VALUES(1);")).unwrap();
        let invalid = "REPLACE INTO media_grabs(id,uuid,extra_data) VALUES(1,'b','invalid')";
        sqlite
            .execute_batch("BEGIN; INSERT INTO earlier_work VALUES(7)")
            .unwrap();
        assert!(sqlite.execute_batch(invalid).is_err());
        sqlite.execute_batch("COMMIT").unwrap();
        pg.batch_execute(&format!("BEGIN; INSERT INTO {schema}.earlier_work VALUES(7); SAVEPOINT replace_statement; LOCK TABLE {schema}.media_grabs IN SHARE ROW EXCLUSIVE MODE")).unwrap();
        assert!(pg.query(&sql(invalid), &[]).is_err());
        pg.batch_execute(
            "ROLLBACK TO SAVEPOINT replace_statement; RELEASE SAVEPOINT replace_statement; COMMIT",
        )
        .unwrap();
        assert_eq!(
            pg.query_one(&format!("SELECT count(*) FROM {schema}.earlier_work"), &[])
                .unwrap()
                .get::<_, i64>(0),
            1
        );
        assert_eq!(
            pg.query_one(&format!("SELECT count(*) FROM {schema}.child"), &[])
                .unwrap()
                .get::<_, i64>(0),
            1
        );
        assert_eq!(
            pg.query_one(
                &format!("SELECT uuid FROM {schema}.media_grabs WHERE id=1"),
                &[]
            )
            .unwrap()
            .get::<_, String>(0),
            "a"
        );
        let successful = "REPLACE INTO media_grabs(id,uuid,extra_data) VALUES(1,'b','changed')";
        sqlite.execute_batch(successful).unwrap();
        pg.batch_execute(&format!(
            "BEGIN; LOCK TABLE {schema}.media_grabs IN SHARE ROW EXCLUSIVE MODE"
        ))
        .unwrap();
        assert_eq!(
            pg.query_one(&sql(successful), &[])
                .unwrap()
                .get::<_, i32>(0),
            1
        );
        pg.batch_execute("COMMIT").unwrap();
        assert_eq!(
            sqlite
                .query_row("SELECT count(*) FROM child", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            pg.query_one(&format!("SELECT count(*) FROM {schema}.child"), &[])
                .unwrap()
                .get::<_, i64>(0),
            0
        );
        pg.batch_execute(&format!("CREATE SEQUENCE {schema}.expression_counter; BEGIN; LOCK TABLE {schema}.media_grabs IN SHARE ROW EXCLUSIVE MODE")).unwrap();
        let once = format!("REPLACE INTO media_grabs(id,uuid,extra_data) VALUES(1,'b',nextval('{schema}.expression_counter'))");
        assert_eq!(pg.query_one(&sql(&once), &[]).unwrap().get::<_, i32>(0), 1);
        pg.batch_execute("COMMIT").unwrap();
        assert_eq!(
            pg.query_one(
                &format!("SELECT last_value FROM {schema}.expression_counter"),
                &[]
            )
            .unwrap()
            .get::<_, i64>(0),
            1
        );
        let prepared = pg
            .prepare(&sql(
                "REPLACE INTO media_grabs(id,uuid,extra_data) VALUES (?, ?, ?)",
            ))
            .unwrap();
        for payload in ["prepared-first", "prepared-second"] {
            pg.batch_execute(&format!(
                "BEGIN; LOCK TABLE {schema}.media_grabs IN SHARE ROW EXCLUSIVE MODE"
            ))
            .unwrap();
            assert_eq!(
                pg.query_one(&prepared, &[&1_i32, &"b", &payload])
                    .unwrap()
                    .get::<_, i32>(0),
                1
            );
            pg.batch_execute("COMMIT").unwrap();
        }
        pg.batch_execute(&format!(
            "BEGIN; LOCK TABLE {schema}.media_grabs IN SHARE ROW EXCLUSIVE MODE"
        ))
        .unwrap();
        assert_eq!(
            pg.query_one(
                &prepared,
                &[&Option::<i32>::None, &"new", &"prepared-null-id"]
            )
            .unwrap()
            .get::<_, i32>(0),
            2
        );
        pg.batch_execute("COMMIT").unwrap();
        pg.batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .unwrap();
        assert!(
            failures.is_empty(),
            "{} REPLACE mismatches:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    #[test]
    fn bounded_replace__typed_parameters_and_quoted_relations() {
        let output = translate("INSERT OR REPLACE INTO \"plex\".\"media_grabs\" (\"id\", uuid, extra_data) VALUES (?, ?, ?)").unwrap();
        assert_eq!(output.param_names.len(), 3);
        assert!(output.sql.contains("($1)::integer"));
        assert!(output.sql.contains("($2)::text"));
        assert!(output.sql.contains("($3)::text"));
        assert_eq!(
            super::replacement_lock_relation(&output.sql).as_deref(),
            Some("\"plex\".\"media_grabs\"")
        );
        crate::translation_validation::validate_postgres_output(&output.sql).unwrap();
    }

    #[test]
    fn bounded_replace__values_evaluated_once_and_all_blob_keys_matched() {
        let output = sql("REPLACE INTO blobs(linked_type, linked_id, linked_guid, blob_type, created_at) VALUES ('type', 7, 'guid', 1, random())");
        assert_eq!(output.to_lowercase().matches("random()").count(), 1);
        assert!(output.contains("old.\"linked_id\" = proposed.\"linked_id\""));
        assert!(output.contains("old.\"linked_guid\" = proposed.\"linked_guid\""));
        assert!(!output.contains("IS NOT DISTINCT FROM"));
        crate::translation_validation::validate_postgres_output(&output).unwrap();
    }

    #[test]
    fn bounded_replace__malformed_marker_never_panics_or_locks() {
        for sql in [
            "/*PLEX_PG_REPLACE tablehex=a€bb*/",
            "/*PLEX_PG_REPLACE tablehex=00ff*/",
            "/*PLEX_PG_REPLACE tablehex=01*/",
            "SELECT 1",
        ] {
            assert!(super::replacement_lock_relation(sql).is_none());
        }
    }

    #[test]
    fn bounded_replace__rejects_unverified_shapes_before_execution() {
        for statement in [
            "REPLACE INTO media_grabs DEFAULT VALUES",
            "REPLACE INTO media_grabs(uuid) VALUES ('a'),('b')",
            "REPLACE INTO media_grabs(uuid) SELECT uuid FROM media_grabs",
            "REPLACE INTO media_grabs(uuid) VALUES (DEFAULT)",
            "REPLACE INTO metadata_agent_provider_group_items(metadata_agent_provider_id) VALUES (7)",
            "REPLACE INTO media_grabs(uuid) VALUES ('a') RETURNING uuid",
        ] { assert!(translate(statement).is_err(), "{statement}"); }
        let normal = sql("INSERT INTO media_grabs(id,uuid) VALUES (1,'a')");
        assert!(super::replacement_lock_relation(&normal).is_none());
        assert!(!normal.contains("DELETE"));
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
        let output = sql(
            "INSERT OR REPLACE INTO statistics_bandwidth (id, account_id, bytes) VALUES (1, 2, 7)",
        );
        assert_eq!(
            super::replacement_lock_relation(&output).as_deref(),
            Some("statistics_bandwidth")
        );
        assert!(output.contains("old.\"id\" = proposed.\"id\""));
        assert!(!output.contains("old.\"account_id\""));
        assert!(!output.to_lowercase().contains("on conflict"));
        assert!(output.contains("__plex_replace_barrier"));
        assert!(output.contains("RETURNING id"));
        assert!(output.ends_with("CROSS JOIN __plex_replace_sequence"));
    }

    #[test]
    fn subset_core__upsert_metadata_item_settings_conflict_and_returning() {
        let output = sql("INSERT OR REPLACE INTO metadata_item_settings (id, account_id, guid) VALUES (1, 2, 'same')");
        assert_eq!(
            super::replacement_lock_relation(&output).as_deref(),
            Some("metadata_item_settings")
        );
        assert!(output.contains("old.\"id\" = proposed.\"id\""));
        assert!(!output.contains("old.\"account_id\""));
        assert!(!output.to_lowercase().contains("on conflict"));
        assert!(output.contains("__plex_replace_barrier"));
        assert!(output.contains("RETURNING id"));
        assert!(output.ends_with("CROSS JOIN __plex_replace_sequence"));
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
