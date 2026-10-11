/// Module: pg_logging
///
/// Core logging backend for the plex PostgreSQL redirect layer.
/// Handles file management, rotation, throttling, and fork safety.
/// Exposed via C FFI; the C shim handles variadic printf formatting
/// and calls into Rust with pre-formatted messages.
///
/// FFI surface:
///   rust_logging_init()
///   rust_logging_get_level() -> i32
///   rust_logging_write(level: i32, message: *const c_char)
///   rust_logging_fallback(original_sql, translated_sql, error_msg, context)
///   rust_logging_is_known_limitation(error_msg: *const c_char) -> i32
///   rust_logging_reset_after_fork()
///   rust_logging_cleanup()
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::raw::c_char;
use std::path::Path;
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::db_interpose_conn_utils::{cstr_to_option, cstr_to_str_safe};
use crate::env_utils;

// ─── Constants ────────────────────────────────────────────────────────────────

const DEFAULT_LOG_FILE: &str = "/tmp/plex_redirect_pg.log";
const FALLBACK_LOG_FILE: &str = "/tmp/plex_pg_fallbacks.log";
const DEFAULT_MAX_SIZE: u64 = 10 * 1024 * 1024; // 10 MB
const ROTATION_CHECK_INTERVAL: u64 = 100;
const THROTTLE_THRESHOLD: u64 = 999_999_999; // effectively infinite
const THROTTLE_SAMPLE_RATE: u64 = 1000;
const THROTTLE_SUMMARY_INTERVAL_SECS: u64 = 10;

// ─── Log level ────────────────────────────────────────────────────────────────

pub const LEVEL_ERROR: i32 = 0;
pub const LEVEL_INFO: i32 = 1;
pub const LEVEL_DEBUG: i32 = 2;

// ─── Global state ─────────────────────────────────────────────────────────────

/// The configured log level (0=ERROR, 1=INFO, 2=DEBUG).
pub static LOG_LEVEL: AtomicI32 = AtomicI32::new(LEVEL_INFO);

/// The configured max file size in bytes.
static MAX_SIZE: AtomicU64 = AtomicU64::new(DEFAULT_MAX_SIZE);

/// Total write count used to decide when to check rotation.
static WRITE_COUNT: AtomicU64 = AtomicU64::new(0);

/// Messages written in the current 1-second window (throttle counter).
static THROTTLE_WINDOW_COUNT: AtomicU64 = AtomicU64::new(0);

/// Unix timestamp (seconds) of the start of the current throttle window.
static THROTTLE_WINDOW_START: AtomicI64 = AtomicI64::new(0);

/// Unix timestamp (seconds) of the last throttle summary message.
static LAST_THROTTLE_SUMMARY: AtomicI64 = AtomicI64::new(0);

/// Whether the system has been initialised.
static INITIALIZED: OnceLock<()> = OnceLock::new();

// ─── Logger state (protected by Mutex) ───────────────────────────────────────

pub(crate) struct LoggerState {
    /// The open log file, or None if using stdout/stderr/not yet open.
    file: Option<File>,
    /// The path to the log file ("stdout" / "stderr" / filesystem path).
    path: String,
    /// Whether the path refers to stdout.
    is_stdout: bool,
    /// Whether the path refers to stderr.
    is_stderr: bool,
}

impl LoggerState {
    fn new() -> Self {
        LoggerState {
            file: None,
            path: default_log_file_path(),
            is_stdout: false,
            is_stderr: false,
        }
    }
}

/// Global logger protected by a Mutex.
/// We wrap it in OnceLock<Mutex<...>> so that rust_logging_reset_after_fork()
/// can replace the inner state without replacing the lock itself.
static LOGGER: OnceLock<Mutex<LoggerState>> = OnceLock::new();

fn logger() -> &'static Mutex<LoggerState> {
    LOGGER.get_or_init(|| Mutex::new(LoggerState::new()))
}

fn tmpdir_log_file_path() -> Option<String> {
    let tmpdir = env_utils::env_string("TMPDIR")?;
    let trimmed = tmpdir.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    Some(format!("{trimmed}/plex_redirect_pg.log"))
}

