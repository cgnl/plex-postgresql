/// Module: pg_config
///
/// SQL classification and PostgreSQL connection config loading, exposed via C FFI.
/// Replaces the C `pg_config.c` module.
use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::sync::OnceLock;

use crate::db_interpose_helpers::cstr_to_str_or_empty;
use crate::env_utils;

// ─── Internal pure helpers ────────────────────────────────────────────────────

pub(crate) fn strip_leading_ws_and_sql_comments(input: &str) -> &str {
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 < bytes.len() {
                i += 2;
            }
            continue;
        }
        break;
    }
    &input[i..]
}

/// Returns true if `filename` matches a Plex database that should be redirected
/// to PostgreSQL, and `passthrough` is false.
pub(crate) fn should_redirect_str(filename: &str, passthrough: bool) -> bool {
    if passthrough || filename.is_empty() {
        return false;
    }
    filename.contains("com.plexapp.plugins.library.db")
        || filename.contains("com.plexapp.plugins.library.blobs.db")
}

/// Returns true if the SQL is a SQLite engine configuration call that must
/// execute on the real SQLite but should NOT be routed to PostgreSQL.
/// These statements configure SQLite's internal runtime (tokenizers, collations,
/// extensions) and are invisible to PG. The shim should not register a PgStmt
/// for them — prepare and step go directly through the real SQLite.
pub(crate) fn is_sqlite_passthrough_str(sql: &str) -> bool {
    if crate::pg_client::transaction::is_transaction_sql(sql) {
        return false;
    }
    let trimmed = strip_leading_ws_and_sql_comments(sql);
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_lowercase();
    let sqlite_fts_ddl = [
        "create trigger ",
        "create trigger if not exists ",
        "drop trigger ",
        "drop trigger if exists ",
        "drop table ",
        "drop table if exists ",
    ]
    .iter()
    .any(|prefix| {
        lower.strip_prefix(prefix).is_some_and(|name| {
            let name = name.trim_start_matches(['"', '`', '[']);
            name.starts_with("fts3_") || name.starts_with("fts4_")
        })
    });
    lower.starts_with("icu_load_collation")
        || lower.starts_with("create virtual table ")
        || sqlite_fts_ddl
        || lower.starts_with("fts3_tokenizer")
        || lower.starts_with("select load_extension")
        || lower.starts_with("select fts3_tokenizer")
        || lower.starts_with("select icu_load_collation")
        || lower == "analyze"
        || lower.starts_with("analyze ")
        || lower.contains("sqlite_stat1")
        || lower.contains("sqlite_stat4")
        || lower.contains("sqlite_master")
        || lower.contains("sqlite_schema")
        || lower.contains("locations_node")
        || lower.contains("locations_parent")
        || lower.contains("locations_rowid")
        || (lower.starts_with("pragma")
            && (lower.trim_end_matches(';').trim_end() == "pragma page_size"
                || lower.contains(".page_size")))
}

/// Returns true if the SQL statement should be skipped (treated as a no-op).
/// Step returns SQLITE_DONE without executing anything.
/// NOTE: SQLite engine config (fts3_tokenizer, icu_load_collation, load_extension)
/// is NOT in this list — those are sqlite-passthrough (execute on real SQLite).
pub(crate) fn should_skip_sql_str(sql: &str) -> bool {
    if crate::pg_client::transaction::is_transaction_sql(sql) {
        return false;
    }
    let trimmed = strip_leading_ws_and_sql_comments(sql);
    if trimmed.is_empty() {
        return false;
    }

    let lower = trimmed.to_lowercase();

    // A) PREFIX patterns (case-insensitive, after stripping leading whitespace)
    const PREFIX_PATTERNS: &[&str] = &[
        "vacuum",
        "pragma",
        "reindex",
        "analyze sqlite_",
        "attach database",
        "detach database",
    ];

    for prefix in PREFIX_PATTERNS {
        if lower.starts_with(prefix) {
            return true;
        }
    }

    // B) ANYWHERE patterns (case-insensitive substring match)
    const ANYWHERE_PATTERNS: &[&str] = &[
        "spellfix",
        "set $2=$2",
        "set $1=$1",
        "set :2=:2",
        "set :1=:1",
        ":col=:col",
    ];

    // Lowercase the full (untrimmed) sql for anywhere matching
    let full_lower = sql.to_lowercase();

    // C) Debug override: skip any SQL containing configured substring(s).
    // Comma-separated, case-insensitive. Example:
    //   PLEX_PG_DEBUG_SKIP_CONTAINS=plugins,tags
    if let Some(skip_contains) = env_utils::env_string("PLEX_PG_DEBUG_SKIP_CONTAINS") {
        for token in skip_contains.split(',') {
            let t = token.trim().to_lowercase();
            if !t.is_empty() && full_lower.contains(&t) {
                return true;
            }
        }
    }

    for pattern in ANYWHERE_PATTERNS {
        if full_lower.contains(pattern) {
            return true;
        }
    }

    false
}

