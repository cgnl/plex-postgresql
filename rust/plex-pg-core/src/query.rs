use sqlparser::ast::helpers::attached_token::AttachedToken;
/// Module: query
///
/// Miscellaneous query fixups for SQLite → PostgreSQL translation:
///   1. Subquery alias      — FROM (SELECT …) without alias → AS _subq
///   2. NULLS LAST          — `ORDER BY col IS NULL, col ASC` → `ORDER BY col NULLS LAST`
///   3. DISTINCT + agg ORDER BY — `SELECT DISTINCT … ORDER BY count(*)` → remove DISTINCT
///   4. CASE booleans       — `CASE WHEN x THEN 1 ELSE 0 END` → TRUE/FALSE
///   5. WHERE 1 / WHERE 0   — → WHERE TRUE / WHERE FALSE
///   6. max(a,b) multi-arg  — → GREATEST(a,b)
///   7. min(a,b) multi-arg  — → LEAST(a,b)
///   8. COLLATE ICU strip   — remove icu_* collations
///   9. COLLATE NOCASE      — `x COLLATE NOCASE = y` → `LOWER(x) = LOWER(y)`
///  10. FTS4 MATCH          — `col MATCH 'term'` → `to_tsvector('simple', col) @@ to_tsquery('simple', E'term')`
///  11. Collections filter  — remove `metadata_type=18` from OR conditions
///  12. Int/text mismatch   — cast integer to ::text for known text columns
///                            and cast string literal to integer for known int columns
use sqlparser::ast::*;
use sqlparser::tokenizer::Span;

use crate::rewriter::ast_utils::{take_boxed_expr, take_expr, wrap_double_colon_cast};

pub fn transform(stmt: &mut Statement) {
    match stmt {
        Statement::Query(q) => transform_query(q),
        Statement::Insert(ins) => {
            if let Some(src) = &mut ins.source {
                transform_query(src);
            }
        }
        Statement::Update(u) => {
            if let Some(sel) = &mut u.selection {
                transform_expr(sel);
            }
        }
        Statement::Delete(d) => {
            if let Some(sel) = &mut d.selection {
                transform_expr(sel);
            }
        }
        _ => {}
    }
}

fn transform_query(q: &mut Query) {
    if let Some(with) = &mut q.with {
        for cte in &mut with.cte_tables {
            transform_query(&mut cte.query);
        }
    }

    // Check for DISTINCT + aggregate in ORDER BY → remove DISTINCT
    fix_distinct_with_agg_orderby(q);

    // For SELECT DISTINCT, ORDER BY exprs must appear in the select list (PG requirement)
    fix_distinct_orderby_not_in_select(q);

    // Normalize parentheses in ORDER BY exprs (must run after fix above)
    normalize_orderby_exprs(q);

    transform_set_expr(&mut q.body);

    // Transform expressions in ORDER BY (collate stripping, etc.)
    if let Some(ob) = &mut q.order_by {
        if let OrderByKind::Expressions(exprs) = &mut ob.kind {
            for oe in exprs.iter_mut() {
                transform_expr(&mut oe.expr);
            }
        }
    }

    // Fix ORDER BY IS NULL pattern → NULLS LAST (must run after expr transform)
    fix_order_by_is_null(q);

    // Remove ORDER BY rowid (PostgreSQL doesn't have rowid)
    fix_order_by_rowid(q);
}

fn transform_set_expr(se: &mut SetExpr) {
    match se {
        SetExpr::Select(s) => transform_select(s),
        SetExpr::Query(q) => transform_query(q),
        SetExpr::SetOperation { left, right, .. } => {
            transform_set_expr(left);
            transform_set_expr(right);
        }
        SetExpr::Values(vals) => {
            for row in &mut vals.rows {
                for e in row {
                    transform_expr(e);
                }
            }
        }
        _ => {}
    }
}

fn transform_select(sel: &mut Select) {
    // Fix FROM subqueries without alias
    for twj in &mut sel.from {
        fix_table_with_joins(twj);
    }

    // WHERE fixups
    if let Some(w) = &mut sel.selection {
        fix_where_numeric(w);
        fix_collections_filter(w);
        transform_expr(w);
    }

    // Projection
    for item in &mut sel.projection {
        match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                transform_expr(e);
            }
            _ => {}
        }
    }

    // GROUP BY
    if let GroupByExpr::Expressions(ref mut exprs, _) = sel.group_by {
        for e in exprs {
            transform_expr(e);
        }
    }

    // HAVING
    if let Some(h) = &mut sel.having {
        transform_expr(h);
    }
}

// ─── Fix 1: Subquery alias ────────────────────────────────────────────────────

fn fix_table_with_joins(twj: &mut TableWithJoins) {
    fix_table_factor(&mut twj.relation);
    for join in &mut twj.joins {
        fix_table_factor(&mut join.relation);
    }
}

fn fix_table_factor(tf: &mut TableFactor) {
    if let TableFactor::Derived {
        subquery, alias, ..
    } = tf
    {
        transform_query(subquery);
        // Add alias if missing
        if alias.is_none() {
            *alias = Some(TableAlias {
                explicit: true,
                name: Ident::new("_subq"),
                columns: vec![],
            });
        }
    }
}

// ─── Fix 2: NULLS LAST from IS NULL pattern ───────────────────────────────────

/// Detect: `ORDER BY expr IS NULL, expr [ASC|DESC]`
/// Replace with: `ORDER BY expr [ASC|DESC] NULLS LAST`
fn fix_order_by_is_null(q: &mut Query) {
    let Some(ob) = &mut q.order_by else { return };
    let OrderByKind::Expressions(exprs) = &mut ob.kind else {
        return;
    };

    // Find pairs: (i, j) where exprs[i] is `x IS NULL` and exprs[j] is `x [ASC|DESC]`
    // Strategy: collect indices of IS NULL exprs, match with following expr
    let len = exprs.len();
    if len < 2 {
        return;
    }

    let mut to_remove: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < len {
        if let Expr::IsNull(ref inner) = exprs[i].expr {
            let col = *inner.clone();
            // Look for the next expr that matches `col`
            if i + 1 < len {
                let next = &mut exprs[i + 1];
                if exprs_display_eq(&next.expr, &col) {
                    // Set NULLS LAST on the real ordering expr
                    next.options.nulls_first = Some(false);
                    to_remove.push(i);
                    i += 2;
                    continue;
                }
            }
        }
        i += 1;
    }

    // Remove IS NULL entries in reverse order
    for idx in to_remove.into_iter().rev() {
        exprs.remove(idx);
    }
}

fn exprs_display_eq(a: &Expr, b: &Expr) -> bool {
    format!("{}", a).to_lowercase() == format!("{}", b).to_lowercase()
}

// ─── Fix 11: ORDER BY rowid → removed ────────────────────────────────────────

fn fix_order_by_rowid(q: &mut Query) {
    let Some(ob) = &mut q.order_by else { return };
    let OrderByKind::Expressions(exprs) = &mut ob.kind else {
        return;
    };

    // Remove any ORDER BY expression that is just `rowid`
    exprs.retain(|oe| {
        if let Expr::Identifier(ident) = &oe.expr {
            ident.value.to_lowercase() != "rowid"
        } else {
            true
        }
    });

    // If all ORDER BY expressions were removed, remove the ORDER BY clause
    if exprs.is_empty() {
        q.order_by = None;
    }
}

// ─── Fix 3: DISTINCT + aggregate ORDER BY ─────────────────────────────────────

fn fix_distinct_with_agg_orderby(q: &mut Query) {
    let has_distinct = if let SetExpr::Select(sel) = q.body.as_ref() {
        matches!(sel.distinct, Some(Distinct::Distinct))
    } else {
        false
    };

    if !has_distinct {
        return;
    }

    let has_incompatible_in_order = if let Some(ob) = &q.order_by {
        if let OrderByKind::Expressions(exprs) = &ob.kind {
            exprs
                .iter()
                .any(|oe| is_distinct_incompatible_expr(&oe.expr))
        } else {
            false
        }
    } else {
        false
    };

    if has_incompatible_in_order {
        if let SetExpr::Select(sel) = q.body.as_mut() {
            sel.distinct = None;
        }
    }
}