fn default_log_file_path() -> String {
    if let Some(p) = tmpdir_log_file_path() {
        return p;
    }
    if Path::new("/config").is_dir() {
        return "/config/plex_redirect_pg.log".to_string();
    }
    DEFAULT_LOG_FILE.to_string()
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Parse a max-size string like "10M", "50K", or a plain integer.
/// Returns `DEFAULT_MAX_SIZE` on empty or invalid input.
pub fn parse_max_size(s: &str) -> u64 {
    let s = s.trim();
    if s.is_empty() {
        return DEFAULT_MAX_SIZE;
    }
    let (digits, suffix) = if s.ends_with(['m', 'M']) {
        (&s[..s.len() - 1], 'm')
    } else if s.ends_with(['k', 'K']) {
        (&s[..s.len() - 1], 'k')
    } else {
        (s, ' ')
    };
    match digits.parse::<u64>() {
        Ok(n) => match suffix {
            'm' => n * 1024 * 1024,
            'k' => n * 1024,
            _ => n,
        },
        Err(_) => DEFAULT_MAX_SIZE,
    }
}

/// Parse a log-level string.
/// "DEBUG" → 2, "INFO" → 1, "ERROR" → 0, unrecognised/empty → 0.
pub fn parse_log_level(s: &str) -> i32 {
    match s.trim().to_ascii_uppercase().as_str() {
        "DEBUG" | "2" => LEVEL_DEBUG,
        "INFO" | "1" => LEVEL_INFO,
        "ERROR" | "0" => LEVEL_ERROR,
        _ => LEVEL_ERROR,
    }
}

/// Parse a boolean-like string.
/// Accepts "1", "true", or "yes" (case-insensitive) as true.
pub fn parse_bool(s: &str) -> bool {
    matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes")
}

/// Format the current local time as `[YYYY-MM-DD HH:MM:SS]`.
/// Uses only `std::time::SystemTime` to avoid libc / chrono dependency.
pub fn format_timestamp() -> String {
    // Compute seconds since Unix epoch then derive calendar fields manually.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Simple UTC-based calendar math (good enough for log timestamps).
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;

    // Days since epoch → date
    let days = secs / 86400;
    let (year, month, day) = days_to_ymd(days);

    format!(
        "[{:04}-{:02}-{:02} {:02}:{:02}:{:02}]",
        year, month, day, h, m, s
    )
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Shift to the civil epoch used in Howard Hinnant's algorithm (days since 0000-03-01).
    // Reference: http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Return the level tag string.
fn level_tag(level: i32) -> &'static str {
    match level {
        LEVEL_DEBUG => "[DEBUG]",
        LEVEL_ERROR => "[ERROR]",
        _ => "[INFO]",
    }
}

/// Open (or reopen) the log file described by `path`.
/// Returns `None` for stdout/stderr targets (they don't use a `File`).
fn open_log_file(path: &str) -> std::io::Result<File> {
    if path == "stdout" || path == "stderr" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "stdout/stderr are not file-backed log targets",
        ));
    }
    OpenOptions::new().create(true).append(true).open(path)
}

fn fallback_log_path_for(path: &str, is_stdout: bool, is_stderr: bool) -> String {
    if !is_stdout && !is_stderr {
        if let Some(parent) = Path::new(path).parent() {
            parent
                .join("plex_pg_fallbacks.log")
                .to_string_lossy()
                .into_owned()
        } else {
            FALLBACK_LOG_FILE.to_string()
        }
    } else {
        FALLBACK_LOG_FILE.to_string()
    }
}

/// Check whether the log file has grown beyond `max_size` bytes.
pub(crate) fn should_rotate(state: &LoggerState, max_size: u64) -> bool {
    if state.is_stdout || state.is_stderr {
        return false;
    }
    match &state.file {
        Some(f) => f.metadata().map(|m| m.len() > max_size).unwrap_or(false),
        None => false,
    }
}