/// Returns true if the SQL is a write operation (INSERT, UPDATE, DELETE, REPLACE)
/// or a DDL operation (ALTER, CREATE, DROP) that should be routed to PostgreSQL.
/// DDL is included here so that migrations (ALTER TABLE ADD COLUMN, CREATE INDEX, etc.)
/// go through the PG path with a dummy shadow, instead of hitting real SQLite where
/// the schema may be out of sync.
pub(crate) fn is_write_operation_str(sql: &str) -> bool {
    if crate::pg_client::transaction::is_transaction_sql(sql) {
        return true;
    }
    let lower = strip_leading_ws_and_sql_comments(sql).to_lowercase();
    lower.starts_with("insert")
        || lower.starts_with("update")
        || lower.starts_with("delete")
        || lower.starts_with("replace")
        || lower.starts_with("alter")
        || lower.starts_with("create")
        || lower.starts_with("drop")
}

/// Returns true if the SQL is a DDL operation (ALTER, CREATE, DROP).
/// Used to force PG-only routing for schema changes — shadow SQLite
/// must never see DDL since it may lack tables/columns that PG has.
pub(crate) fn is_ddl_str(sql: &str) -> bool {
    let lower = strip_leading_ws_and_sql_comments(sql).to_lowercase();
    lower.starts_with("alter") || lower.starts_with("create") || lower.starts_with("drop")
}

/// Returns true if the SQL is a read operation (SELECT).
pub(crate) fn is_read_operation_str(sql: &str) -> bool {
    strip_leading_ws_and_sql_comments(sql)
        .to_ascii_lowercase()
        .starts_with("select")
}

/// Parse a comma-separated string of millisecond delays.
/// Returns up to `max_count` entries, each capped at `cap_ms`.
fn parse_delay_list(s: &str, max_count: usize, cap_ms: i32) -> Vec<i32> {
    let mut result = Vec::new();
    for part in s.split(',') {
        if result.len() >= max_count {
            break;
        }
        let trimmed = part.trim();
        if let Ok(n) = trimmed.parse::<i32>() {
            result.push(n.min(cap_ms));
        }
    }
    result
}

const DEFAULT_DELAYS: &[i32] = &[500, 1000, 2000, 3000, 4000];
const MAX_DELAYS: usize = 10;
const MAX_DELAY_MS: i32 = 60_000;

/// Return the configured retry delays.  Uses PLEX_PG_RETRY_DELAYS env var if
/// present and parseable; otherwise falls back to `DEFAULT_DELAYS`.
pub(crate) fn get_retry_delays_vec() -> Vec<i32> {
    if let Some(val) = env_utils::env_string("PLEX_PG_RETRY_DELAYS") {
        let parsed = parse_delay_list(&val, MAX_DELAYS, MAX_DELAY_MS);
        if !parsed.is_empty() {
            return parsed;
        }
    }
    DEFAULT_DELAYS.to_vec()
}

// ─── Public C FFI functions ───────────────────────────────────────────────────

/// Returns 1 if `filename` contains a Plex database pattern that should be
/// redirected to PostgreSQL, 0 otherwise.
///
/// Patterns: `com.plexapp.plugins.library.db`, `com.plexapp.plugins.library.blobs.db`
///
/// Returns 0 if `filename` is NULL, empty, or doesn't match.
/// Also returns 0 if `passthrough_only` is non-zero.
pub fn pg_config_should_redirect(filename: *const c_char, passthrough_only: i32) -> i32 {
    let s = unsafe { cstr_to_str_or_empty(filename) };
    i32::from(should_redirect_str(s, passthrough_only != 0))
}