// ─── Fix 3b: DISTINCT + ORDER BY expr not in select list ──────────────────────
// PostgreSQL requires all ORDER BY expressions to appear in the SELECT list
// when the query uses SELECT DISTINCT. SQLite does not have this restriction.
// We append any missing ORDER BY expressions as extra (unnamed) select items.
fn fix_distinct_orderby_not_in_select(q: &mut Query) {
    // Only apply to SELECT DISTINCT (not DISTINCT ON)
    let has_plain_distinct = if let SetExpr::Select(sel) = q.body.as_ref() {
        matches!(sel.distinct, Some(Distinct::Distinct))
    } else {
        false
    };
    if !has_plain_distinct {
        return;
    }

    let Some(ob) = &q.order_by else { return };
    let OrderByKind::Expressions(order_exprs) = &ob.kind else {
        return;
    };
    if order_exprs.is_empty() {
        return;
    }

    // Collect string representations of current select items for membership check
    let existing: Vec<String> = if let SetExpr::Select(sel) = q.body.as_ref() {
        sel.projection
            .iter()
            .map(|item| match item {
                SelectItem::UnnamedExpr(e) => expr_to_key(e),
                SelectItem::ExprWithAlias { expr, .. } => expr_to_key(expr),
                SelectItem::QualifiedWildcard(name, _) => name.to_string(),
                SelectItem::Wildcard(_) => "*".to_string(),
            })
            .collect()
    } else {
        return;
    };

    // Check if any ORDER BY expr is missing from the select list
    let has_missing = order_exprs.iter().any(|oe| {
        let key = expr_to_key(&oe.expr);
        !existing.contains(&key)
    });

    if !has_missing {
        return;
    }

    // Strategy 1: append missing ORDER BY exprs to SELECT list
    // Strategy 2: if SELECT list is a single expression (common Plex pattern like
    //   SELECT DISTINCT (id) ... ORDER BY col), append all missing exprs.
    // We always append — this is the PostgreSQL-required approach.
    let to_add: Vec<Expr> = order_exprs
        .iter()
        .filter_map(|oe| {
            let key = expr_to_key(&oe.expr);
            if existing.contains(&key) {
                None
            } else {
                Some(oe.expr.clone())
            }
        })
        .collect();

    if let SetExpr::Select(sel) = q.body.as_mut() {
        // Also unwrap parentheses from existing select items to ensure PG equality
        // e.g. SELECT DISTINCT (id) ORDER BY id — PG requires they match exactly.
        for item in sel.projection.iter_mut() {
            if let SelectItem::UnnamedExpr(Expr::Nested(inner)) = item {
                *item = SelectItem::UnnamedExpr(*inner.clone());
            }
        }
        if to_add.is_empty() {
            // All ORDER BY exprs already appear in select list — nothing to append.
            // But parens unwrap above may have changed the query, which is enough.
            return;
        }
        // Append missing ORDER BY exprs to SELECT list so PostgreSQL accepts DISTINCT.
        for expr in to_add {
            sel.projection.push(SelectItem::UnnamedExpr(expr));
        }
        // Fallback safety: if we still can't resolve (e.g. wildcard select),
        // remove DISTINCT entirely — this may return duplicates but avoids 500.
        // (Recheck after appending: if projection now has all ORDER BY exprs, keep DISTINCT.)
        // Actually we already appended them above, so DISTINCT is now valid. No need to remove.
    }
}

// ─── Fix 3c: DISTINCT + ORDER BY — strip DISTINCT when ORDER BY contains
//            expressions not derivable from select list (fallback) ─────────────
//
// For queries like:
//   SELECT DISTINCT metadata_items.id FROM ... ORDER BY metadata_items.id
// where the expr_to_key comparison may still miss some cases (e.g. qualified
// vs unqualified, aliases), we additionally strip parentheses from any
// Nested expr in the ORDER BY to match what's now in the select list.
fn normalize_orderby_exprs(q: &mut Query) {
    let Some(ob) = &mut q.order_by else { return };
    let OrderByKind::Expressions(exprs) = &mut ob.kind else {
        return;
    };
    for oe in exprs.iter_mut() {
        if let Expr::Nested(inner) = &oe.expr {
            oe.expr = *inner.clone();
        }
    }
}

/// Produce a normalised string key for an expression for membership comparison.
fn expr_to_key(expr: &Expr) -> String {
    // Unwrap a single-layer of parentheses if present (AST level)
    let inner = match expr {
        Expr::Nested(inner) => inner.as_ref(),
        other => other,
    };
    // Use the Display impl but strip whitespace and parens, lowercase for comparison
    inner
        .to_string()
        .to_lowercase()
        .replace(' ', "")
        .replace('(', "")
        .replace(')', "")
}

// ─── Fix 5: WHERE 1 / WHERE 0 ─────────────────────────────────────────────────

fn fix_where_numeric(expr: &mut Expr) {
    if let Expr::Value(ValueWithSpan {
        value: Value::Number(ref s, _),
        ref span,
    }) = *expr
    {
        if s == "1" {
            *expr = Expr::Value(ValueWithSpan {
                value: Value::Boolean(true),
                span: *span,
            });
        } else if s == "0" {
            *expr = Expr::Value(ValueWithSpan {
                value: Value::Boolean(false),
                span: *span,
            });
        }
    }
}

// ─── Main expression transformer ─────────────────────────────────────────────

/// Recursively transform an expression in place, applying all query-level
/// fixups (CASE booleans, max/min→GREATEST/LEAST, COLLATE, FTS MATCH, etc.).
fn transform_expr(expr: &mut Expr) {
    match expr {
        // Fix 4: CASE WHEN x THEN 1 ELSE 0 END → TRUE/FALSE
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            // Idempotence guard: if this already is our JSON validity guard pattern,
            // do not rewrite the result branch again on retranslations.
            let is_json_guard = operand.is_none()
                && conditions.len() == 1
                && is_json_valid_call(&conditions[0].condition)
                && matches!(
                    else_result.as_deref(),
                    Some(Expr::Value(ValueWithSpan {
                        value: Value::Null,
                        ..
                    }))
                );
            if is_json_guard {
                return;
            }
            if let Some(op) = operand {
                transform_expr(op);
            }
            for cw in conditions {
                transform_expr(&mut cw.condition);
                transform_expr(&mut cw.result);
                maybe_bool_value_inplace(&mut cw.result);
            }
            if let Some(er) = else_result {
                transform_expr(er);
                maybe_bool_value_inplace(er);
            }
        }

        // Fix 6 & 7: max(a,b) → GREATEST, min(a,b) → LEAST
        Expr::Function(ref mut func) => {
            let fn_name = func_name_str(func);
            let arg_count = if let FunctionArguments::List(ref al) = func.args {
                al.args.len()
            } else {
                0
            };

            // Recurse into args
            if let FunctionArguments::List(ref mut al) = func.args {
                for arg in &mut al.args {
                    match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                            transform_expr(e);
                        }
                        FunctionArg::Named {
                            arg: FunctionArgExpr::Expr(e),
                            ..
                        } => {
                            transform_expr(e);
                        }
                        _ => {}
                    }
                }
            }

            if fn_name == "max" && arg_count >= 2 {
                rename_function(func, "GREATEST");
            } else if fn_name == "min" && arg_count >= 2 {
                rename_function(func, "LEAST");
            }
        }

        // Fix FTS4: col MATCH 'term' → to_tsvector('simple', col) @@ to_tsquery('simple', E'converted_term')
        Expr::BinaryOp {
            op: BinaryOperator::PGCustomBinaryOperator(ref custom_ops),
            ..
        } if custom_ops.len() == 1 && custom_ops[0] == "MATCH" => {
            let taken = take_expr(expr);
            if let Expr::BinaryOp { left, right, .. } = taken {
                *expr = transform_fts_match(*left, *right);
            }
        }
        // JSON operator fix: PostgreSQL requires JSON/JSONB LHS.
        // Plex stores `extra_data` as TEXT, so cast that column to jsonb.
        Expr::BinaryOp {
            op: BinaryOperator::PGCustomBinaryOperator(ref custom_ops),
            left,
            right,
        } if custom_ops.len() == 1 && is_pg_json_op(&custom_ops[0]) => {
            let op_str = custom_ops[0].clone();
            let taken_left = take_boxed_expr(left);
            let taken_right = take_boxed_expr(right);
            *expr = rewrite_json_binary_op(
                taken_left,
                BinaryOperator::PGCustomBinaryOperator(vec![op_str]),
                taken_right,
            );
        }
        Expr::BinaryOp {
            op:
                BinaryOperator::Arrow
                | BinaryOperator::LongArrow
                | BinaryOperator::HashArrow
                | BinaryOperator::HashLongArrow,
            ..
        } => {
            let taken = take_expr(expr);
            if let Expr::BinaryOp { left, op, right } = taken {
                *expr = rewrite_json_binary_op(*left, op, *right);
            }
        }
        Expr::BinaryOp {
            op: BinaryOperator::Match,
            ..
        } => {
            let taken = take_expr(expr);
            if let Expr::BinaryOp { left, right, .. } = taken {
                *expr = transform_fts_match(*left, *right);
            }
        }

        // Fix 12 + Fix 9 + Fix 10 + general BinaryOp
        Expr::BinaryOp { left, op, right } => {
            let is_eq_op = matches!(op, BinaryOperator::Eq | BinaryOperator::NotEq);
            let is_logical_op = matches!(op, BinaryOperator::And | BinaryOperator::Or);

            // Fix 12: Int/text mismatch
            if is_eq_op && should_fix_int_text_mismatch(left, right) {
                let taken = take_expr(expr);
                if let Expr::BinaryOp { left, op, right } = taken {
                    *expr = fix_int_text_mismatch(*left, op, *right);
                }
                return;
            }

            // Fix 9: COLLATE NOCASE on equality → LOWER(x) = LOWER(y)
            let left_nocase = is_collate_nocase(left);
            let right_nocase = is_collate_nocase(right);

            if is_eq_op && (left_nocase || right_nocase) {
                strip_collate_nocase_inplace(left);
                strip_collate_nocase_inplace(right);
                transform_expr(left);
                transform_expr(right);
                wrap_lower_inplace(left);
                wrap_lower_inplace(right);
            } else if is_logical_op {
                // Fix 10: 0/1 in boolean context (AND/OR) → FALSE/TRUE
                strip_collate_nocase_inplace(left);
                strip_collate_nocase_inplace(right);
                transform_expr(left);
                transform_expr(right);
                maybe_bool_value_inplace(left);
                maybe_bool_value_inplace(right);
            } else {
                strip_collate_nocase_inplace(left);
                strip_collate_nocase_inplace(right);
                transform_expr(left);
                transform_expr(right);
            }
        }
        Expr::UnaryOp { expr: inner, .. } => transform_expr(inner),
        Expr::Nested(inner) => transform_expr(inner),
        Expr::Cast { expr: inner, .. } => transform_expr(inner),
        Expr::InList {
            expr: inner, list, ..
        } => {
            transform_expr(inner);
            for e in list {
                transform_expr(e);
            }
        }
        Expr::Between {
            expr: inner,
            low,
            high,
            ..
        } => {
            transform_expr(inner);
            transform_expr(low);
            transform_expr(high);
        }
        Expr::Collate { .. } => {
            // Fix 8: strip ICU collations entirely
            // Fix 9b: NOCASE outside equality → wrap in LOWER()
            let collation_name = match expr {
                Expr::Collate { collation, .. } => collation
                    .0
                    .first()
                    .and_then(|p| match p {
                        ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                        _ => None,
                    })
                    .unwrap_or_default(),
                _ => unreachable!(),
            };

            if collation_name.starts_with("icu") || collation_name == "unicode" {
                // Strip the collation entirely — unwrap inner expr
                let taken = take_expr(expr);
                if let Expr::Collate { expr: inner, .. } = taken {
                    *expr = *inner;
                }
                transform_expr(expr);
            } else if collation_name == "nocase" {
                // Standalone NOCASE (ORDER BY, non-equality) → LOWER(expr)
                let taken = take_expr(expr);
                if let Expr::Collate { expr: inner, .. } = taken {
                    *expr = *inner;
                }
                transform_expr(expr);
                wrap_lower_inplace(expr);
            } else {
                // Keep other collations, recurse into inner
                if let Expr::Collate { expr: inner, .. } = expr {
                    transform_expr(inner);
                }
            }
        }
        // Fix LIKE ... COLLATE NOCASE → ILIKE
        Expr::Like {
            expr: like_expr,
            pattern,
            ..
        } => {
            let pat_nocase = is_collate_nocase(pattern);
            strip_collate_nocase_inplace(pattern);
            transform_expr(like_expr);
            transform_expr(pattern);
            if pat_nocase {
                // Convert to ILIKE — need to take the whole node
                let taken = take_expr(expr);
                if let Expr::Like {
                    negated,
                    expr: e,
                    pattern: p,
                    escape_char,
                    any,
                } = taken
                {
                    *expr = Expr::ILike {
                        negated,
                        expr: e,
                        pattern: p,
                        escape_char,
                        any,
                    };
                }
            }
        }
        Expr::ILike {
            expr: ilike_expr,
            pattern,
            ..
        } => {
            // Strip NOCASE from ILIKE too (already case-insensitive)
            strip_collate_nocase_inplace(pattern);
            transform_expr(ilike_expr);
            transform_expr(pattern);
        }
        Expr::IsNull(inner) => transform_expr(inner),
        Expr::IsNotNull(inner) => transform_expr(inner),
        _ => {}
    }
}