/// Rotate the log file: rename current to `<path>.1`, open a fresh file.
fn rotate(state: &mut LoggerState) {
    let backup = format!("{}.1", state.path);
    // Close existing handle before renaming.
    state.file = None;
    let _ = std::fs::rename(&state.path, &backup);
    state.file = open_log_file(&state.path).ok();
}

/// Write a formatted line directly to the log destination.
fn write_line(state: &mut LoggerState, line: &str) {
    if state.is_stdout {
        let _ = writeln!(std::io::stdout(), "{}", line);
    } else if state.is_stderr {
        let _ = writeln!(std::io::stderr(), "{}", line);
    } else {
        // Lazily open the file if not yet open.
        if state.file.is_none() {
            state.file = open_log_file(&state.path).ok();
        }
        if let Some(ref mut f) = state.file {
            let _ = writeln!(f, "{}", line);
        } else {
            let _ = writeln!(std::io::stderr(), "{}", line);
        }
    }
}

/// Core of `is_known_limitation` operating on a Rust `&str`.
pub fn is_known_limitation_str(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    const PATTERNS: &[&str] = &[
        "fts4",
        "fts5",
        "spellfix",
        "rtree",
        "json_tree",
        "virtual table",
        "no such function: unicode",
        "fts3_tokenizer",
    ];
    PATTERNS.iter().any(|p| lower.contains(p))
}

/// Return the current Unix time in seconds.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ─── FFI functions ────────────────────────────────────────────────────────────

/// Initialize the logging system.
///
/// Reads environment variables:
///   `PLEX_PG_LOG_LEVEL`    → "DEBUG"=2, "ERROR"=0, else INFO=1
///   `PLEX_PG_LOG_FILE`     → file path, "stdout", "stderr", or default
///   `PLEX_PG_LOG_MAX_SIZE` → "10M", "50K", or raw bytes (default 10 MB)
///   `PLEX_PG_LOG_TRUNCATE_ON_START` → "1"/"true"/"yes" to truncate on init
///
/// Opens the log file and sets it unbuffered. Thread-safe; call once.
pub fn rust_logging_init() {
    // Ensure we only initialise once.
    INITIALIZED.get_or_init(|| {
        // Parse env vars.
        let level = env_utils::env_string("PLEX_PG_LOG_LEVEL")
            .map(|v| parse_log_level(&v))
            .unwrap_or(LEVEL_INFO);

        let requested_path =
            env_utils::env_string_or_else("PLEX_PG_LOG_FILE", default_log_file_path);

        let max_size = env_utils::env_string("PLEX_PG_LOG_MAX_SIZE")
            .map(|v| parse_max_size(&v))
            .unwrap_or(DEFAULT_MAX_SIZE);

        let truncate_on_start = env_utils::env_string("PLEX_PG_LOG_TRUNCATE_ON_START")
            .map(|v| parse_bool(&v))
            .unwrap_or(false);

        LOG_LEVEL.store(level, Ordering::Relaxed);
        MAX_SIZE.store(max_size, Ordering::Relaxed);

        let mut path = requested_path.clone();
        let mut is_stdout = requested_path == "stdout";
        let mut is_stderr = requested_path == "stderr";
        let mut file = if is_stdout || is_stderr {
            None
        } else {
            match open_log_file(&requested_path) {
                Ok(f) => Some(f),
                Err(first_err) => {
                    let mut fallback_paths: Vec<String> = Vec::new();
                    if let Some(p) = tmpdir_log_file_path() {
                        fallback_paths.push(p);
                    }
                    fallback_paths.push("/config/plex_redirect_pg.log".to_string());
                    fallback_paths.push(DEFAULT_LOG_FILE.to_string());
                    fallback_paths.retain(|p| p != &requested_path);

                    let mut opened = None;
                    for fallback in fallback_paths {
                        if let Ok(f) = open_log_file(&fallback) {
                            path = fallback;
                            opened = Some(f);
                            break;
                        }
                    }

                    if opened.is_none() {
                        // Keep logging available even if file targets fail.
                        let _ = writeln!(
                            std::io::stderr(),
                            "pg_logging: failed to open '{}' ({}) and fallback paths; using stderr",
                            requested_path,
                            first_err
                        );
                        path = "stderr".to_string();
                        is_stderr = true;
                        is_stdout = false;
                    }
                    opened
                }
            }
        };

        if truncate_on_start && !is_stdout && !is_stderr {
            let _ = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path);
            file = open_log_file(&path).ok();
        }

        if truncate_on_start {
            let fallback_path = fallback_log_path_for(&path, is_stdout, is_stderr);
            let _ = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&fallback_path);
        }

        let mut state = logger().lock().unwrap_or_else(|e| e.into_inner());
        state.path = path;
        state.is_stdout = is_stdout;
        state.is_stderr = is_stderr;
        state.file = file;
    });
}