/// Returns 1 if the SQL should be skipped (no-op'd) — SQLite-specific commands.
///
/// Skipped via PREFIX match (case-insensitive, after stripping leading whitespace):
///   `icu_load_collation`, `fts3_tokenizer`, `SELECT load_extension`,
///   `VACUUM`, `PRAGMA`, `REINDEX`, `ANALYZE sqlite_`,
///   `ATTACH DATABASE`, `DETACH DATABASE`
///
/// Skipped via ANYWHERE match (case-insensitive substring):
///   `sqlite_schema`, `sqlite_master`, `fts3_tokenizer`, `spellfix`,
///   `icu_load_collation`, `SET $2=$2`, `SET $1=$1`, `SET :2=:2`, `SET :1=:1`, `:col=:col`
pub fn pg_config_should_skip_sql(sql: *const c_char) -> i32 {
    let s = unsafe { cstr_to_str_or_empty(sql) };
    i32::from(should_skip_sql_str(s))
}

/// Returns 1 if the SQL is a write operation (INSERT, UPDATE, DELETE, REPLACE).
/// Case-insensitive, skips leading whitespace.  Returns 0 for NULL input.
pub fn pg_config_is_write_operation(sql: *const c_char) -> i32 {
    let s = unsafe { cstr_to_str_or_empty(sql) };
    i32::from(is_write_operation_str(s))
}

/// Returns 1 if the SQL is a read operation (SELECT).
/// Case-insensitive, skips leading whitespace.  Returns 0 for NULL input.
pub fn pg_config_is_read_operation(sql: *const c_char) -> i32 {
    let s = unsafe { cstr_to_str_or_empty(sql) };
    i32::from(is_read_operation_str(s))
}

/// Parse `PLEX_PG_RETRY_DELAYS` env var into an array of millisecond delays.
///
/// Format: comma-separated integers, e.g. `"500,1000,2000,3000,4000"`.
/// Defaults to `[500, 1000, 2000, 3000, 4000]` when the env var is missing or
/// cannot be parsed.  At most 10 entries are returned; each is capped at 60 000 ms.
///
/// # Safety
/// `delays_out` must point to an array of at least 10 `i32` values.
/// `count_out` must be a valid non-null pointer to an `i32`.
pub fn pg_config_get_retry_delays(delays_out: *mut i32, count_out: *mut i32) {
    if delays_out.is_null() || count_out.is_null() {
        return;
    }
    let delays = get_retry_delays_vec();
    let n = delays.len().min(MAX_DELAYS);
    unsafe {
        for (i, &d) in delays[..n].iter().enumerate() {
            *delays_out.add(i) = d;
        }
        *count_out = n as i32;
    }
}

// ─── Config struct and loader ─────────────────────────────────────────────────

/// PostgreSQL connection configuration populated from environment variables.
#[repr(C)]
pub struct PgConnConfig {
    pub host: [u8; 256],
    pub port: i32,
    pub database: [u8; 128],
    pub user: [u8; 128],
    pub password: [u8; 256],
    pub schema: [u8; 64],
}

/// Rust-native connection config parsed from environment variables.
#[derive(Clone, Debug, Default)]
pub struct PgEnvConfig {
    pub host: String,
    pub port: i32,
    pub database: String,
    pub user: String,
    pub password: String,
    pub schema: String,
}

impl PgEnvConfig {
    /// Load configuration from `PLEX_PG_*` environment variables.
    pub fn from_env() -> Self {
        let port = env_utils::env_string("PLEX_PG_PORT")
            .and_then(|v| v.trim().parse::<i32>().ok())
            .unwrap_or(0);
        Self {
            host: env_utils::env_string("PLEX_PG_HOST").unwrap_or_default(),
            port,
            database: env_utils::env_string("PLEX_PG_DATABASE").unwrap_or_default(),
            user: env_utils::env_string("PLEX_PG_USER").unwrap_or_default(),
            password: env_utils::env_string("PLEX_PG_PASSWORD").unwrap_or_default(),
            schema: env_utils::env_string("PLEX_PG_SCHEMA").unwrap_or_default(),
        }
    }