fn is_json_valid_call(expr: &Expr) -> bool {
    match expr {
        Expr::Function(f) => {
            let name = f.name.0.first().and_then(|p| match p {
                ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                _ => None,
            });
            name.as_deref() == Some("json_valid")
        }
        _ => false,
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

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

fn rename_function(func: &mut Function, new_name: &str) {
    func.name = ObjectName(vec![ObjectNamePart::Identifier(Ident::new(new_name))]);
}

fn is_distinct_incompatible_expr(expr: &Expr) -> bool {
    // Functions that are incompatible with DISTINCT (aggregates + random)
    if let Expr::Function(func) = expr {
        let name = func_name_str(func);
        name == "random" || is_aggregate_expr(expr)
    } else {
        false
    }
}

pub(crate) fn is_aggregate_expr(expr: &Expr) -> bool {
    const AGGREGATES: &[&str] = &[
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
    ];
    if let Expr::Function(func) = expr {
        let name = func_name_str(func);
        AGGREGATES.contains(&name.as_str())
    } else {
        false
    }
}

/// If the expression is a numeric literal 1 or 0, replace with TRUE/FALSE in place.
fn maybe_bool_value_inplace(expr: &mut Expr) {
    if let Expr::Value(ValueWithSpan {
        value: Value::Number(ref s, _),
        ref span,
    }) = *expr
    {
        if s == "1" {
            *expr = Expr::Value(ValueWithSpan {
                value: Value::Boolean(true),
                span: *span,
            });
        } else if s == "0" {
            *expr = Expr::Value(ValueWithSpan {
                value: Value::Boolean(false),
                span: *span,
            });
        }
    }
}

/// Check if an expression is `COLLATE NOCASE` (shallow check).
fn is_collate_nocase(expr: &Expr) -> bool {
    if let Expr::Collate { collation, .. } = expr {
        collation
            .0
            .first()
            .and_then(|p| match p {
                ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                _ => None,
            })
            .map(|name| name == "nocase")
            .unwrap_or(false)
    } else {
        false
    }
}

/// Strip COLLATE NOCASE or ICU collation from an expression in place (shallow).
/// If the expression is `expr COLLATE NOCASE` or `expr COLLATE icu_*`, unwrap to just `expr`.
fn strip_collate_nocase_inplace(expr: &mut Expr) {
    let should_strip = if let Expr::Collate { collation, .. } = &*expr {
        let name = collation
            .0
            .first()
            .and_then(|p| match p {
                ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                _ => None,
            })
            .unwrap_or_default();
        name == "nocase" || name.starts_with("icu") || name == "unicode"
    } else {
        false
    };
    if should_strip {
        let taken = take_expr(expr);
        if let Expr::Collate { expr: inner, .. } = taken {
            *expr = *inner;
        }
    }
}

// ─── Fix 11: Collections filter ───────────────────────────────────────────────

/// Remove `metadata_type=18` from OR conditions.
/// Plex can't properly serialize collection objects (type 18), causing errors.
/// Pattern: `(metadata_type=1 OR metadata_type=18)` → `metadata_type=1`
fn fix_collections_filter(expr: &mut Expr) {
    // Recurse into nested expressions first
    match expr {
        Expr::Nested(inner) => {
            fix_collections_filter(inner);
            // After fixing inner, if it simplified to a single condition, unwrap
            if is_metadata_type_eq(inner, 18) {
                // Entire expression is just metadata_type=18 — replace with TRUE
                *expr = Expr::Value(ValueWithSpan {
                    value: Value::Boolean(true),
                    span: Span::empty(),
                });
            }
        }
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Or,
            right,
        } => {
            fix_collections_filter(left);
            fix_collections_filter(right);

            let left_is_18 = is_metadata_type_eq(left, 18);
            let right_is_18 = is_metadata_type_eq(right, 18);

            if left_is_18 && !right_is_18 {
                // Remove left (type=18), keep right
                let r = take_boxed_expr(right);
                *expr = r;
            } else if right_is_18 && !left_is_18 {
                // Remove right (type=18), keep left
                let l = take_boxed_expr(left);
                *expr = l;
            }
        }
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            fix_collections_filter(left);
            fix_collections_filter(right);
        }
        _ => {}
    }
}

/// Check if expr matches `metadata_type = <value>` or `metadata_items.metadata_type = <value>`
fn is_metadata_type_eq(expr: &Expr, value: i64) -> bool {
    if let Expr::BinaryOp {
        left,
        op: BinaryOperator::Eq,
        right,
    } = expr
    {
        let col_name = match left.as_ref() {
            Expr::Identifier(ident) => Some(ident.value.to_lowercase()),
            Expr::CompoundIdentifier(parts) => parts.last().map(|i| i.value.to_lowercase()),
            _ => None,
        };
        if col_name.as_deref() == Some("metadata_type") {
            if let Expr::Value(ValueWithSpan {
                value: Value::Number(ref s, _),
                ..
            }) = right.as_ref()
            {
                if let Ok(n) = s.parse::<i64>() {
                    return n == value;
                }
            }
        }
    }
    false
}