/// Returns the current log level (0=ERROR, 1=INFO, 2=DEBUG).
/// Used by the C shim for fast-path filtering before message formatting.
#[no_mangle]
pub extern "C" fn rust_logging_get_level() -> i32 {
    LOG_LEVEL.load(Ordering::Relaxed)
}

/// Write a pre-formatted log message.
///
/// Called by the C shim after `vsnprintf`. Handles timestamp prefix,
/// throttling, rotation check, and mutex-protected write.
///
/// `level`   — 0=ERROR, 1=INFO, 2=DEBUG  
/// `message` — pre-formatted C string (no trailing newline)
#[no_mangle]
pub extern "C" fn rust_logging_write(level: i32, message: *const c_char) {
    // Fast-path level filter.
    if level > LOG_LEVEL.load(Ordering::Relaxed) {
        return;
    }

    if message.is_null() {
        return;
    }

    let msg = cstr_to_str_safe(message);

    // Throttle check (ERROR bypasses completely).
    if level != LEVEL_ERROR {
        let now = now_secs();
        let window_start = THROTTLE_WINDOW_START.load(Ordering::Relaxed);

        if now != window_start {
            // New second — reset window.
            THROTTLE_WINDOW_START.store(now, Ordering::Relaxed);
            THROTTLE_WINDOW_COUNT.store(1, Ordering::Relaxed);
        } else {
            let count = THROTTLE_WINDOW_COUNT.fetch_add(1, Ordering::Relaxed) + 1;

            if count > THROTTLE_THRESHOLD {
                // Throttled: only emit 1-in-1000 samples.
                if !count.is_multiple_of(THROTTLE_SAMPLE_RATE) {
                    return;
                }
                // Periodically emit a throttle summary.
                let last_summary = LAST_THROTTLE_SUMMARY.load(Ordering::Relaxed);
                if now - last_summary >= THROTTLE_SUMMARY_INTERVAL_SECS as i64 {
                    LAST_THROTTLE_SUMMARY.store(now, Ordering::Relaxed);
                    let summary = format!(
                        "{} [INFO] pg_logging: throttled — {} msgs/sec",
                        format_timestamp(),
                        count
                    );
                    if let Ok(mut state) = logger().try_lock() {
                        write_line(&mut state, &summary);
                    }
                }
                return;
            }
        }
    }

    let line = format!("{} {} {}", format_timestamp(), level_tag(level), msg);

    // Use try_lock to avoid deadlock when called while holding a stmt/conn mutex.
    // If the logger is contended, fall back to writing directly to stderr.
    match logger().try_lock() {
        Ok(mut state) => {
            // Rotation check every N writes.
            let count = WRITE_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            if count.is_multiple_of(ROTATION_CHECK_INTERVAL) {
                let max_size = MAX_SIZE.load(Ordering::Relaxed);
                if should_rotate(&state, max_size) {
                    rotate(&mut state);
                }
            }

            write_line(&mut state, &line);
        }
        Err(_) => {
            // Logger contended — write to stderr to avoid deadlock.
            let bytes = line.as_bytes();
            unsafe {
                libc::write(libc::STDERR_FILENO, bytes.as_ptr() as *const _, bytes.len());
                libc::write(libc::STDERR_FILENO, b"\n".as_ptr() as *const _, 1);
            }
        }
    }
}