    fn write_to_conn_config(&self, cfg: &mut PgConnConfig) {
        if !self.host.is_empty() {
            write_str_to_buf(&mut cfg.host, &self.host);
        }
        cfg.port = self.port;
        if !self.database.is_empty() {
            write_str_to_buf(&mut cfg.database, &self.database);
        }
        if !self.user.is_empty() {
            write_str_to_buf(&mut cfg.user, &self.user);
        }
        if !self.password.is_empty() {
            write_str_to_buf(&mut cfg.password, &self.password);
        }
        if !self.schema.is_empty() {
            write_str_to_buf(&mut cfg.schema, &self.schema);
        }
    }
}

static DEFAULT_CONFIG: PgConnConfig = PgConnConfig {
    host: [0; 256],
    port: 0,
    database: [0; 128],
    user: [0; 128],
    password: [0; 256],
    schema: [0; 64],
};

static GLOBAL_CONFIG: OnceLock<PgConnConfig> = OnceLock::new();

fn init_config_once() {
    GLOBAL_CONFIG.get_or_init(|| {
        let mut cfg = PgConnConfig {
            host: [0; 256],
            port: 0,
            database: [0; 128],
            user: [0; 128],
            password: [0; 256],
            schema: [0; 64],
        };
        let _ = pg_config_load(&mut cfg as *mut PgConnConfig);

        let host = unsafe { cstr_to_str_or_empty(cfg.host.as_ptr() as *const c_char) };
        let user = unsafe { cstr_to_str_or_empty(cfg.user.as_ptr() as *const c_char) };
        let db = unsafe { cstr_to_str_or_empty(cfg.database.as_ptr() as *const c_char) };
        let schema = unsafe { cstr_to_str_or_empty(cfg.schema.as_ptr() as *const c_char) };
        let msg = format!(
            "PostgreSQL config: {}@{}:{}/{} (schema: {})",
            user, host, cfg.port, db, schema
        );
        if let Ok(cs) = CString::new(msg) {
            crate::pg_logging::rust_logging_write(1, cs.as_ptr());
        }

        cfg
    });
}

// ─── C ABI wrappers (pg_config.c replacement) ────────────────────────────────

#[no_mangle]
pub extern "C" fn pg_config_init() {
    init_config_once();
}

#[no_mangle]
pub extern "C" fn pg_config_get() -> *mut PgConnConfig {
    init_config_once();
    GLOBAL_CONFIG.get().unwrap_or(&DEFAULT_CONFIG) as *const PgConnConfig as *mut PgConnConfig
}

#[no_mangle]
pub extern "C" fn should_redirect(filename: *const c_char) -> c_int {
    let passthrough = crate::db_interpose_common::SHIM_PASSTHROUGH_ONLY
        .load(std::sync::atomic::Ordering::Acquire);
    pg_config_should_redirect(filename, passthrough)
}

#[no_mangle]
pub extern "C" fn should_skip_sql(sql: *const c_char) -> c_int {
    pg_config_should_skip_sql(sql)
}

#[no_mangle]
pub extern "C" fn is_write_operation(sql: *const c_char) -> c_int {
    pg_config_is_write_operation(sql)
}

#[no_mangle]
pub extern "C" fn is_read_operation(sql: *const c_char) -> c_int {
    pg_config_is_read_operation(sql)
}

#[no_mangle]
pub extern "C" fn pg_get_retry_delays(delays_out: *mut i32, count_out: *mut i32) {
    pg_config_get_retry_delays(delays_out, count_out);
}

/// Write a Rust `&str` into a fixed-size byte buffer as a null-terminated C string.
/// Silently truncates if `src` is longer than `buf.len() - 1`.
fn write_str_to_buf(buf: &mut [u8], src: &str) {
    let bytes = src.as_bytes();
    let len = bytes.len().min(buf.len() - 1);
    buf[..len].copy_from_slice(&bytes[..len]);
    buf[len] = 0;
}

