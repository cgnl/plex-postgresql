/// Module: groupby
///
/// Enforces PostgreSQL GROUP BY strict mode:
///   - All non-aggregate SELECT columns must appear in GROUP BY
///   - SELECT DISTINCT → remove GROUP BY only when no aggregate depends on it
///   - ORDER BY col that has an aggregate in SELECT → replace with aggregate expr
use sqlparser::ast::*;
use sqlparser::tokenizer::Span;

pub fn transform(stmt: &mut Statement) {
    match stmt {
        Statement::Query(q) => transform_query(q),
        _ => {}
    }
}

fn transform_query(q: &mut Query) {
    // Recurse into CTEs
    if let Some(with) = &mut q.with {
        for cte in &mut with.cte_tables {
            transform_query(&mut cte.query);
        }
    }

    // Check if this is a SELECT (not DELETE/UPDATE/INSERT subquery)
    let is_select = matches!(q.body.as_ref(), SetExpr::Select(_));

    // Collect info from SELECT body before mutating order_by
    let (has_distinct, gb_exprs, agg_map) = if let SetExpr::Select(sel) = q.body.as_ref() {
        let has_distinct = matches!(sel.distinct, Some(Distinct::Distinct));
        let gb_exprs: Vec<Expr> = match &sel.group_by {
            GroupByExpr::Expressions(exprs, _) => exprs.clone(),
            _ => vec![],
        };
        // Build map: bare column name → aggregate expression that uses it
        // e.g. max(viewed_at) → "viewed_at" → Expr::Function(max(viewed_at))
        let agg_map = build_agg_map_from_projection(&sel.projection);
        (has_distinct, gb_exprs, agg_map)
    } else {
        (false, vec![], vec![])
    };

    let has_group_by = !gb_exprs.is_empty();
    let aggregate_order = q.order_by.as_ref().is_some_and(|order| {
        matches!(&order.kind, OrderByKind::Expressions(expressions)
            if expressions.iter().any(|expression| crate::query::is_aggregate_expr(&expression.expr)))
    });
    let aggregate_projection_or_having = matches!(q.body.as_ref(), SetExpr::Select(select)
    if select.having.is_some() || select.projection.iter().any(|item| match item {
        SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => crate::query::is_aggregate_expr(expr),
        _ => false,
    }));
    let preserve_aggregate_group = aggregate_order || aggregate_projection_or_having;

    // Fix ORDER BY: if a bare column appears as arg of aggregate in SELECT,
    // replace the ORDER BY expression with the aggregate call.
    if has_group_by && !has_distinct {
        if let Some(ob) = &mut q.order_by {
            if let OrderByKind::Expressions(exprs) = &mut ob.kind {
                for oe in exprs.iter_mut() {
                    if let Some(agg_expr) = find_agg_for_expr(&oe.expr, &agg_map) {
                        oe.expr = agg_expr;
                    }
                }
            }
        }
    }

    // Now mutate the SELECT body
    if let SetExpr::Select(sel) = q.body.as_mut() {
        if has_distinct && !preserve_aggregate_group {
            // DISTINCT cannot replace grouping used by aggregate expressions.
            sel.group_by = GroupByExpr::Expressions(vec![], vec![]);
        } else if has_group_by {
            // Collect missing non-aggregate columns from SELECT projection
            let missing = collect_missing_cols(&sel.projection, &gb_exprs, &sel.from);
            if let GroupByExpr::Expressions(ref mut exprs, _) = sel.group_by {
                exprs.extend(missing);
            }
        }

        // HAVING alias expansion: replace aliases with their underlying expressions
        if sel.having.is_some() {
            let alias_map = build_alias_map(&sel.projection);
            if let Some(ref mut having) = sel.having {
                expand_aliases_in_expr(having, &alias_map);
            }
        }

        // Recurse into subqueries in FROM
        for twj in &mut sel.from {
            transform_table_with_joins(twj);
        }
    }

    // NULLS FIRST: when GROUP BY is present and no ORDER BY exists,
    // add ORDER BY 1 NULLS FIRST to ensure SOCI sees NULLs first
    // (prevents "Null value not allowed" errors).
    if is_select && has_group_by && !has_distinct && q.order_by.is_none() {
        q.order_by = Some(OrderBy {
            kind: OrderByKind::Expressions(vec![OrderByExpr {
                expr: Expr::Value(ValueWithSpan {
                    value: Value::Number("1".to_string(), false),
                    span: Span::empty(),
                }),
                options: OrderByOptions {
                    asc: None,
                    nulls_first: Some(true),
                },
                with_fill: None,
            }]),
            interpolate: None,
        });
    }

    // NULLS FIRST: when GROUP BY + existing ORDER BY, add NULLS FIRST to each expr
    if is_select && has_group_by && !has_distinct {
        if let Some(ref mut ob) = q.order_by {
            if let OrderByKind::Expressions(ref mut exprs) = ob.kind {
                for oe in exprs.iter_mut() {
                    if oe.options.nulls_first.is_none() {
                        oe.options.nulls_first = Some(true);
                    }
                }
            }
        }
    }
}

