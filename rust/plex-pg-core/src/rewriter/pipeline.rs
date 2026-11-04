use sqlparser::ast::Statement;

use super::rules::{default_rules, RewriteContext, RewriteRule};

pub struct RewritePipeline {
    rules: Vec<Box<dyn RewriteRule>>,
}

impl RewritePipeline {
    pub fn new(rules: Vec<Box<dyn RewriteRule>>) -> Self {
        Self { rules }
    }

    pub fn apply(&self, stmt: &mut Statement, ctx: &mut RewriteContext<'_>) {
        for rule in &self.rules {
            rule.apply(stmt, ctx);
        }
    }

    pub fn rule_names(&self) -> Vec<&'static str> {
        self.rules.iter().map(|r| r.name()).collect()
    }
}

impl Default for RewritePipeline {
    fn default() -> Self {
        Self::new(default_rules())
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::RewritePipeline;

    #[test]
    fn rewrite_idempotence__default_pipeline_rule_order_is_stable() {
        let pipeline = RewritePipeline::default();
        assert_eq!(
            pipeline.rule_names(),
            vec![
                "dedup",
                "placeholders",
                "functions",
                "types",
                "keywords",
                "derived_aliases",
                "upsert",
                "quotes",
                "groupby",
                "query",
            ]
        );
    }
}