// ─── Fix 12: Int/text mismatch ────────────────────────────────────────────────

/// Known columns that are stored as TEXT in Plex but sometimes compared with integers
const KNOWN_TEXT_COLUMNS: &[&str] = &["status", "state", "downloaded", "metadata_item_id"];

/// Known columns that are stored as INTEGER in Plex but sometimes compared with strings
const KNOWN_INT_COLUMNS: &[&str] = &["id"];

fn get_column_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Identifier(ident) => Some(ident.value.to_lowercase()),
        Expr::CompoundIdentifier(parts) => parts.last().map(|i| i.value.to_lowercase()),
        _ => None,
    }
}

fn is_pg_json_op(op: &str) -> bool {
    op == "->" || op == "->>"
}

fn is_json_cast(expr: &Expr) -> bool {
    match expr {
        Expr::Nested(inner) => is_json_cast(inner),
        Expr::Cast { data_type, .. } => match data_type {
            DataType::JSON => true,
            DataType::Custom(name, _) => name
                .0
                .first()
                .and_then(|p| match p {
                    ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                    _ => None,
                })
                .map(|s| s == "jsonb")
                .unwrap_or(false),
            _ => false,
        },
        _ => false,
    }
}

fn make_text_cast(expr: Expr) -> Expr {
    wrap_double_colon_cast(expr, DataType::Text)
}

fn make_jsonb_cast(expr: Expr) -> Expr {
    Expr::Cast {
        kind: CastKind::DoubleColon,
        expr: Box::new(expr),
        data_type: DataType::Custom(
            ObjectName(vec![ObjectNamePart::Identifier(Ident::new("JSONB"))]),
            vec![],
        ),
        array: false,
        format: None,
    }
}

fn make_json_valid_call(expr: Expr) -> Expr {
    Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new("json_valid"))]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))],
            clauses: vec![],
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: vec![],
    })
}

fn make_function_call(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new(name))]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: args
                .into_iter()
                .map(|e| FunctionArg::Unnamed(FunctionArgExpr::Expr(e)))
                .collect(),
            clauses: vec![],
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: vec![],
    })
}

fn parse_json_path_segments(path: &str) -> Option<Vec<String>> {
    if !path.starts_with('$') {
        return None;
    }
    let bytes = path.as_bytes();
    let mut i = 1usize;
    let mut out = Vec::new();
    while i < bytes.len() {
        match bytes[i] {
            b'.' => {
                i += 1;
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric()
                        || bytes[i] == b'_'
                        || bytes[i] == b':'
                        || bytes[i] == b'-')
                {
                    i += 1;
                }
                if i == start {
                    return None;
                }
                out.push(path[start..i].to_string());
            }
            b'[' => {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i >= bytes.len() {
                    return None;
                }
                if bytes[i] == b'\'' || bytes[i] == b'"' {
                    let quote = bytes[i];
                    i += 1;
                    let start = i;
                    while i < bytes.len() && bytes[i] != quote {
                        i += 1;
                    }
                    if i >= bytes.len() {
                        return None;
                    }
                    out.push(path[start..i].to_string());
                    i += 1;
                    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                        i += 1;
                    }
                    if i >= bytes.len() || bytes[i] != b']' {
                        return None;
                    }
                    i += 1;
                    continue;
                }
                let token_start = i;
                if bytes[i] == b'-' {
                    i += 1;
                }
                let digit_start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if i == digit_start {
                    return None;
                }
                out.push(path[token_start..i].to_string());
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i >= bytes.len() || bytes[i] != b']' {
                    return None;
                }
                i += 1;
            }
            _ => return None,
        }
    }
    Some(out)
}

fn extract_json_path_segments_from_rhs(expr: &Expr) -> Option<Vec<String>> {
    if let Expr::Value(ValueWithSpan {
        value: Value::SingleQuotedString(s),
        ..
    }) = expr
    {
        return parse_json_path_segments(s);
    }
    None
}

fn is_json_extract_text_op(op: &BinaryOperator) -> bool {
    match op {
        BinaryOperator::LongArrow | BinaryOperator::HashLongArrow => true,
        BinaryOperator::PGCustomBinaryOperator(ops) if ops.len() == 1 => ops[0] == "->>",
        _ => false,
    }
}

fn rewrite_json_path_binary_op(left_jsonb: Expr, op: &BinaryOperator, segs: &[String]) -> Expr {
    let mut args = vec![left_jsonb];
    args.extend(segs.iter().cloned().map(|s| {
        Expr::Value(ValueWithSpan {
            value: Value::SingleQuotedString(s),
            span: Span::empty(),
        })
    }));
    let fn_name = if is_json_extract_text_op(op) {
        "jsonb_extract_path_text"
    } else {
        "jsonb_extract_path"
    };
    make_function_call(fn_name, args)
}

fn rewrite_json_binary_op(mut left: Expr, op: BinaryOperator, mut right: Expr) -> Expr {
    transform_expr(&mut left);
    transform_expr(&mut right);

    if is_json_cast(&left) {
        return Expr::BinaryOp {
            left: Box::new(left),
            op,
            right: Box::new(right),
        };
    }

    let left_as_text = make_text_cast(left.clone());
    let guarded_left = make_jsonb_cast(left_as_text.clone());
    let json_op_expr = if let Some(segs) = extract_json_path_segments_from_rhs(&right) {
        rewrite_json_path_binary_op(guarded_left, &op, &segs)
    } else {
        Expr::BinaryOp {
            left: Box::new(guarded_left),
            op,
            right: Box::new(right),
        }
    };

    Expr::Case {
        case_token: AttachedToken::empty(),
        end_token: AttachedToken::empty(),
        operand: None,
        conditions: vec![CaseWhen {
            condition: make_json_valid_call(left_as_text),
            result: json_op_expr,
        }],
        else_result: Some(Box::new(Expr::Value(ValueWithSpan {
            value: Value::Null,
            span: Span::empty(),
        }))),
    }
}

fn is_number_literal(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Value(ValueWithSpan {
            value: Value::Number(_, _),
            ..
        })
    )
}

fn is_string_literal(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Value(ValueWithSpan {
            value: Value::SingleQuotedString(_),
            ..
        })
    )
}

/// Check if this binary comparison has an int/text mismatch that needs fixing
fn should_fix_int_text_mismatch(left: &Expr, right: &Expr) -> bool {
    // Pattern A: known_text_col = number → cast number to text
    if let Some(col) = get_column_name(left) {
        if KNOWN_TEXT_COLUMNS.contains(&col.as_str()) && is_number_literal(right) {
            return true;
        }
    }
    if let Some(col) = get_column_name(right) {
        if KNOWN_TEXT_COLUMNS.contains(&col.as_str()) && is_number_literal(left) {
            return true;
        }
    }
    // Pattern B: known_int_col = 'string' → cast string to int
    if let Some(col) = get_column_name(left) {
        if KNOWN_INT_COLUMNS.contains(&col.as_str()) && is_string_literal(right) {
            return true;
        }
    }
    if let Some(col) = get_column_name(right) {
        if KNOWN_INT_COLUMNS.contains(&col.as_str()) && is_string_literal(left) {
            return true;
        }
    }
    false
}

/// Fix int/text mismatch by adding appropriate casts.
/// Takes ownership because the caller already extracted the parts via std::mem::replace.
fn fix_int_text_mismatch(mut left: Expr, op: BinaryOperator, mut right: Expr) -> Expr {
    let left_col = get_column_name(&left);
    let right_col = get_column_name(&right);

    // Recurse into both sides first
    transform_expr(&mut left);
    transform_expr(&mut right);

    if left_col
        .as_ref()
        .map(|c| KNOWN_TEXT_COLUMNS.contains(&c.as_str()))
        .unwrap_or(false)
        && is_number_literal(&right)
    {
        // text_col = 123 → text_col = 123::text
        Expr::BinaryOp {
            left: Box::new(left),
            op,
            right: Box::new(Expr::Cast {
                kind: CastKind::DoubleColon,
                expr: Box::new(right),
                data_type: DataType::Text,
                array: false,
                format: None,
            }),
        }
    } else if right_col
        .as_ref()
        .map(|c| KNOWN_TEXT_COLUMNS.contains(&c.as_str()))
        .unwrap_or(false)
        && is_number_literal(&left)
    {
        Expr::BinaryOp {
            left: Box::new(Expr::Cast {
                kind: CastKind::DoubleColon,
                expr: Box::new(left),
                data_type: DataType::Text,
                array: false,
                format: None,
            }),
            op,
            right: Box::new(right),
        }
    } else if left_col
        .as_ref()
        .map(|c| KNOWN_INT_COLUMNS.contains(&c.as_str()))
        .unwrap_or(false)
        && is_string_literal(&right)
    {
        // int_col = '123' → int_col = CAST('123' AS INTEGER)
        Expr::BinaryOp {
            left: Box::new(left),
            op,
            right: Box::new(Expr::Cast {
                kind: CastKind::Cast,
                expr: Box::new(right),
                data_type: DataType::Integer(None),
                array: false,
                format: None,
            }),
        }
    } else if right_col
        .as_ref()
        .map(|c| KNOWN_INT_COLUMNS.contains(&c.as_str()))
        .unwrap_or(false)
        && is_string_literal(&left)
    {
        Expr::BinaryOp {
            left: Box::new(Expr::Cast {
                kind: CastKind::Cast,
                expr: Box::new(left),
                data_type: DataType::Integer(None),
                array: false,
                format: None,
            }),
            op,
            right: Box::new(right),
        }
    } else {
        Expr::BinaryOp {
            left: Box::new(left),
            op,
            right: Box::new(right),
        }
    }
}

