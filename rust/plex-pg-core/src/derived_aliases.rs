//! Give every derived table (subquery in FROM) an alias.
//!
//! SQLite accepts `SELECT x FROM (SELECT ...)`; PostgreSQL before 16 rejects it
//! with "subquery in FROM must have an alias". Plex's library cleanup fixups use
//! that form (e.g. `delete from locations where id in (select l_id from
//! (select ...) where l_count = 0)`), so deleting a library failed (issue #60).
//!
//! Aliases are numbered in visiting order per statement, so the same SQL always
//! translates to the same text and prepared-statement caches keep working.

use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr, Ident, JoinConstraint,
    JoinOperator, OrderByKind, Query, Select, SelectItem, SetExpr, Statement, TableAlias,
    TableFactor, TableWithJoins, UpdateTableFromKind,
};

pub fn transform(stmt: &mut Statement) {
    let mut walker = Walker { next: 1 };
    match stmt {
        Statement::Query(q) => walker.query(q),
        Statement::Insert(ins) => {
            if let Some(source) = &mut ins.source {
                walker.query(source);
            }
        }
        Statement::Update(update) => {
            walker.table_with_joins(&mut update.table);
            for assignment in &mut update.assignments {
                walker.expr(&mut assignment.value);
            }
            if let Some(UpdateTableFromKind::BeforeSet(from) | UpdateTableFromKind::AfterSet(from)) =
                &mut update.from
            {
                for twj in from {
                    walker.table_with_joins(twj);
                }
            }
            if let Some(selection) = &mut update.selection {
                walker.expr(selection);
            }
        }
        Statement::Delete(delete) => {
            if let Some(selection) = &mut delete.selection {
                walker.expr(selection);
            }
        }
        _ => {}
    }
}

struct Walker {
    next: usize,
}

impl Walker {
    fn query(&mut self, q: &mut Query) {
        if let Some(with) = &mut q.with {
            for cte in &mut with.cte_tables {
                self.query(&mut cte.query);
            }
        }
        self.set_expr(&mut q.body);
        if let Some(order_by) = &mut q.order_by {
            if let OrderByKind::Expressions(exprs) = &mut order_by.kind {
                for order in exprs {
                    self.expr(&mut order.expr);
                }
            }
        }
    }

    fn set_expr(&mut self, se: &mut SetExpr) {
        match se {
            SetExpr::Select(select) => self.select(select),
            SetExpr::Query(q) => self.query(q),
            SetExpr::SetOperation { left, right, .. } => {
                self.set_expr(left);
                self.set_expr(right);
            }
            _ => {}
        }
    }

    fn select(&mut self, select: &mut Select) {
        for item in &mut select.projection {
            match item {
                SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                    self.expr(e)
                }
                _ => {}
            }
        }
        for twj in &mut select.from {
            self.table_with_joins(twj);
        }
        if let Some(selection) = &mut select.selection {
            self.expr(selection);
        }
        if let GroupByExpr::Expressions(exprs, _) = &mut select.group_by {
            for e in exprs {
                self.expr(e);
            }
        }
        if let Some(having) = &mut select.having {
            self.expr(having);
        }
    }

    fn table_with_joins(&mut self, twj: &mut TableWithJoins) {
        self.table_factor(&mut twj.relation);
        for join in &mut twj.joins {
            self.table_factor(&mut join.relation);
            if let Some(JoinConstraint::On(on)) = join_constraint(&mut join.join_operator) {
                self.expr(on);
            }
        }
    }

    fn table_factor(&mut self, tf: &mut TableFactor) {
        match tf {
            TableFactor::Derived {
                subquery, alias, ..
            } => {
                self.query(subquery);
                if alias.is_none() {
                    *alias = Some(TableAlias {
                        explicit: true,
                        name: Ident::new(format!("plex_subquery_{}", self.next)),
                        columns: vec![],
                    });
                    self.next += 1;
                }
            }
            TableFactor::NestedJoin {
                table_with_joins, ..
            } => self.table_with_joins(table_with_joins),
            _ => {}
        }
    }

    fn expr(&mut self, e: &mut Expr) {
        match e {
            Expr::Subquery(q) => self.query(q),
            Expr::Function(function) => match &mut function.args {
                FunctionArguments::Subquery(q) => self.query(q),
                FunctionArguments::List(list) => {
                    for arg in &mut list.args {
                        let (FunctionArg::Named { arg, .. }
                        | FunctionArg::ExprNamed { arg, .. }
                        | FunctionArg::Unnamed(arg)) = arg;
                        if let FunctionArgExpr::Expr(e) = arg {
                            self.expr(e);
                        }
                    }
                }
                FunctionArguments::None => {}
            },
            Expr::Exists { subquery, .. } => self.query(subquery),
            Expr::InSubquery { expr, subquery, .. } => {
                self.expr(expr);
                self.query(subquery);
            }
            Expr::BinaryOp { left, right, .. } => {
                self.expr(left);
                self.expr(right);
            }
            Expr::UnaryOp { expr, .. }
            | Expr::Nested(expr)
            | Expr::IsNull(expr)
            | Expr::IsNotNull(expr)
            | Expr::Cast { expr, .. } => self.expr(expr),
            Expr::InList { expr, list, .. } => {
                self.expr(expr);
                for item in list {
                    self.expr(item);
                }
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                self.expr(expr);
                self.expr(low);
                self.expr(high);
            }
            Expr::Case {
                operand,
                conditions,
                else_result,
                ..
            } => {
                if let Some(operand) = operand {
                    self.expr(operand);
                }
                for when in conditions {
                    self.expr(&mut when.condition);
                    self.expr(&mut when.result);
                }
                if let Some(else_result) = else_result {
                    self.expr(else_result);
                }
            }
            _ => {}
        }
    }
}