fn transform_table_with_joins(twj: &mut TableWithJoins) {
    transform_table_factor(&mut twj.relation);
    for join in &mut twj.joins {
        transform_table_factor(&mut join.relation);
    }
}

fn transform_table_factor(tf: &mut TableFactor) {
    if let TableFactor::Derived { subquery, .. } = tf {
        transform_query(subquery);
    }
}

// ─── Aggregate detection ──────────────────────────────────────────────────────

const AGGREGATE_FUNCTIONS: &[&str] = &[
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "group_concat",
    "string_agg",
    "array_agg",
    "bool_and",
    "bool_or",
    "every",
    "bit_and",
    "bit_or",
    "stddev",
    "stddev_pop",
    "stddev_samp",
    "variance",
    "var_pop",
    "var_samp",
    "percentile_cont",
    "percentile_disc",
    "first",
    "last",
];

fn is_aggregate_name(name: &str) -> bool {
    AGGREGATE_FUNCTIONS.contains(&name.to_lowercase().as_str())
}

fn func_name_str(func: &Function) -> String {
    func.name
        .0
        .first()
        .and_then(|p| match p {
            ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Returns true if the expression should be skipped from GROUP BY expansion:
/// - aggregate function calls
/// - literals (numbers, strings, booleans, null)
/// - CASE expressions
/// - subqueries
/// - non-column expressions (arithmetic etc.)
fn should_skip(expr: &Expr) -> bool {
    match expr {
        Expr::Function(func) => is_aggregate_name(&func_name_str(func)),
        Expr::Value(_) => true,
        Expr::Case { .. } => true,
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => true,
        Expr::Cast { .. } => false, // e.g. CAST(col AS type) — include
        Expr::BinaryOp { .. } | Expr::UnaryOp { .. } => true,
        Expr::Nested(inner) => should_skip(inner),
        _ => false,
    }
}

/// Extract the bare column name(s) from a SELECT item expression.
/// Returns None if the expression is not a simple column reference.
fn expr_column_key(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Identifier(ident) => Some(ident.value.to_lowercase()),
        Expr::CompoundIdentifier(parts) => {
            // e.g. t.name → "t.name"
            let s = parts
                .iter()
                .map(|p| p.value.to_lowercase())
                .collect::<Vec<_>>()
                .join(".");
            Some(s)
        }
        Expr::Nested(inner) => expr_column_key(inner),
        _ => None,
    }
}

/// Check whether `expr` is already present in `group_by` list.
fn expr_in_group_by(expr: &Expr, group_by: &[Expr]) -> bool {
    let key = expr_column_key(expr);
    for gb in group_by {
        // Direct equality
        if exprs_equivalent(expr, gb) {
            return true;
        }
        // Match by column key: handle "t.name" vs "name"
        if let (Some(ref k), Some(ref gbk)) = (&key, expr_column_key(gb)) {
            // When both are fully qualified (have dots), compare full keys
            // to avoid false matches like "a.id" == "b.id"
            if k.contains('.') && gbk.contains('.') {
                if k == gbk {
                    return true;
                }
            } else {
                // At least one is unqualified: compare bare column names
                let k_bare = k.rsplit('.').next().unwrap_or(k);
                let gbk_bare = gbk.rsplit('.').next().unwrap_or(gbk);
                if k_bare == gbk_bare {
                    return true;
                }
            }
        }
    }
    false
}

/// Check two expressions for equivalence (simplified: structural equality).
fn exprs_equivalent(a: &Expr, b: &Expr) -> bool {
    // Use Display for comparison (handles formatting differences)
    format!("{}", a).to_lowercase() == format!("{}", b).to_lowercase()
}

/// Extract table names (or aliases) from the FROM clause.
fn collect_table_names_from_from(from: &[TableWithJoins]) -> Vec<String> {
    let mut names = Vec::new();
    for twj in from {
        if let Some(name) = table_factor_name(&twj.relation) {
            names.push(name);
        }
        for join in &twj.joins {
            if let Some(name) = table_factor_name(&join.relation) {
                names.push(name);
            }
        }
    }
    names
}

/// Get the effective name (alias or table name) from a TableFactor.
fn table_factor_name(tf: &TableFactor) -> Option<String> {
    match tf {
        TableFactor::Table { name, alias, .. } => {
            if let Some(a) = alias {
                Some(a.name.value.to_lowercase())
            } else {
                name.0.last().and_then(|p| match p {
                    ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                    _ => None,
                })
            }
        }
        TableFactor::Derived { alias, .. } => alias.as_ref().map(|a| a.name.value.to_lowercase()),
        _ => None,
    }
}

/// Build a `table.id` compound identifier expression.
fn make_table_dot_id(table: &str) -> Expr {
    Expr::CompoundIdentifier(vec![Ident::new(table), Ident::new("id")])
}

/// Collect SELECT projection columns that are not aggregates and not in GROUP BY.
/// When wildcards are present alongside GROUP BY, adds `table.id` for each
/// wildcarded table so PostgreSQL's strict GROUP BY is satisfied.
fn collect_missing_cols(
    projection: &[SelectItem],
    group_by: &[Expr],
    from: &[TableWithJoins],
) -> Vec<Expr> {
    let mut missing = Vec::new();
    for item in projection {
        match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                if should_skip(e) {
                    continue;
                }
                if expr_in_group_by(e, group_by) {
                    continue;
                }
                missing.push(e.clone());
            }
            SelectItem::QualifiedWildcard(kind, _) => {
                // Extract table name from `table.*` and add `table.id` to GROUP BY
                if let SelectItemQualifiedWildcardKind::ObjectName(ref name) = kind {
                    if let Some(last) = name.0.last() {
                        let table_name = match last {
                            ObjectNamePart::Identifier(i) => i.value.to_lowercase(),
                            _ => continue,
                        };
                        let id_expr = make_table_dot_id(&table_name);
                        if !expr_in_group_by(&id_expr, group_by) {
                            missing.push(id_expr);
                        }
                    }
                }
            }
            SelectItem::Wildcard(_) => {
                // Bare `*`: add `table.id` for every table in FROM
                let table_names = collect_table_names_from_from(from);
                for table_name in &table_names {
                    let id_expr = make_table_dot_id(table_name);
                    if !expr_in_group_by(&id_expr, group_by) {
                        missing.push(id_expr);
                    }
                }
            }
        }
    }
    missing
}

