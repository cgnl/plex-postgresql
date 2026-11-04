use sqlparser::ast::Statement;

use crate::{dedup, derived_aliases, functions, keywords, placeholders, quotes, types, upsert};

use super::rules::{RewriteContext, RewriteRule};

struct DedupRule;
struct PlaceholdersRule;
struct FunctionsRule;
struct TypesRule;
struct KeywordsRule;
struct DerivedAliasesRule;
struct UpsertRule;
struct QuotesRule;

impl RewriteRule for DedupRule {
    fn name(&self) -> &'static str {
        "dedup"
    }

    fn apply(&self, stmt: &mut Statement, _ctx: &mut RewriteContext<'_>) {
        dedup::transform(stmt);
    }
}

impl RewriteRule for PlaceholdersRule {
    fn name(&self) -> &'static str {
        "placeholders"
    }

    fn apply(&self, stmt: &mut Statement, ctx: &mut RewriteContext<'_>) {
        placeholders::transform(stmt, ctx.param_names);
    }
}

impl RewriteRule for FunctionsRule {
    fn name(&self) -> &'static str {
        "functions"
    }

    fn apply(&self, stmt: &mut Statement, _ctx: &mut RewriteContext<'_>) {
        functions::transform(stmt);
    }
}

impl RewriteRule for TypesRule {
    fn name(&self) -> &'static str {
        "types"
    }

    fn apply(&self, stmt: &mut Statement, _ctx: &mut RewriteContext<'_>) {
        types::transform(stmt);
    }
}

impl RewriteRule for KeywordsRule {
    fn name(&self) -> &'static str {
        "keywords"
    }

    fn apply(&self, stmt: &mut Statement, _ctx: &mut RewriteContext<'_>) {
        keywords::transform(stmt);
    }
}

impl RewriteRule for DerivedAliasesRule {
    fn name(&self) -> &'static str {
        "derived_aliases"
    }

    fn apply(&self, stmt: &mut Statement, _ctx: &mut RewriteContext<'_>) {
        derived_aliases::transform(stmt);
    }
}

impl RewriteRule for UpsertRule {
    fn name(&self) -> &'static str {
        "upsert"
    }

    fn apply(&self, stmt: &mut Statement, _ctx: &mut RewriteContext<'_>) {
        upsert::transform(stmt);
    }
}

impl RewriteRule for QuotesRule {
    fn name(&self) -> &'static str {
        "quotes"
    }

    fn apply(&self, stmt: &mut Statement, _ctx: &mut RewriteContext<'_>) {
        quotes::transform(stmt);
    }
}

pub fn core_rules() -> Vec<Box<dyn RewriteRule>> {
    vec![
        Box::new(DedupRule),
        Box::new(PlaceholdersRule),
        Box::new(FunctionsRule),
        Box::new(TypesRule),
        Box::new(KeywordsRule),
        Box::new(DerivedAliasesRule),
        Box::new(UpsertRule),
        Box::new(QuotesRule),
    ]
}