// ─── Fix FTS4: MATCH → @@ to_tsquery ──────────────────────────────────────────

/// Transform `col MATCH 'term'` → `to_tsvector('simple', col) @@ to_tsquery('simple', E'converted_term')`
///
/// Column MATCH uses the source text, including title_sort, original_title and tag.
/// SQLite's table-wide MATCH names the virtual table rather than a text column.
/// For the four known Plex FTS views, use their combined title_fts vector instead
/// of passing a PostgreSQL composite record to to_tsvector.
fn transform_fts_match(left: Expr, right: Expr) -> Expr {
    // sqlparser's SQLite MATCH handler parses its RHS with parse_expr(),
    // consuming following AND/OR predicates. SQLite binds MATCH above both.
    // Restore that boundary before converting the term; explicit nesting stays intact.
    if let Expr::BinaryOp {
        left: term,
        op: op @ (BinaryOperator::And | BinaryOperator::Or),
        right: mut predicate,
    } = right
    {
        transform_expr(&mut predicate);
        return Expr::BinaryOp {
            left: Box::new(transform_fts_match(left, *term)),
            op,
            right: predicate,
        };
    }

    // Build: to_tsvector('simple', col)
    let make_fn = |fn_name: &str, col: Expr| -> Expr {
        Expr::Function(Function {
            name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new(fn_name))]),
            uses_odbc_syntax: false,
            parameters: FunctionArguments::None,
            args: FunctionArguments::List(FunctionArgumentList {
                duplicate_treatment: None,
                args: vec![
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(ValueWithSpan {
                        value: Value::SingleQuotedString("simple".to_string()),
                        span: Span::empty(),
                    }))),
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(col)),
                ],
                clauses: vec![],
            }),
            filter: None,
            null_treatment: None,
            over: None,
            within_group: vec![],
        })
    };

    let table_ident = match &left {
        Expr::Identifier(ident) => Some(ident),
        _ => None,
    };
    let tsvector_call = match table_ident {
        Some(ident)
            if matches!(
                ident.value.to_ascii_lowercase().as_str(),
                "fts4_metadata_titles"
                    | "fts4_metadata_titles_icu"
                    | "fts4_tag_titles"
                    | "fts4_tag_titles_icu"
            ) =>
        {
            Expr::CompoundIdentifier(vec![ident.clone(), Ident::new("title_fts")])
        }
        _ => make_fn("to_tsvector", left),
    };

    // Convert the match term to tsquery syntax
    let term_str = match &right {
        Expr::Value(ValueWithSpan {
            value: Value::SingleQuotedString(s),
            ..
        }) => convert_fts_term(s),
        Expr::Value(ValueWithSpan {
            value: Value::Placeholder(_),
            ..
        }) => {
            // Bind normalization converts only a proven, exclusive MATCH parameter.
            return Expr::BinaryOp {
                left: Box::new(tsvector_call),
                op: BinaryOperator::AtAt,
                right: Box::new(make_fn("to_tsquery", right)),
            };
        }
        _ => {
            return Expr::BinaryOp {
                left: Box::new(tsvector_call),
                op: BinaryOperator::AtAt,
                right: Box::new(right),
            }
        }
    };

    // Build: to_tsvector('simple', col) @@ to_tsquery('simple', E'term')
    let tsquery_call = Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new("to_tsquery"))]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: vec![
                FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(ValueWithSpan {
                    value: Value::SingleQuotedString("simple".to_string()),
                    span: Span::empty(),
                }))),
                FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(ValueWithSpan {
                    value: Value::EscapedStringLiteral(term_str),
                    span: Span::empty(),
                }))),
            ],
            clauses: vec![],
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: vec![],
    });

    Expr::BinaryOp {
        left: Box::new(tsvector_call),
        op: BinaryOperator::AtAt,
        right: Box::new(tsquery_call),
    }
}

/// Prove the original SQLite parameter index is used exclusively as a direct
/// MATCH RHS on a Plex FTS view. Reusing it outside MATCH or extending the RHS
/// with an expression is explicitly unsupported, because changing that value
/// would also change a regular predicate. Quoted strings/comments are tokens,
/// not parameter occurrences. Named and numbered SQLite indexes retain their
/// original numbering independently of the PostgreSQL placeholder mapping.
pub(crate) fn fts_match_parameter(sql: &str, index: i32) -> Result<bool, &'static str> {
    use sqlparser::tokenizer::{Token, Tokenizer};
    let dialect = sqlparser::dialect::SQLiteDialect {};
    let Ok(tokens) = Tokenizer::new(&dialect, sql).tokenize() else {
        return Ok(false);
    };
    let tokens: Vec<_> = tokens
        .into_iter()
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .collect();
    let word = |token: &Token, expected: &str| matches!(token, Token::Word(w) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case(expected));
    let known_fts = |token: &Token| {
        matches!(token, Token::Word(w) if matches!(w.value.to_ascii_lowercase().as_str(),
        "fts4_metadata_titles" | "fts4_metadata_titles_icu" | "fts4_tag_titles" | "fts4_tag_titles_icu"))
    };
    let has_fts_source = tokens.iter().enumerate().any(|(i, token)| {
        (word(token, "from") || word(token, "join"))
            && (tokens.get(i + 1).is_some_and(known_fts)
                || (tokens.get(i + 2) == Some(&Token::Period)
                    && tokens.get(i + 3).is_some_and(known_fts)))
    });
    if !has_fts_source || index <= 0 {
        return Ok(false);
    }
    let mut next_index = 0;
    let mut names = std::collections::HashMap::new();
    let mut matches = 0;
    let mut ordinary = 0;
    let mut unsupported = false;
    let mut i = 0;
    while i < tokens.len() {
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
                number
                    .parse::<i32>()
                    .map_err(|_| "invalid parameter index")?
            } else {
                *names.entry(parameter).or_insert(next_index + 1)
            };
            next_index = next_index.max(parameter_index);
            if parameter_index == index {
                let mut rhs_start = i;
                while rhs_start > 0 && tokens[rhs_start - 1] == Token::LParen {
                    rhs_start -= 1;
                }
                if rhs_start < i && rhs_start > 0 && word(&tokens[rhs_start - 1], "match") {
                    matches += 1;
                    unsupported = true;
                    i += consumed;
                    continue;
                }
                let direct_match = i >= 2
                    && word(&tokens[i - 1], "match")
                    && matches!(&tokens[i - 2], Token::Word(w) if matches!(w.value.to_ascii_lowercase().as_str(),
                        "title" | "title_sort" | "original_title" | "tag" | "fts4_metadata_titles"
                        | "fts4_metadata_titles_icu" | "fts4_tag_titles" | "fts4_tag_titles_icu"));
                if direct_match {
                    matches += 1;
                    unsupported |= tokens.get(i + consumed).is_some_and(|next| {
                        !matches!(next, Token::RParen | Token::SemiColon)
                            && ![
                                "and", "or", "order", "group", "having", "limit", "offset", "union",
                            ]
                            .iter()
                            .any(|keyword| word(next, keyword))
                    });
                } else {
                    ordinary += 1;
                }
            }
        }
        i += consumed;
    }
    if matches == 0 {
        return Ok(false);
    }
    if ordinary > 0
        || unsupported
        || tokens
            .iter()
            .enumerate()
            .any(|(i, t)| *t == Token::SemiColon && i + 1 < tokens.len())
    {
        return Err("MATCH parameter reused outside MATCH or used in a compound RHS");
    }
    Ok(true)
}