/// Write to the fallback log file and also to the main log.
///
/// Used for SQL fallback tracking. Arguments are pre-formatted C strings.
pub fn rust_logging_fallback(
    original_sql: *const c_char,
    translated_sql: *const c_char,
    error_msg: *const c_char,
    context: *const c_char,
) {
    let c_to_str = |ptr: *const c_char| -> &str { cstr_to_option(ptr).unwrap_or("<null>") };

    let original = c_to_str(original_sql);
    let translated = c_to_str(translated_sql);
    let error = c_to_str(error_msg);
    let ctx = c_to_str(context);

    let ts = format_timestamp();
    let fallback_line = format!(
        "{} [FALLBACK] context={} | original={} | translated={} | error={}",
        ts, ctx, original, translated, error
    );

    // Write to the dedicated fallback log file and main log.
    // Use try_lock to avoid deadlock when called from mutex-holding contexts.
    if let Ok(state) = logger().try_lock() {
        let fallback_path = fallback_log_path_for(&state.path, state.is_stdout, state.is_stderr);
        drop(state);

        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&fallback_path)
        {
            let _ = writeln!(f, "{}", fallback_line);
        }
    }

    // Also write to the main log (non-blocking).
    if let Ok(mut state) = logger().try_lock() {
        write_line(&mut state, &fallback_line);
    }
}

/// Check if an error message matches known translation limitations.
///
/// Returns 1 if it's a known limitation (caller should suppress logging),
/// 0 otherwise.
pub fn rust_logging_is_known_limitation(error_msg: *const c_char) -> i32 {
    let msg = match cstr_to_option(error_msg) {
        Some(s) => s,
        None => return 0,
    };
    if is_known_limitation_str(msg) {
        1
    } else {
        0
    }
}

/// Reset logging state after `fork()`. Called in the child process.
///
/// Clears the file handle (child will reopen on next write) and resets
/// all atomic counters. The `Mutex` is reinitialised by replacing the
/// inner state through a fresh lock acquisition.
pub fn rust_logging_reset_after_fork() {
    // Reset atomics.
    WRITE_COUNT.store(0, Ordering::Relaxed);
    THROTTLE_WINDOW_COUNT.store(0, Ordering::Relaxed);
    THROTTLE_WINDOW_START.store(0, Ordering::Relaxed);
    LAST_THROTTLE_SUMMARY.store(0, Ordering::Relaxed);

    // Drop the existing file handle so the child doesn't share the
    // parent's file descriptor; it will be reopened lazily on next write.
    // We use `lock()` with a poison-recovery fallback.
    let mut state = logger().lock().unwrap_or_else(|e| e.into_inner());
    // Close the file by replacing it with None.
    state.file = None;
    // Mark as not initialised so rust_logging_init() can be called again
    // in the child if desired. We cannot reset OnceLock, but the child can
    // just start writing and the file will be reopened lazily.
}

/// Flush and close all log files.
pub fn rust_logging_cleanup() {
    let mut state = logger().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(ref mut f) = state.file {
        let _ = f.flush();
    }
    state.file = None;
}

// ─── C ABI wrappers (pg_logging.c replacement) ───────────────────────────────

#[no_mangle]
pub extern "C" fn pg_logging_init() {
    rust_logging_init();
}

#[no_mangle]
pub extern "C" fn pg_logging_cleanup() {
    rust_logging_cleanup();
}

#[no_mangle]
pub extern "C" fn pg_logging_reset_after_fork() {
    rust_logging_reset_after_fork();
}

#[no_mangle]
pub extern "C" fn log_sql_fallback(
    original_sql: *const c_char,
    translated_sql: *const c_char,
    error_msg: *const c_char,
    context: *const c_char,
) {
    rust_logging_fallback(original_sql, translated_sql, error_msg, context);
}