// ─── ORDER BY aggregate map ───────────────────────────────────────────────────

/// Map from bare column name → aggregate expression containing that column.
/// E.g. `max(viewed_at)` in SELECT → ("viewed_at", Expr::Function(max(viewed_at)))
type AggMap = Vec<(String, Expr)>;

fn build_agg_map_from_projection(projection: &[SelectItem]) -> AggMap {
    let mut map = Vec::new();
    for item in projection {
        let expr = match item {
            SelectItem::UnnamedExpr(e) => e,
            SelectItem::ExprWithAlias { expr: e, .. } => e,
            _ => continue,
        };
        if let Expr::Function(func) = expr {
            if is_aggregate_name(&func_name_str(func)) {
                // Extract the first argument as the key
                if let FunctionArguments::List(ref al) = func.args {
                    for arg in &al.args {
                        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(inner)) = arg {
                            if let Some(key) = expr_column_key(inner) {
                                let bare_key = key.rsplit('.').next().unwrap_or(&key).to_string();
                                map.push((bare_key, expr.clone()));
                            }
                        }
                    }
                }
            }
        }
    }
    map
}

/// If `expr` (an ORDER BY expression) is a bare column that appears as an arg
/// of an aggregate in the SELECT, return the aggregate expression.
fn find_agg_for_expr(expr: &Expr, agg_map: &AggMap) -> Option<Expr> {
    let key = expr_column_key(expr)?;
    let bare_key = key.rsplit('.').next().unwrap_or(&key).to_string();
    for (col, agg_expr) in agg_map {
        if col == &bare_key {
            return Some(agg_expr.clone());
        }
    }
    None
}