/// Convert SQLite FTS term syntax to PostgreSQL tsquery syntax.
///
/// | SQLite         | PostgreSQL tsquery |
/// |----------------|-------------------|
/// | `term`         | `term`            |
/// | `term1 term2`  | `term1 & term2`   |
/// | `term*`        | `term:*`          |
/// | `-term`        | `!term`           |
/// | `term1 OR term2`| `term1 | term2`  |
/// | `"exact phrase"`| same-column weighted phrase alternatives |
/// | `'` (quote)    | stripped           |
pub(crate) fn convert_fts_term(input: &str) -> String {
    let mut result = String::new();
    let mut chars = input.chars().peekable();
    let mut need_and = false;

    while let Some(&c) = chars.peek() {
        match c {
            '*' => {
                result.push_str(":*");
                chars.next();
            }
            // Negation
            '-' => {
                if need_and {
                    result.push_str(" & ");
                }
                chars.next();
                result.push('!');
                need_and = false;
            }
            // Phrase: "word1 word2" → word1 <-> word2
            '"' => {
                if need_and {
                    result.push_str(" & ");
                }
                chars.next(); // skip opening "
                let mut phrase = String::new();
                while let Some(&pc) = chars.peek() {
                    if pc == '"' {
                        chars.next();
                        break;
                    }
                    phrase.push(pc);
                    chars.next();
                }
                let words: Vec<&str> = phrase.split_whitespace().collect();
                if !words.is_empty() {
                    // Metadata view fields use distinct A/B/C weights; ordinary
                    // single-column vectors use D. Every phrase must stay inside
                    // one field, while unquoted terms may span multiple fields.
                    let alternatives: Vec<String> = ['A', 'B', 'C', 'D']
                        .iter()
                        .map(|weight| {
                            words
                                .iter()
                                .map(|word| format!("{word}:{weight}"))
                                .collect::<Vec<_>>()
                                .join(" <-> ")
                        })
                        .collect();
                    result.push('(');
                    result.push_str(&alternatives.join(" | "));
                    result.push(')');
                    need_and = true;
                }
            }
            // Single quote — strip (not valid in tsquery)
            '\'' => {
                chars.next();
            }
            // Whitespace — check for OR or implicit AND
            ' ' | '\t' | '\n' => {
                chars.next();
                // Skip whitespace
                while let Some(&wc) = chars.peek() {
                    if wc == ' ' || wc == '\t' || wc == '\n' {
                        chars.next();
                    } else {
                        break;
                    }
                }
                // Check for OR keyword
                if chars.peek() == Some(&'O') || chars.peek() == Some(&'o') {
                    let rest: String = chars.clone().take(3).collect();
                    if rest.to_ascii_uppercase().starts_with("OR ")
                        || rest.eq_ignore_ascii_case("OR")
                    {
                        chars.next(); // O
                        chars.next(); // R
                                      // skip space after OR
                        while let Some(&wc) = chars.peek() {
                            if wc == ' ' {
                                chars.next();
                            } else {
                                break;
                            }
                        }
                        result.push_str(" | ");
                        need_and = false;
                        continue;
                    }
                }
                // Implicit AND (space between terms)
                if need_and && chars.peek().is_some() {
                    result.push_str(" & ");
                    need_and = false;
                }
            }
            // Regular character — part of a term
            _ => {
                if !need_and {
                    // First char of a new term
                }
                result.push(c);
                chars.next();
                need_and = true;
            }
        }
    }
    result
}

/// Wrap an expression in LOWER(…) in place.
fn wrap_lower_inplace(expr: &mut Expr) {
    let inner = take_expr(expr);
    *expr = wrap_lower(inner);
}

/// Wrap an expression in LOWER(…)
fn wrap_lower(expr: Expr) -> Expr {
    Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new("LOWER"))]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))],
            clauses: vec![],
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: vec![],
    })
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use crate::translate;

    #[test]
    fn compat_aliases__query_subquery_gets_alias() {
        let r = translate("SELECT * FROM (SELECT id FROM t) WHERE id > 0").unwrap();
        let sql_up = r.sql.to_uppercase();
        assert!(
            sql_up.contains("AS ") || sql_up.contains(" AS"),
            "Subquery should have alias, got: {}",
            r.sql
        );
    }

    #[test]
    fn subset_core__query_nulls_last_from_is_null_pattern() {
        let r =
            translate(r#"SELECT * FROM t ORDER BY parents."index" IS NULL, parents."index" asc"#)
                .unwrap();
        let sql = r.sql.to_uppercase();
        assert!(
            sql.contains("NULLS LAST"),
            "Expected NULLS LAST, got: {}",
            r.sql
        );
        assert!(
            !sql.contains("IS NULL"),
            "IS NULL pattern should be replaced, got: {}",
            r.sql
        );
    }

    #[test]
    fn subset_core__query_case_then_1_else_0_to_booleans() {
        let r = translate("SELECT (CASE WHEN a THEN 1 ELSE 0 END) FROM t").unwrap();
        let sql = r.sql.to_lowercase();
        assert!(
            sql.contains("true") || sql.contains("false"),
            "Expected boolean values, got: {}",
            r.sql
        );
    }

    #[test]
    fn subset_core__query_where_0_to_false() {
        let r = translate("SELECT * FROM t WHERE 0").unwrap();
        assert!(r.sql.to_uppercase().contains("FALSE"), "Got: {}", r.sql);
    }

    #[test]
    fn subset_core__query_where_1_to_true() {
        let r = translate("SELECT * FROM t WHERE 1").unwrap();
        assert!(r.sql.to_uppercase().contains("TRUE"), "Got: {}", r.sql);
    }

    #[test]
    fn subset_core__query_max_two_args_to_greatest() {
        let r = translate("SELECT max(x, y) FROM t").unwrap();
        assert!(r.sql.to_uppercase().contains("GREATEST"), "Got: {}", r.sql);
    }

    #[test]
    fn subset_core__query_max_one_arg_preserved() {
        let r = translate("SELECT max(x) FROM t").unwrap();
        assert!(!r.sql.to_uppercase().contains("GREATEST"), "Got: {}", r.sql);
        assert!(r.sql.to_lowercase().contains("max("), "Got: {}", r.sql);
    }

    #[test]
    fn subset_core__query_min_two_args_to_least() {
        let r = translate("SELECT min(x, y) FROM t").unwrap();
        assert!(r.sql.to_uppercase().contains("LEAST"), "Got: {}", r.sql);
    }

    #[test]
    fn subset_core__query_strip_icu_collation() {
        let r = translate("SELECT * FROM t ORDER BY name COLLATE icu_root").unwrap();
        assert!(!r.sql.to_lowercase().contains("icu_root"), "Got: {}", r.sql);
    }

    #[test]
    fn subset_core__query_collate_nocase_to_lower() {
        let r = translate("SELECT * FROM t WHERE name COLLATE NOCASE = 'Test'").unwrap();
        let sql = r.sql.to_lowercase();
        assert!(sql.contains("lower("), "Expected LOWER(), got: {}", r.sql);
        assert!(
            !sql.contains("collate nocase"),
            "COLLATE NOCASE should be removed, got: {}",
            r.sql
        );
    }

    #[test]
    fn subset_core__query_orderby_unchanged_without_isnull() {
        let r = translate("SELECT * FROM t ORDER BY id").unwrap();
        assert!(
            !r.sql.to_uppercase().contains("NULLS LAST"),
            "Should not add NULLS LAST, got: {}",
            r.sql
        );
    }

    #[test]
    fn subset_json__query_json_op_uses_safe_jsonb_cast() {
        let r = translate(
            "SELECT * FROM media_streams st WHERE st.extra_data ->> '$.pv:version' < '1'",
        )
        .unwrap();
        let sql = r.sql.to_lowercase();
        assert!(
            sql.contains("json_valid")
                && sql.contains("::jsonb")
                && (sql.contains("jsonb_extract_path_text") || sql.contains("->>")),
            "Expected guarded json rewrite, got: {}",
            r.sql
        );
    }

    #[test]
    fn subset_json__query_json_op_uses_safe_cast_on_unrelated_column_too() {
        let r = translate("SELECT * FROM t WHERE t.title ->> '$.key' = 'x'").unwrap();
        let sql = r.sql.to_lowercase();
        assert!(
            sql.contains("json_valid") && sql.contains("title") && sql.contains("::jsonb"),
            "Expected generic guarded rewrite for JSON operator, got: {}",
            r.sql
        );
    }

    #[test]
    fn subset_json__query_json_op_sqlite_path_literal_rewrites_to_extract_path_text() {
        let r = translate("SELECT extra_data ->> '$.status' FROM t").unwrap();
        let sql = r.sql.to_lowercase();
        assert!(
            sql.contains("jsonb_extract_path_text") && sql.contains("'status'"),
            "Expected $.status operator path rewrite, got: {}",
            r.sql
        );
    }

    // FTS MATCH tests
    //
    // The old strategy appended `_fts` to the matched column name (e.g.
    // `title_sort` → `title_sort_fts`), which produced nonexistent column
    // references.  The correct strategy wraps the source column with
    // `to_tsvector('simple', col)` so any column name is valid.

    #[test]
    fn subset_fts__match_simple_col_uses_to_tsvector_wrapper() {
        let r = translate("SELECT * FROM fts4_metadata_titles WHERE title MATCH 'Plex'").unwrap();
        let sql = r.sql.to_lowercase();
        assert!(
            sql.contains("to_tsvector("),
            "Expected to_tsvector() wrapper, got: {}",
            r.sql
        );
        assert!(
            !sql.contains("title_fts"),
            "Should not produce _fts suffix column, got: {}",
            r.sql
        );
        assert!(
            sql.contains("to_tsquery("),
            "Expected to_tsquery() call, got: {}",
            r.sql
        );
        assert!(sql.contains("@@"), "Expected @@ operator, got: {}", r.sql);
    }

    #[test]
    fn subset_fts__match_compound_title_sort_no_nonexistent_suffix() {
        // Regression: `fts4_metadata_titles_icu.title_sort MATCH 'Straf*'`
        // must NOT produce `title_sort_fts` (nonexistent column).
        let r = translate(
            "SELECT rowid FROM fts4_metadata_titles_icu \
             WHERE fts4_metadata_titles_icu.title_sort match 'Straf*'",
        )
        .unwrap();
        let sql = r.sql.to_lowercase();
        assert!(
            !sql.contains("title_sort_fts"),
            "Must not produce nonexistent title_sort_fts column, got: {}",
            r.sql
        );
        assert!(
            sql.contains("to_tsvector("),
            "Expected to_tsvector() wrapper, got: {}",
            r.sql
        );
        assert!(
            sql.contains("title_sort"),
            "Expected original title_sort column inside to_tsvector, got: {}",
            r.sql
        );
        assert!(sql.contains("@@"), "Expected @@ operator, got: {}", r.sql);
    }

    #[test]
    fn subset_fts__match_prefix_query_converts_star_to_colon_star() {
        let r =
            translate("SELECT rowid FROM fts4_metadata_titles WHERE title MATCH 'War*'").unwrap();
        let sql = r.sql.to_lowercase();
        assert!(
            sql.contains("war:*"),
            "Expected prefix tsquery token, got: {}",
            r.sql
        );
    }
}