#[no_mangle]
pub extern "C" fn is_known_translation_limitation(error_msg: *const c_char) -> i32 {
    rust_logging_is_known_limitation(error_msg)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_max_size ──────────────────────────────────────────────────────

    #[test]
    fn parse_max_size_megabytes() {
        assert_eq!(parse_max_size("10M"), 10 * 1024 * 1024);
    }

    #[test]
    fn parse_max_size_megabytes_lower() {
        assert_eq!(parse_max_size("10m"), 10 * 1024 * 1024);
    }

    #[test]
    fn parse_max_size_kilobytes() {
        assert_eq!(parse_max_size("50K"), 50 * 1024);
    }

    #[test]
    fn parse_max_size_kilobytes_lower() {
        assert_eq!(parse_max_size("50k"), 50 * 1024);
    }

    #[test]
    fn parse_max_size_raw_bytes() {
        assert_eq!(parse_max_size("1234"), 1234);
    }

    #[test]
    fn parse_max_size_empty_returns_default() {
        assert_eq!(parse_max_size(""), DEFAULT_MAX_SIZE);
    }

    #[test]
    fn parse_max_size_invalid_returns_default() {
        assert_eq!(parse_max_size("notanumber"), DEFAULT_MAX_SIZE);
    }

    #[test]
    fn parse_max_size_whitespace() {
        assert_eq!(parse_max_size("  5M  "), 5 * 1024 * 1024);
    }

    // ── parse_log_level ─────────────────────────────────────────────────────

    #[test]
    fn parse_log_level_debug() {
        assert_eq!(parse_log_level("DEBUG"), LEVEL_DEBUG);
    }

    #[test]
    fn parse_log_level_error() {
        assert_eq!(parse_log_level("ERROR"), LEVEL_ERROR);
    }

    #[test]
    fn parse_log_level_info_upper() {
        assert_eq!(parse_log_level("INFO"), LEVEL_INFO);
    }

    #[test]
    fn parse_log_level_info_lower() {
        assert_eq!(parse_log_level("info"), LEVEL_INFO);
    }

    #[test]
    fn parse_log_level_debug_lower() {
        assert_eq!(parse_log_level("debug"), LEVEL_DEBUG);
    }

    #[test]
    fn parse_log_level_empty_returns_error() {
        assert_eq!(parse_log_level(""), LEVEL_ERROR);
    }

    #[test]
    fn parse_log_level_unknown_returns_error() {
        assert_eq!(parse_log_level("VERBOSE"), LEVEL_ERROR);
    }

    #[test]
    fn parse_log_level_numeric() {
        assert_eq!(parse_log_level("0"), LEVEL_ERROR);
        assert_eq!(parse_log_level("1"), LEVEL_INFO);
        assert_eq!(parse_log_level("2"), LEVEL_DEBUG);
    }

    // ── is_known_limitation_str ─────────────────────────────────────────────

    #[test]
    fn known_limitation_fts4() {
        assert!(is_known_limitation_str("no such table: fts4_content"));
    }

    #[test]
    fn known_limitation_fts5() {
        assert!(is_known_limitation_str("error using fts5 tokenizer"));
    }

    #[test]
    fn known_limitation_spellfix() {
        assert!(is_known_limitation_str("spellfix1 module not found"));
    }

    #[test]
    fn known_limitation_rtree() {
        assert!(is_known_limitation_str("rtree: unsupported operation"));
    }

    #[test]
    fn known_limitation_json_tree() {
        assert!(is_known_limitation_str("no such function: json_tree"));
    }

    #[test]
    fn known_limitation_virtual_table() {
        assert!(is_known_limitation_str("no such virtual table: t"));
    }

    #[test]
    fn known_limitation_unicode_function() {
        assert!(is_known_limitation_str("no such function: unicode"));
    }

    #[test]
    fn known_limitation_fts3_tokenizer() {
        assert!(is_known_limitation_str("fts3_tokenizer disabled"));
    }

    #[test]
    fn known_limitation_case_insensitive() {
        assert!(is_known_limitation_str("FTS5 error"));
        assert!(is_known_limitation_str("RTREE problem"));
        assert!(is_known_limitation_str("Virtual Table missing"));
    }

    #[test]
    fn known_limitation_non_matching() {
        assert!(!is_known_limitation_str("syntax error near SELECT"));
        assert!(!is_known_limitation_str("column not found"));
        assert!(!is_known_limitation_str(""));
    }

    // ── format_timestamp ────────────────────────────────────────────────────

    #[test]
    fn format_timestamp_looks_correct() {
        let ts = format_timestamp();
        // Should be exactly "[YYYY-MM-DD HH:MM:SS]"
        assert_eq!(ts.len(), 21, "unexpected length: '{}'", ts);
        assert!(ts.starts_with('['), "should start with '['");
        assert!(ts.ends_with(']'), "should end with ']'");
        // Basic structure check: [NNNN-NN-NN NN:NN:NN]
        let inner = &ts[1..20];
        assert_eq!(&inner[4..5], "-");
        assert_eq!(&inner[7..8], "-");
        assert_eq!(&inner[10..11], " ");
        assert_eq!(&inner[13..14], ":");
        assert_eq!(&inner[16..17], ":");
    }

    #[test]
    fn format_timestamp_year_reasonable() {
        let ts = format_timestamp();
        let year: u32 = ts[1..5].parse().expect("year should be numeric");
        assert!((2024..=2100).contains(&year), "year out of range: {}", year);
    }

    // ── should_rotate ───────────────────────────────────────────────────────

    #[test]
    fn should_rotate_no_file_returns_false() {
        let state = LoggerState {
            file: None,
            path: "/tmp/nonexistent_pg_log_test".to_string(),
            is_stdout: false,
            is_stderr: false,
        };
        // No file → never rotate
        assert!(!should_rotate(&state, 100));
    }

    #[test]
    fn should_rotate_stdout_returns_false() {
        let state = LoggerState {
            file: None,
            path: "stdout".to_string(),
            is_stdout: true,
            is_stderr: false,
        };
        assert!(!should_rotate(&state, 0));
    }

    #[test]
    fn should_rotate_stderr_returns_false() {
        let state = LoggerState {
            file: None,
            path: "stderr".to_string(),
            is_stdout: false,
            is_stderr: true,
        };
        assert!(!should_rotate(&state, 0));
    }

    #[test]
    fn should_rotate_small_file_returns_false() {
        use std::io::Write;
        let path = format!("/tmp/pg_log_test_small_{}.log", std::process::id());
        {
            let mut f = std::fs::File::create(&path).unwrap();
            write!(f, "hello").unwrap();
        }
        let file = std::fs::File::open(&path).unwrap();
        let state = LoggerState {
            file: Some(file),
            path: path.clone(),
            is_stdout: false,
            is_stderr: false,
        };
        // 5 bytes is well under 1024 limit
        assert!(!should_rotate(&state, 1024));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn should_rotate_large_file_returns_true() {
        use std::io::Write;
        let path = format!("/tmp/pg_log_test_large_{}.log", std::process::id());
        {
            let mut f = std::fs::File::create(&path).unwrap();
            // Write 200 bytes; limit is 100
            write!(f, "{}", "x".repeat(200)).unwrap();
        }
        let file = std::fs::File::open(&path).unwrap();
        let state = LoggerState {
            file: Some(file),
            path: path.clone(),
            is_stdout: false,
            is_stderr: false,
        };
        assert!(should_rotate(&state, 100));
        let _ = std::fs::remove_file(&path);
    }

    // ── days_to_ymd ─────────────────────────────────────────────────────────

    #[test]
    fn days_to_ymd_epoch() {
        // Day 0 = 1970-01-01
        let (y, m, d) = days_to_ymd(0);
        assert_eq!((y, m, d), (1970, 1, 1));
    }

    #[test]
    fn days_to_ymd_known_date() {
        // 2024-01-01 is 19723 days after Unix epoch
        let (y, m, d) = days_to_ymd(19723);
        assert_eq!((y, m, d), (2024, 1, 1));
    }

    // ── level_tag ───────────────────────────────────────────────────────────

    #[test]
    fn level_tag_values() {
        assert_eq!(level_tag(LEVEL_ERROR), "[ERROR]");
        assert_eq!(level_tag(LEVEL_INFO), "[INFO]");
        assert_eq!(level_tag(LEVEL_DEBUG), "[DEBUG]");
        assert_eq!(level_tag(99), "[INFO]"); // unknown → INFO tag
    }
}
