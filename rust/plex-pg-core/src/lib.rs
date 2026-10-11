#![allow(
    dangerous_implicit_autorefs,
    static_mut_refs,
    clippy::cmp_null,
    clippy::collapsible_match,
    clippy::collapsible_else_if,
    clippy::collapsible_if,
    clippy::collapsible_str_replace,
    clippy::doc_lazy_continuation,
    clippy::doc_overindented_list_items,
    clippy::explicit_counter_loop,
    clippy::if_same_then_else,
    clippy::manual_c_str_literals,
    clippy::manual_is_ascii_check,
    clippy::manual_pattern_char_comparison,
    clippy::missing_const_for_thread_local,
    clippy::needless_range_loop,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::ptr_eq,
    clippy::single_match,
    clippy::too_many_arguments,
    clippy::wildcard_in_or_patterns
)]

pub mod byte_utils;
pub mod c_abi;
pub mod db_interpose_bind;
pub mod db_interpose_column;
pub mod db_interpose_common;
pub mod db_interpose_conn_utils;
pub mod db_interpose_exec;
pub mod db_interpose_helpers;
pub mod db_interpose_metadata;
pub mod db_interpose_open;
pub mod db_interpose_prepare;
pub mod db_interpose_prepare_helpers;
pub mod db_interpose_prepare_utils;
pub mod db_interpose_step;
pub mod db_interpose_step_cached_read_utils;
pub mod db_interpose_step_read_utils;
pub mod db_interpose_step_write_utils;
pub mod db_interpose_stmt_lifecycle;
pub mod db_interpose_trace_helpers;
pub mod db_interpose_txn_utils;
pub mod db_interpose_value;
pub mod db_interpose_value_helpers;
pub mod dedup;
pub mod emit;
pub mod env_utils;
pub mod exception_what;
pub mod ffi;
pub mod ffi_types;
#[cfg(target_os = "macos")]
pub mod fishhook;
pub mod functions;
pub mod groupby;
pub mod keywords;
pub mod libpq_helpers;
pub mod pg_client;
pub mod pg_client_stmt_cache;
pub mod pg_config;
pub mod pg_logging;
pub mod pg_mem_telemetry;
pub mod pg_query_cache;
pub mod pg_statement;
/// plex-pg-core: SQLite → PostgreSQL SQL translation, caching, statement lifecycle & more
///
/// Pipeline (in order):
///   1. dedup         — UPDATE duplicate SET targets (keep last assignment)
///   2. placeholders  — ? / :name → $1, $2, ...
///   3. functions     — IFNULL→COALESCE, iif→CASE, typeof→pg_typeof, etc.
///   4. types         — AUTOINCREMENT, BLOB, INTEGER8, etc.
///   5. keywords      — GLOB→ILIKE, BEGIN IMMEDIATE, sqlite_master, etc.
///   6. upsert        — INSERT OR REPLACE / INSERT OR IGNORE → ON CONFLICT
///   7. quotes        — backtick identifiers → double-quote
///   8. groupby       — GROUP BY strict mode (add missing non-aggregate cols)
///   9. query         — misc query fixups (subquery alias, NULLS FIRST, etc.)
pub mod placeholders;
pub mod platform_backtrace;
#[cfg(target_os = "linux")]
pub mod pms_child_env;
#[cfg(target_os = "linux")]
pub mod pms_net_compat;
#[cfg(target_os = "linux")]
pub mod pms_process_compat;
pub mod query;
pub mod quotes;
pub mod rewriter;
pub mod runtime_common;
#[cfg(target_os = "linux")]
pub mod runtime_linux;
#[cfg(target_os = "macos")]
pub mod runtime_macos;
pub mod shim_alloc;
mod sync_utils;
#[cfg(test)]
pub mod test_utils;
mod translation_validation;
pub mod types;
pub mod upsert;

use sqlparser::dialect::{MySqlDialect, SQLiteDialect};
use sqlparser::parser::Parser;
use translation_validation::{
    parse_output_validation_mode, parse_sample_pct, should_validate_output,
    validate_postgres_output, VALIDATE_MODE_ENV, VALIDATE_SAMPLE_PCT_ENV,
};

/// Result of a full translation
#[derive(Debug, Clone)]
pub struct Translation {
    /// Translated PostgreSQL SQL
    pub sql: String,
    /// Original named parameter names in bind order (None for ? placeholders)
    pub param_names: Vec<Option<String>>,
}

/// Translate SQLite SQL to PostgreSQL SQL.
/// Returns Err with a description on parse or translation failure.
pub fn translate(sqlite_sql: &str) -> Result<Translation, String> {
    if sqlite_sql.is_empty() {
        return Ok(Translation {
            sql: String::new(),
            param_names: vec![],
        });
    }

    // Pre-parse normalisation: handle constructs sqlparser doesn't support
    // (GLOB → ILIKE, INDEXED BY → removed)
    let preprocessed = keywords::preprocess(sqlite_sql);
    if preprocessed.trim().is_empty() {
        return Ok(Translation {
            sql: String::new(),
            param_names: vec![],
        });
    }

    let sqlite_dialect = SQLiteDialect {};
    let mut stmts = match Parser::parse_sql(&sqlite_dialect, &preprocessed) {
        Ok(stmts) => stmts,
        Err(sqlite_err) => {
            // sqlparser's SQLiteDialect rejects some valid SQLite/MySQL-style
            // backtick-qualified forms (`alias.`reserved``). In that case,
            // retry with MySQL dialect to keep translation coverage broad.
            if preprocessed.as_bytes().contains(&b'`') {
                let mysql_dialect = MySqlDialect {};
                match Parser::parse_sql(&mysql_dialect, &preprocessed) {
                    Ok(stmts) => stmts,
                    Err(mysql_err) => {
                        return Err(format!(
                            "parse error: {sqlite_err}; mysql fallback parse error: {mysql_err}"
                        ));
                    }
                }
            } else {
                return Err(format!("parse error: {sqlite_err}"));
            }
        }
    };

    let mut param_names: Vec<Option<String>> = Vec::new();

    let pipeline = rewriter::pipeline::RewritePipeline::default();
    for stmt in &mut stmts {
        let mut ctx = rewriter::rules::RewriteContext {
            param_names: &mut param_names,
        };
        pipeline.apply(stmt, &mut ctx);
    }

    let sql = stmts
        .iter()
        .map(|stmt| {
            upsert::emit_supported_replace(stmt)
                .map(|replacement| replacement.unwrap_or_else(|| emit::emit(stmt)))
        })
        .collect::<Result<Vec<_>, String>>()?
        .join("; ");

    let validation_mode =
        parse_output_validation_mode(env_utils::env_string(VALIDATE_MODE_ENV).as_deref());
    let sample_pct = parse_sample_pct(env_utils::env_string(VALIDATE_SAMPLE_PCT_ENV).as_deref());
    if should_validate_output(validation_mode, &sql, sample_pct) {
        validate_postgres_output(&sql)?;
    }

    Ok(Translation { sql, param_names })
}