#[cfg(test)]
mod fts_view_parity_tests {
    use crate::translate;

    const FIXTURES: &str = "INSERT INTO metadata_items (id,title,title_sort,original_title) VALUES
        (1,'Keep Big','Zebra','Bunny'),(2,NULL,'Sort Only',NULL),
        (3,'München',NULL,NULL),(4,NULL,NULL,NULL);
        INSERT INTO tags (id,tag) VALUES (1,'Adventure'),(2,NULL),(3,'München');";

    fn sqlite_oracle() -> rusqlite::Connection {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE metadata_items(id integer,title text,title_sort text,original_title text);
            CREATE TABLE tags(id integer,tag text);").unwrap();
        db.execute_batch(FIXTURES).unwrap();
        for table in ["fts4_metadata_titles", "fts4_metadata_titles_icu"] {
            db.execute_batch(&format!("CREATE VIRTUAL TABLE {table} USING fts4(title,title_sort,original_title,tokenize=unicode61 'remove_diacritics=0');
                INSERT INTO {table}(rowid,title,title_sort,original_title) SELECT id,title,title_sort,original_title FROM metadata_items;")).unwrap();
        }
        for table in ["fts4_tag_titles", "fts4_tag_titles_icu"] {
            db.execute_batch(&format!("CREATE VIRTUAL TABLE {table} USING fts4(tag,tokenize=unicode61 'remove_diacritics=0');
                INSERT INTO {table}(rowid,tag) SELECT id,tag FROM tags;")).unwrap();
        }
        db
    }

    fn cases() -> Vec<(String, Vec<i32>)> {
        let mut cases = Vec::new();
        for table in ["fts4_metadata_titles", "fts4_metadata_titles_icu"] {
            for (column, term, expected) in [
                ("title", "KEEP*", vec![1]),
                ("title_sort", "Zeb*", vec![1]),
                ("title_sort", "sort*", vec![2]),
                ("original_title", "bun*", vec![1]),
                (table, "BUN*", vec![1]),
                (table, "mün*", vec![3]),
                (table, "absent", vec![]),
            ] {
                cases.push((format!("SELECT rowid FROM {table} WHERE \"{column}\" MATCH '{term}' ORDER BY rowid"), expected));
            }
        }
        for table in ["fts4_tag_titles", "fts4_tag_titles_icu"] {
            for (column, term, expected) in [("tag", "ADVEN*", vec![1]), (table, "mün*", vec![3])]
            {
                cases.push((format!("SELECT rowid FROM {table} WHERE \"{column}\" MATCH '{term}' ORDER BY rowid"), expected));
            }
        }
        cases
    }

    #[test]
    fn prepared_match_parameter_indexes_are_proven_and_exclusive() {
        use super::fts_match_parameter;
        for (sql, idx) in [
            ("SELECT rowid FROM fts4_metadata_titles WHERE title MATCH ?", 1),
            ("SELECT rowid FROM fts4_metadata_titles WHERE rowid=? AND title_sort MATCH ?", 2),
            ("SELECT rowid FROM fts4_metadata_titles WHERE original_title MATCH ?3", 3),
            ("SELECT rowid FROM fts4_metadata_titles WHERE title MATCH :term", 1),
            ("SELECT rowid FROM fts4_metadata_titles WHERE rowid=:id AND title MATCH :term", 2),
            ("SELECT rowid FROM fts4_metadata_titles WHERE title MATCH @term", 1),
            ("SELECT rowid FROM fts4_metadata_titles WHERE title MATCH $term", 1),
            ("SELECT rowid FROM fts4_metadata_titles WHERE title MATCH ?1 OR title_sort MATCH ?1", 1),
            ("SELECT rowid FROM fts4_metadata_titles WHERE title MATCH ? /* ? ignored */ ORDER BY rowid", 1),
        ] { assert_eq!(fts_match_parameter(sql, idx), Ok(true), "{sql}"); }
        for (sql, idx) in [
            (
                "SELECT rowid FROM fts4_metadata_titles WHERE title MATCH 'Keep' AND rowid=?",
                1,
            ),
            (
                "SELECT rowid FROM fts4_metadata_titles WHERE rowid=? AND title MATCH ?",
                1,
            ),
            ("SELECT '?' FROM ordinary WHERE title MATCH ?", 1),
            (
                "SELECT rowid FROM fts4_metadata_titles WHERE title=? /* MATCH ? */",
                1,
            ),
        ] {
            assert_eq!(fts_match_parameter(sql, idx), Ok(false), "{sql}");
        }
        for sql in [
            "SELECT rowid FROM fts4_metadata_titles WHERE title MATCH ?1 AND title=?1",
            "SELECT :term FROM fts4_metadata_titles WHERE title MATCH :term",
            "SELECT rowid FROM fts4_metadata_titles WHERE title MATCH ? || '*'",
            "SELECT rowid FROM fts4_metadata_titles WHERE title MATCH (?)",
            "SELECT rowid FROM fts4_metadata_titles WHERE title MATCH ?; SELECT ?",
        ] {
            assert!(fts_match_parameter(sql, 1).is_err(), "{sql}");
        }
        let translated =
            translate("SELECT rowid FROM fts4_metadata_titles WHERE title_sort MATCH ?")
                .unwrap()
                .sql;
        assert!(
            translated.contains("to_tsquery('simple', $1)"),
            "{translated}"
        );
    }

    #[test]
    fn table_wide_match_uses_known_aggregate_vector() {
        let sqlite = sqlite_oracle();
        for (sql, expected) in cases() {
            let rows = sqlite
                .prepare(&sql)
                .unwrap()
                .query_map([], |row| row.get::<_, i32>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(rows, expected, "SQLite oracle: {sql}");
            let translated = translate(&sql).unwrap().sql;
            if sql.contains("WHERE \"fts4_") {
                assert!(translated.contains(".title_fts @@"), "{translated}");
                assert!(!translated.contains("to_tsvector("), "{translated}");
            } else {
                assert!(translated.contains("to_tsvector("), "{translated}");
            }
        }
        // A similarly named column remains a text column; no generic alias guessing.
        let sql = translate("SELECT * FROM t WHERE other_fts MATCH 'Keep'")
            .unwrap()
            .sql;
        assert!(sql.contains("to_tsvector('simple', other_fts)"), "{sql}");
    }

    #[test]
    #[ignore = "requires isolated PostgreSQL via TEST_FTS_PG_URL"]
    fn postgres_fts_views_match_sqlite_and_upgrade_in_place() {
        let sqlite = sqlite_oracle();
        let mut pg = postgres::Client::connect(
            &std::env::var("TEST_FTS_PG_URL").expect("TEST_FTS_PG_URL"),
            postgres::NoTls,
        )
        .unwrap();
        let mut tx = pg.transaction().unwrap();
        let long_unicode = "München 東京 🐇 d'été ".repeat(2048);
        let literal_sql = format!(
            "SELECT '{}' AS \"日本語\" ORDER BY \"日本語\"",
            long_unicode.replace('\'', "''")
        );
        let oracle_text: String = sqlite
            .query_row(&literal_sql, [], |row| row.get(0))
            .unwrap();
        let translated_literal = translate(&literal_sql).unwrap().sql;
        let pg_text: String = tx.query_one(&translated_literal, &[]).unwrap().get(0);
        assert_eq!(pg_text, oracle_text);
        assert_eq!(pg_text, long_unicode);
        tx.batch_execute("CREATE SCHEMA plex; SET LOCAL search_path TO plex;
            CREATE TABLE metadata_items(id integer,title text,title_sort text,original_title text,search_vector tsvector,title_fts tsvector);
            CREATE TABLE tags(id integer,tag text,search_vector tsvector);
            CREATE VIEW fts4_metadata_titles AS SELECT id AS rowid,title,title_fts FROM metadata_items;
            CREATE VIEW fts4_metadata_titles_icu AS SELECT id AS rowid,title,title_fts FROM metadata_items;
            CREATE VIEW fts4_tag_titles AS SELECT id AS rowid,tag AS title,search_vector AS title_fts FROM tags;
            CREATE VIEW fts4_tag_titles_icu AS SELECT id AS rowid,tag AS title,search_vector AS title_fts FROM tags;").unwrap();
        let upgrade = include_str!("../../../schema/fts_view_parity_upgrade.sql")
            .replace("BEGIN;", "")
            .replace("COMMIT;", "");
        tx.batch_execute(&upgrade).unwrap();
        tx.batch_execute(&upgrade).unwrap();
        tx.batch_execute(FIXTURES).unwrap();
        for (sql, expected) in cases() {
            let oracle = sqlite
                .prepare(&sql)
                .unwrap()
                .query_map([], |row| row.get::<_, i32>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(oracle, expected);
            let translated = translate(&sql).unwrap().sql;
            let actual: Vec<i32> = tx
                .query(&translated, &[])
                .unwrap_or_else(|e| panic!("{translated}: {e}"))
                .iter()
                .map(|row| row.get(0))
                .collect();
            assert_eq!(actual, oracle, "{translated}");
        }
        let columns: Vec<String> = tx.query("SELECT column_name::text FROM information_schema.columns WHERE table_schema='plex' AND table_name='fts4_metadata_titles' ORDER BY ordinal_position", &[])
            .unwrap().iter().map(|row| row.get(0)).collect();
        assert_eq!(
            columns,
            [
                "rowid",
                "title",
                "title_fts",
                "title_sort",
                "original_title"
            ]
        );
        // Exercise the actual schema triggers after validating imported NULL vectors.
        let schema = include_str!("../../../schema/plex_schema.sql");
        for function in ["metadata_items_search_trigger", "tags_search_trigger"] {
            let start = schema
                .find(&format!("CREATE FUNCTION plex.{function}()"))
                .unwrap();
            let end = start + schema[start..].find("$$;").unwrap() + 3;
            tx.batch_execute(&schema[start..end]).unwrap();
        }
        tx.batch_execute("CREATE TRIGGER metadata_items_search_update BEFORE INSERT OR UPDATE ON plex.metadata_items FOR EACH ROW EXECUTE FUNCTION plex.metadata_items_search_trigger();
            CREATE TRIGGER tags_search_update BEFORE INSERT OR UPDATE ON plex.tags FOR EACH ROW EXECUTE FUNCTION plex.tags_search_trigger();
            UPDATE metadata_items SET title=title; UPDATE tags SET tag=tag;").unwrap();
        for (sql, expected) in cases() {
            let actual: Vec<i32> = tx
                .query(&translate(&sql).unwrap().sql, &[])
                .unwrap()
                .iter()
                .map(|row| row.get(0))
                .collect();
            assert_eq!(actual, expected, "populated vector: {sql}");
        }
        // Phrases must stay within one SQLite FTS source column. Bare terms
        // may span columns, and separate phrases may match different columns.
        let phrase_rows = "(1,'Big','Bunny',NULL),(2,'Big Bunny','Trail',NULL),
            (3,NULL,'Big','Bunny'),(4,NULL,'Big Bunny','Trail'),
            (5,NULL,'Big','Bunny Trail'),(6,'Only',NULL,'Big Bunny'),
            (7,'Big Bunny','Bunny Trail','Original Title'),(8,'Big Bunny','The Original','Trail')";
        let phrase_sqlite = rusqlite::Connection::open_in_memory().unwrap();
        phrase_sqlite.execute_batch(&format!("CREATE VIRTUAL TABLE fts4_metadata_titles USING fts4(title,title_sort,original_title);
            INSERT INTO fts4_metadata_titles(rowid,title,title_sort,original_title) VALUES {phrase_rows};")).unwrap();
        tx.batch_execute(&format!(
            "DELETE FROM metadata_items;
            INSERT INTO metadata_items(id,title,title_sort,original_title) VALUES {phrase_rows};"
        ))
        .unwrap();
        let phrase_cases = [
            ("fts4_metadata_titles", "\"Big Bunny\"", vec![2, 4, 6, 7, 8]),
            (
                "fts4_metadata_titles",
                "Big Bunny",
                vec![1, 2, 3, 4, 5, 6, 7, 8],
            ),
            (
                "fts4_metadata_titles",
                "\"Big Bunny\" Trail",
                vec![2, 4, 7, 8],
            ),
            (
                "fts4_metadata_titles",
                "\"Big Bunny\" \"Bunny Trail\"",
                vec![7],
            ),
            (
                "fts4_metadata_titles",
                "\"Big Bunny\" OR \"Bunny Trail\"",
                vec![2, 4, 5, 6, 7, 8],
            ),
            (
                "fts4_metadata_titles",
                "\"Big Bunny\" \"Original Title\"",
                vec![7],
            ),
            ("title_sort", "\"Big Bunny\"", vec![4]),
            ("original_title", "\"Big Bunny\"", vec![6]),
            ("fts4_metadata_titles", "\"Bunny Trail\"", vec![5, 7]),
            ("fts4_metadata_titles", "\"Bunny The\"", vec![]),
        ];
        for populated in [false, true] {
            tx.batch_execute(
                "ALTER TABLE metadata_items DISABLE TRIGGER metadata_items_search_update;",
            )
            .unwrap();
            if populated {
                tx.batch_execute("ALTER TABLE metadata_items ENABLE TRIGGER metadata_items_search_update; UPDATE metadata_items SET title=title;").unwrap();
            } else {
                tx.batch_execute("UPDATE metadata_items SET search_vector=NULL;")
                    .unwrap();
            }
            for (column, term, expected) in &phrase_cases {
                let sql = format!("SELECT rowid FROM fts4_metadata_titles WHERE {column} MATCH '{term}' ORDER BY rowid");
                let oracle: Vec<i32> = phrase_sqlite
                    .prepare(&sql)
                    .unwrap()
                    .query_map([], |row| row.get(0))
                    .unwrap()
                    .collect::<Result<_, _>>()
                    .unwrap();
                assert_eq!(&oracle, expected, "SQLite phrase oracle: {sql}");
                let translated = translate(&sql).unwrap().sql;
                let actual: Vec<i32> = tx
                    .query(&translated, &[])
                    .unwrap()
                    .iter()
                    .map(|row| row.get(0))
                    .collect();
                assert_eq!(
                    actual, oracle,
                    "literal, populated={populated}: {translated}"
                );
                let prepared_sql = sql.replace(&format!("'{term}'"), "?");
                let prepared = tx.prepare(&translate(&prepared_sql).unwrap().sql).unwrap();
                let normalized = super::convert_fts_term(term);
                let actual: Vec<i32> = tx
                    .query(&prepared, &[&normalized])
                    .unwrap()
                    .iter()
                    .map(|row| row.get(0))
                    .collect();
                assert_eq!(
                    actual, oracle,
                    "prepared, populated={populated}: {prepared_sql}, {term}"
                );
            }
        }
        tx.rollback().unwrap();
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod distinct_fix_test {
    use crate::translate;
    #[test]
    fn rewrite_distinct_orderby__col_added_to_select() {
        let sql = "SELECT DISTINCT (metadata_items.id) FROM metadata_items LEFT JOIN media_items ON media_items.metadata_item_id = metadata_items.id WHERE metadata_items.id = ? ORDER BY metadata_items.title_sort ASC";
        let r = translate(sql).expect("translate ok");
        eprintln!("OUT: {}", r.sql);
        // title_sort must appear in the SELECT list (before FROM)
        let out = r.sql.to_lowercase();
        let from_pos = out.find(" from ").unwrap_or(out.len());
        assert!(
            out[..from_pos].contains("title_sort"),
            "title_sort not in select: {}",
            r.sql
        );
    }
}