fn join_constraint(op: &mut JoinOperator) -> Option<&mut JoinConstraint> {
    match op {
        JoinOperator::Join(c)
        | JoinOperator::Inner(c)
        | JoinOperator::Left(c)
        | JoinOperator::LeftOuter(c)
        | JoinOperator::Right(c)
        | JoinOperator::RightOuter(c)
        | JoinOperator::FullOuter(c)
        | JoinOperator::CrossJoin(c)
        | JoinOperator::Semi(c)
        | JoinOperator::LeftSemi(c)
        | JoinOperator::RightSemi(c)
        | JoinOperator::Anti(c)
        | JoinOperator::LeftAnti(c)
        | JoinOperator::RightAnti(c) => Some(c),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::translate;

    fn pg(sql: &str) -> String {
        translate(sql).unwrap_or_else(|e| panic!("translation failed for {sql}: {e}")).sql
    }

    #[test]
    fn plex_orphan_locations_fixup_gets_alias() {
        let out = pg("delete from locations where id in (select l_id from (select locations.id as l_id, count(locatables.id) as l_count from locations left join locatables on locatables.location_id = locations.id group by locations.id) where l_count = 0)");
        assert!(
            out.contains(") AS plex_subquery_1 WHERE l_count = 0"),
            "{out}"
        );
    }

    #[test]
    fn existing_aliases_are_kept_and_numbering_is_stable() {
        let sql = "select a from (select 1 as a) as keep join (select 2 as b) on 1 = 1 where a in (select c from (select 3 as c))";
        let first = pg(sql);
        assert!(first.contains("AS keep"), "{first}");
        assert!(first.contains("AS plex_subquery_1"), "{first}");
        assert!(first.contains("AS plex_subquery_2"), "{first}");
        assert_eq!(first, pg(sql), "translation must be deterministic");
    }

    #[test]
    fn derived_tables_in_select_and_update_are_aliased() {
        let out = pg("select (select count(*) from (select 1)) as n");
        assert!(out.contains("AS plex_subquery_1"), "{out}");
        let out = pg("update t set x = 1 where id in (select id from (select id from t2))");
        assert!(out.contains("AS plex_subquery_1"), "{out}");
    }

    #[test]
    fn set_join_function_and_order_positions_are_walked() {
        for sql in [
            "update t set c = (select count(*) from (select 1)) where id = 1",
            "select a.id from a join b on b.id in (select id from (select 1 as id))",
            "select coalesce((select max(x) from (select 1 as x)), 0)",
            "select id from t order by (select count(*) from (select 1))",
            "select id from t group by id, (select 1 from (select 1))",
        ] {
            let out = pg(sql);
            assert!(out.contains("AS plex_subquery_1"), "{sql} -> {out}");
        }
    }
}