/// Load PostgreSQL connection configuration from environment variables.
///
/// Reads: `PLEX_PG_HOST`, `PLEX_PG_PORT`, `PLEX_PG_DATABASE`, `PLEX_PG_USER`,
///        `PLEX_PG_PASSWORD`, `PLEX_PG_SCHEMA`
///
/// Writes results into the caller-supplied `PgConnConfig` struct.
/// Returns 1 on success.
///
/// # Safety
/// `config` must be a valid non-null pointer to a `PgConnConfig`.
pub fn pg_config_load(config: *mut PgConnConfig) -> i32 {
    if config.is_null() {
        return 0;
    }

    let cfg = unsafe { &mut *config };

    // Zero-initialise all fields.
    cfg.host.fill(0);
    cfg.port = 0;
    cfg.database.fill(0);
    cfg.user.fill(0);
    cfg.password.fill(0);
    cfg.schema.fill(0);

    let env_cfg = PgEnvConfig::from_env();
    env_cfg.write_to_conn_config(cfg);

    1
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::env_lock;
    use std::ffi::CString;

    // Helper: call FFI via CString
    fn c(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    // ── should_redirect ──────────────────────────────────────────────────────

    #[test]
    fn redirect_null_returns_false() {
        assert!(!should_redirect_str("", false));
    }

    #[test]
    fn redirect_empty_returns_false() {
        assert!(!should_redirect_str("", false));
    }

    #[test]
    fn redirect_library_db() {
        assert!(should_redirect_str(
            "/data/com.plexapp.plugins.library.db",
            false
        ));
    }

    #[test]
    fn redirect_blobs_db() {
        assert!(should_redirect_str(
            "/data/com.plexapp.plugins.library.blobs.db",
            false
        ));
    }

    #[test]
    fn redirect_other_db_returns_false() {
        assert!(!should_redirect_str("/tmp/other.db", false));
    }

    #[test]
    fn redirect_partial_match_returns_false() {
        // "library" alone is not enough
        assert!(!should_redirect_str("library.db", false));
    }

    #[test]
    fn redirect_passthrough_mode_returns_false() {
        assert!(!should_redirect_str(
            "/data/com.plexapp.plugins.library.db",
            true
        ));
    }

    #[test]
    fn redirect_ffi_null() {
        assert_eq!(pg_config_should_redirect(std::ptr::null(), 0), 0);
    }

    #[test]
    fn redirect_ffi_passthrough() {
        let cs = c("/data/com.plexapp.plugins.library.db");
        assert_eq!(pg_config_should_redirect(cs.as_ptr(), 1), 0);
    }

    #[test]
    fn redirect_ffi_library_db() {
        let cs = c("/data/com.plexapp.plugins.library.db");
        assert_eq!(pg_config_should_redirect(cs.as_ptr(), 0), 1);
    }

    // ── should_skip_sql ──────────────────────────────────────────────────────

    #[test]
    fn skip_null_returns_false() {
        assert!(!should_skip_sql_str(""));
    }

    #[test]
    fn skip_pragma() {
        assert!(should_skip_sql_str("PRAGMA journal_mode=WAL"));
    }

    #[test]
    fn skip_pragma_lowercase() {
        assert!(should_skip_sql_str("pragma journal_mode=WAL"));
    }

    #[test]
    fn skip_vacuum() {
        assert!(should_skip_sql_str("VACUUM"));
    }

    #[test]
    fn skip_reindex() {
        assert!(should_skip_sql_str("REINDEX"));
    }

    #[test]
    fn skip_attach_database() {
        assert!(should_skip_sql_str("ATTACH DATABASE ':memory:' AS aux"));
    }

    #[test]
    fn skip_detach_database() {
        assert!(should_skip_sql_str("DETACH DATABASE aux"));
    }

    #[test]
    fn passthrough_icu_load_collation() {
        assert!(!should_skip_sql_str(
            "icu_load_collation('en_US', 'icu_root')"
        ));
        assert!(is_sqlite_passthrough_str(
            "icu_load_collation('en_US', 'icu_root')"
        ));
    }

    #[test]
    fn passthrough_fts3_tokenizer() {
        assert!(!should_skip_sql_str("fts3_tokenizer('simple')"));
        assert!(is_sqlite_passthrough_str("fts3_tokenizer('simple')"));
        assert!(is_sqlite_passthrough_str("SELECT fts3_tokenizer(?, ?)"));
    }

    #[test]
    fn do_not_skip_begin() {
        assert!(!should_skip_sql_str("BEGIN"));
    }

    #[test]
    fn do_not_skip_begin_immediate() {
        assert!(!should_skip_sql_str("BEGIN IMMEDIATE"));
    }

    #[test]
    fn do_not_skip_commit() {
        assert!(!should_skip_sql_str("COMMIT"));
    }

    #[test]
    fn do_not_skip_rollback() {
        assert!(!should_skip_sql_str("ROLLBACK"));
    }

    #[test]
    fn do_not_skip_savepoint() {
        assert!(!should_skip_sql_str("SAVEPOINT sp1"));
    }

    #[test]
    fn do_not_skip_release_savepoint() {
        assert!(!should_skip_sql_str("RELEASE SAVEPOINT sp1"));
    }

    #[test]
    fn do_not_skip_transaction_keywords_with_semicolon_and_case() {
        assert!(!should_skip_sql_str("  begin immediate ;"));
        assert!(!should_skip_sql_str("\tCoMmIt;"));
        assert!(!should_skip_sql_str("Rollback ;"));
        assert!(!should_skip_sql_str("end;"));
        assert!(!should_skip_sql_str(" savepoint a ;"));
        assert!(!should_skip_sql_str(" release a ;"));
        assert!(!should_skip_sql_str("ReLeAsE savepoint a;"));
    }

    #[test]
    fn do_not_skip_transaction_keywords_with_leading_comments() {
        assert!(!should_skip_sql_str("/*tx*/BEGIN"));
        assert!(!should_skip_sql_str("-- tx\nCOMMIT"));
        assert!(!should_skip_sql_str("/* tx */ ROLLBACK"));
        assert!(!should_skip_sql_str("/* tx */ SAVEPOINT s1"));
        assert!(!should_skip_sql_str("/* tx */ RELEASE s1"));
    }

    #[test]
    fn skip_analyze_sqlite_master() {
        assert!(should_skip_sql_str("ANALYZE sqlite_master"));
    }

    #[test]
    fn skip_analyze_normal_table_not_skipped() {
        // ANALYZE on a regular table should NOT be skipped
        assert!(!should_skip_sql_str("ANALYZE my_table"));
    }

    #[test]
    fn passthrough_select_load_extension() {
        assert!(!should_skip_sql_str("SELECT load_extension('foo')"));
        assert!(is_sqlite_passthrough_str("SELECT load_extension('foo')"));
    }

    #[test]
    fn passthrough_sqlite_master() {
        assert!(!should_skip_sql_str(
            "SELECT * FROM sqlite_master WHERE type='table'"
        ));
        assert!(is_sqlite_passthrough_str(
            "SELECT * FROM sqlite_master WHERE type='table'"
        ));
    }

    #[test]
    fn passthrough_sqlite_schema() {
        assert!(!should_skip_sql_str(
            "SELECT * FROM sqlite_schema WHERE type='table'"
        ));
        assert!(is_sqlite_passthrough_str(
            "SELECT * FROM sqlite_schema WHERE type='table'"
        ));
    }

    #[test]
    fn skip_spellfix_anywhere() {
        assert!(should_skip_sql_str(
            "CREATE VIRTUAL TABLE t USING spellfix1"
        ));
    }

    #[test]
    fn skip_spellfix_select_anywhere() {
        assert!(should_skip_sql_str("SELECT * FROM spellfix_table"));
    }

    #[test]
    fn skip_set_dollar_2() {
        assert!(should_skip_sql_str("SET $2=$2"));
    }

    #[test]
    fn skip_set_dollar_1() {
        assert!(should_skip_sql_str("SET $1=$1"));
    }

    #[test]
    fn skip_set_colon_2() {
        assert!(should_skip_sql_str("SET :2=:2"));
    }

    #[test]
    fn skip_set_colon_1() {
        assert!(should_skip_sql_str("SET :1=:1"));
    }

    #[test]
    fn skip_col_eq_col() {
        assert!(should_skip_sql_str("UPDATE t SET :col=:col"));
    }

    #[test]
    fn skip_leading_whitespace() {
        assert!(should_skip_sql_str("  \t PRAGMA journal_mode=WAL"));
    }

    #[test]
    fn skip_case_insensitive_vacuum() {
        assert!(should_skip_sql_str("vacuum"));
    }

    #[test]
    fn no_skip_select() {
        assert!(!should_skip_sql_str("SELECT id FROM t"));
    }

    #[test]
    fn no_skip_insert() {
        assert!(!should_skip_sql_str("INSERT INTO t VALUES (1)"));
    }

    #[test]
    fn no_skip_update() {
        assert!(!should_skip_sql_str("UPDATE t SET x=1"));
    }

    #[test]
    fn no_skip_delete() {
        assert!(!should_skip_sql_str("DELETE FROM t WHERE id=1"));
    }

    #[test]
    fn skip_ffi_null() {
        assert_eq!(pg_config_should_skip_sql(std::ptr::null()), 0);
    }

    #[test]
    fn skip_ffi_pragma() {
        let cs = c("PRAGMA journal_mode=WAL");
        assert_eq!(pg_config_should_skip_sql(cs.as_ptr()), 1);
    }

    #[test]
    fn skip_ffi_select() {
        let cs = c("SELECT 1");
        assert_eq!(pg_config_should_skip_sql(cs.as_ptr()), 0);
    }

    // ── is_write_operation ───────────────────────────────────────────────────

    #[test]
    fn write_null_returns_false() {
        assert!(!is_write_operation_str(""));
    }

    #[test]
    fn write_insert() {
        assert!(is_write_operation_str("INSERT INTO t VALUES (1)"));
    }

    #[test]
    fn write_update() {
        assert!(is_write_operation_str("UPDATE t SET x=1"));
    }

    #[test]
    fn write_delete() {
        assert!(is_write_operation_str("DELETE FROM t WHERE id=1"));
    }

    #[test]
    fn write_replace() {
        assert!(is_write_operation_str("REPLACE INTO t VALUES (1)"));
    }

    #[test]
    fn write_select_is_not_write() {
        assert!(!is_write_operation_str("SELECT * FROM t"));
    }

    #[test]
    fn write_ddl_is_write() {
        assert!(is_write_operation_str("CREATE TABLE t (id INTEGER)"));
        assert!(is_write_operation_str("CREATE INDEX idx ON t(col)"));
        assert!(is_write_operation_str("ALTER TABLE t ADD COLUMN c TEXT"));
        assert!(is_write_operation_str("DROP TABLE t"));
        assert!(is_write_operation_str("drop index idx"));
        assert!(is_write_operation_str("  ALTER TABLE t ADD c TEXT"));
    }

    #[test]
    fn write_transaction_control() {
        assert!(is_write_operation_str("BEGIN"));
        assert!(is_write_operation_str("COMMIT"));
        assert!(is_write_operation_str("ROLLBACK"));
        assert!(is_write_operation_str("SAVEPOINT sp1"));
        assert!(is_write_operation_str("RELEASE SAVEPOINT sp1"));
    }

    #[test]
    fn write_leading_whitespace() {
        assert!(is_write_operation_str("   INSERT INTO t VALUES (1)"));
    }

    #[test]
    fn write_case_insensitive() {
        assert!(is_write_operation_str("insert into t values (1)"));
        assert!(is_write_operation_str("Insert Into t Values (1)"));
    }

    #[test]
    fn write_ffi_null() {
        assert_eq!(pg_config_is_write_operation(std::ptr::null()), 0);
    }

    #[test]
    fn write_ffi_insert() {
        let cs = c("INSERT INTO t VALUES (1)");
        assert_eq!(pg_config_is_write_operation(cs.as_ptr()), 1);
    }

    #[test]
    fn write_ffi_select() {
        let cs = c("SELECT 1");
        assert_eq!(pg_config_is_write_operation(cs.as_ptr()), 0);
    }

    // ── is_read_operation ────────────────────────────────────────────────────

    #[test]
    fn read_null_returns_false() {
        assert!(!is_read_operation_str(""));
    }

    #[test]
    fn read_select() {
        assert!(is_read_operation_str("SELECT * FROM t"));
    }

    #[test]
    fn read_insert_is_not_read() {
        assert!(!is_read_operation_str("INSERT INTO t VALUES (1)"));
    }

    #[test]
    fn read_leading_whitespace() {
        assert!(is_read_operation_str("   SELECT * FROM t"));
    }

    #[test]
    fn read_case_insensitive() {
        assert!(is_read_operation_str("select * from t"));
        assert!(is_read_operation_str("Select * From t"));
    }

    #[test]
    fn read_ffi_null() {
        assert_eq!(pg_config_is_read_operation(std::ptr::null()), 0);
    }

    #[test]
    fn read_ffi_select() {
        let cs = c("SELECT 1");
        assert_eq!(pg_config_is_read_operation(cs.as_ptr()), 1);
    }

    #[test]
    fn read_ffi_insert() {
        let cs = c("INSERT INTO t VALUES (1)");
        assert_eq!(pg_config_is_read_operation(cs.as_ptr()), 0);
    }

    // ── get_retry_delays ─────────────────────────────────────────────────────

    #[test]
    fn retry_delays_default_when_no_env_var() {
        // Ensure the env var is not set for this test.
        // (Other tests may have set it — use a subshell-like approach via
        //  remove_var + restore.)
        let prev = std::env::var("PLEX_PG_RETRY_DELAYS").ok();
        std::env::remove_var("PLEX_PG_RETRY_DELAYS");

        let delays = get_retry_delays_vec();
        assert_eq!(delays, vec![500, 1000, 2000, 3000, 4000]);

        // Restore
        if let Some(v) = prev {
            std::env::set_var("PLEX_PG_RETRY_DELAYS", v);
        }
    }

    #[test]
    fn retry_delays_from_env_var() {
        let prev = std::env::var("PLEX_PG_RETRY_DELAYS").ok();
        std::env::set_var("PLEX_PG_RETRY_DELAYS", "100,200,300");

        let delays = get_retry_delays_vec();
        assert_eq!(delays, vec![100, 200, 300]);

        std::env::remove_var("PLEX_PG_RETRY_DELAYS");
        if let Some(v) = prev {
            std::env::set_var("PLEX_PG_RETRY_DELAYS", v);
        }
    }

    #[test]
    fn retry_delays_capped_at_60000() {
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var("PLEX_PG_RETRY_DELAYS").ok();
        std::env::set_var("PLEX_PG_RETRY_DELAYS", "100,999999,200");

        let delays = get_retry_delays_vec();
        assert_eq!(delays, vec![100, 60_000, 200]);

        std::env::remove_var("PLEX_PG_RETRY_DELAYS");
        if let Some(v) = prev {
            std::env::set_var("PLEX_PG_RETRY_DELAYS", v);
        }
    }

    #[test]
    fn retry_delays_max_10_entries() {
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var("PLEX_PG_RETRY_DELAYS").ok();
        std::env::set_var("PLEX_PG_RETRY_DELAYS", "1,2,3,4,5,6,7,8,9,10,11,12");

        let delays = get_retry_delays_vec();
        assert_eq!(delays.len(), 10);

        std::env::remove_var("PLEX_PG_RETRY_DELAYS");
        if let Some(v) = prev {
            std::env::set_var("PLEX_PG_RETRY_DELAYS", v);
        }
    }

    #[test]
    fn retry_delays_invalid_env_falls_back_to_default() {
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var("PLEX_PG_RETRY_DELAYS").ok();
        std::env::set_var("PLEX_PG_RETRY_DELAYS", "not,valid,numbers!");

        let delays = get_retry_delays_vec();
        assert_eq!(delays, vec![500, 1000, 2000, 3000, 4000]);

        std::env::remove_var("PLEX_PG_RETRY_DELAYS");
        if let Some(v) = prev {
            std::env::set_var("PLEX_PG_RETRY_DELAYS", v);
        }
    }

    #[test]
    fn retry_delays_ffi() {
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var("PLEX_PG_RETRY_DELAYS").ok();
        std::env::remove_var("PLEX_PG_RETRY_DELAYS");

        let mut delays = [0i32; 10];
        let mut count = 0i32;
        pg_config_get_retry_delays(delays.as_mut_ptr(), &mut count);

        assert_eq!(count, 5);
        assert_eq!(&delays[..5], &[500, 1000, 2000, 3000, 4000]);

        if let Some(v) = prev {
            std::env::set_var("PLEX_PG_RETRY_DELAYS", v);
        }
    }
}