// ─── HAVING alias expansion ───────────────────────────────────────────────────

/// Build a map from alias name → expression for SELECT aliases.
/// E.g. `count(media_items.id) AS cnt` → ("cnt", Expr::Function(count(…)))
fn build_alias_map(projection: &[SelectItem]) -> Vec<(String, Expr)> {
    let mut map = Vec::new();
    for item in projection {
        if let SelectItem::ExprWithAlias { expr, alias } = item {
            map.push((alias.value.to_lowercase(), expr.clone()));
        }
    }
    map
}

/// Replace bare identifier references in an expression with their aliased expressions.
/// E.g. `cnt = 0` → `count(media_items.id) = 0`
fn expand_aliases_in_expr(expr: &mut Expr, alias_map: &[(String, Expr)]) {
    match expr {
        Expr::Identifier(ident) => {
            let name = ident.value.to_lowercase();
            for (alias, replacement) in alias_map {
                if &name == alias {
                    *expr = replacement.clone();
                    return;
                }
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            expand_aliases_in_expr(left, alias_map);
            expand_aliases_in_expr(right, alias_map);
        }
        Expr::UnaryOp { expr: inner, .. } => {
            expand_aliases_in_expr(inner, alias_map);
        }
        Expr::Nested(inner) => {
            expand_aliases_in_expr(inner, alias_map);
        }
        _ => {}
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn _make_null() -> Expr {
    Expr::Value(ValueWithSpan {
        value: Value::Null,
        span: Span::empty(),
    })
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use crate::translate;

    #[test]
    fn rewrite_groupby__groupby_adds_missing_column() {
        let r = translate("SELECT id, name, title FROM t GROUP BY id").unwrap();
        let sql = r.sql.to_uppercase();
        assert!(sql.contains("GROUP BY"));
        // name and title should be added
        assert!(
            sql.contains("NAME") && sql.contains("TITLE"),
            "Expected NAME and TITLE in GROUP BY, got: {}",
            r.sql
        );
    }

    #[test]
    fn rewrite_groupby__groupby_skips_aggregate() {
        let r = translate("SELECT id, count(*) as cnt FROM t GROUP BY id").unwrap();
        let sql = r.sql.to_uppercase();
        // count(*) should NOT appear in GROUP BY
        let gb_pos = sql.find("GROUP BY").unwrap();
        let after_gb = &sql[gb_pos..];
        assert!(
            !after_gb.contains("COUNT"),
            "COUNT should not be in GROUP BY, got: {}",
            r.sql
        );
    }

    #[test]
    fn rewrite_groupby__groupby_already_complete() {
        let r = translate("SELECT id, name FROM t GROUP BY id, name").unwrap();
        // name should NOT be doubled
        let count = r.sql.to_lowercase().matches("name").count();
        // appears in SELECT and GROUP BY = 2 times, not 3
        assert!(
            count <= 2,
            "name should not be duplicated, got sql: {}",
            r.sql
        );
    }

    #[test]
    fn rewrite_groupby__groupby_distinct_removes_groupby() {
        let r = translate(
            "SELECT DISTINCT metadata_item_clusterings.id, title \
             FROM metadata_item_clusterings GROUP BY title ORDER BY title",
        )
        .unwrap();
        // DISTINCT present → GROUP BY should be removed
        assert!(
            !r.sql.to_uppercase().contains("GROUP BY"),
            "GROUP BY should be removed when DISTINCT, got: {}",
            r.sql
        );
    }

    #[test]
    fn rewrite_groupby__groupby_plex_external_metadata() {
        // Real Plex query: GROUP BY title, needs id,uri,etc. added
        let r = translate(
            "SELECT external_metadata_items.id,uri,user_title,library_section_id,\
             metadata_type,year,added_at,updated_at,extra_data,title \
             FROM external_metadata_items \
             group by title order by added_at",
        )
        .unwrap();
        let sql = r.sql.to_lowercase();
        // id should be added to GROUP BY
        let gb_pos = sql.find("group by").unwrap();
        let after_gb = &sql[gb_pos..];
        assert!(
            after_gb.contains("id") || after_gb.contains("external_metadata_items.id"),
            "id should be in GROUP BY, got: {}",
            r.sql
        );
    }

    #[test]
    fn rewrite_groupby__groupby_plex_viewed_at_order_by() {
        // When GROUP BY is present and ORDER BY uses non-aggregated col that has max() in SELECT
        let r = translate(
            "SELECT metadata_item_id, max(viewed_at) FROM metadata_item_views \
             GROUP BY metadata_item_id ORDER BY viewed_at DESC",
        )
        .unwrap();
        let sql = r.sql.to_lowercase();
        // ORDER BY viewed_at should become ORDER BY max(viewed_at)
        assert!(
            sql.contains("max(viewed_at)") || sql.contains("order by max"),
            "ORDER BY should use max(viewed_at), got: {}",
            r.sql
        );
    }

    #[test]
    fn rewrite_groupby__groupby_table_dot_column() {
        let r = translate("SELECT t.id, t.name, count(*) FROM t GROUP BY t.id").unwrap();
        let sql = r.sql.to_lowercase();
        let gb_pos = sql.find("group by").unwrap();
        let after_gb = &sql[gb_pos..];
        assert!(
            after_gb.contains("t.name") || after_gb.contains("name"),
            "t.name should be added to GROUP BY, got: {}",
            r.sql
        );
    }

    #[test]
    fn rewrite_groupby__groupby_preserves_having() {
        let r =
            translate("SELECT id, name, count(*) as cnt FROM t GROUP BY id HAVING count(*) > 1")
                .unwrap();
        assert!(
            r.sql.to_uppercase().contains("HAVING"),
            "HAVING should be preserved, got: {}",
            r.sql
        );
    }

    #[test]
    fn rewrite_groupby__groupby_no_groupby_unchanged() {
        let r = translate("SELECT id, name FROM t").unwrap();
        assert!(
            !r.sql.to_uppercase().contains("GROUP BY"),
            "Should not add GROUP BY, got: {}",
            r.sql
        );
    }

    #[test]
    fn rewrite_groupby__qualified_wildcard_adds_table_id() {
        // Real Plex pattern: SELECT table.* with GROUP BY from another table's column
        let r = translate(
            "SELECT metadata_items.*, metadata_item_settings.* \
             FROM metadata_items \
             LEFT JOIN metadata_item_settings ON metadata_item_settings.id = metadata_items.id \
             GROUP BY metadata_items.id",
        )
        .unwrap();
        let sql = r.sql.to_lowercase();
        let gb_pos = sql.find("group by").unwrap();
        let after_gb = &sql[gb_pos..];
        // metadata_item_settings.id should be added to GROUP BY
        assert!(
            after_gb.contains("metadata_item_settings.id"),
            "metadata_item_settings.id should be in GROUP BY, got: {}",
            r.sql
        );
        // metadata_items.id should already be there (it was in original GROUP BY)
        assert!(
            after_gb.contains("metadata_items.id"),
            "metadata_items.id should be in GROUP BY, got: {}",
            r.sql
        );
    }

    #[test]
    fn rewrite_groupby__bare_wildcard_adds_all_table_ids() {
        // Bare * with GROUP BY should add table.id for all FROM tables
        let r = translate(
            "SELECT * FROM metadata_items \
             LEFT JOIN metadata_item_settings ON metadata_item_settings.id = metadata_items.id \
             GROUP BY metadata_items.id",
        )
        .unwrap();
        let sql = r.sql.to_lowercase();
        let gb_pos = sql.find("group by").unwrap();
        let after_gb = &sql[gb_pos..];
        assert!(
            after_gb.contains("metadata_item_settings.id"),
            "metadata_item_settings.id should be in GROUP BY for bare *, got: {}",
            r.sql
        );
    }

    #[test]
    fn rewrite_groupby__qualified_wildcard_no_dup_when_already_in_groupby() {
        // If table.id is already in GROUP BY, don't duplicate it
        let r = translate("SELECT t.* FROM t GROUP BY t.id").unwrap();
        let sql = r.sql.to_lowercase();
        let gb_pos = sql.find("group by").unwrap();
        let after_gb = &sql[gb_pos..];
        // Count occurrences of "id" in GROUP BY — should be exactly 1
        let id_count = after_gb.matches("id").count();
        assert!(
            id_count == 1,
            "t.id should appear exactly once in GROUP BY, got: {}",
            r.sql
        );
    }
}
