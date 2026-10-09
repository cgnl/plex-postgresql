use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::sync::{Arc, Mutex, OnceLock};

use crate::db_interpose_conn_utils::PthreadMutexGuard;
use crate::ffi_types::PgConnection;
use crate::libpq_helpers::*;

#[derive(Default)]
struct TransactionState {
    active: bool,
    implicit: bool,
    session: usize,
    next_savepoint: u64,
    savepoints: Vec<(String, String)>,
}

type States = HashMap<usize, Arc<Mutex<TransactionState>>>;
static STATES: OnceLock<Mutex<States>> = OnceLock::new();

fn states() -> &'static Mutex<States> {
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn state_for(conn: *mut PgConnection) -> Arc<Mutex<TransactionState>> {
    states()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .entry(conn as usize)
        .or_default()
        .clone()
}

pub(crate) fn forget_connection(conn: *mut PgConnection) {
    states()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .remove(&(conn as usize));
}

pub(crate) fn transaction_active(conn: *mut PgConnection) -> bool {
    let state = state_for(conn);
    let active = state
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .active;
    active
}

pub(crate) unsafe fn session_ready_locked(conn: *mut PgConnection) -> bool {
    if conn.is_null() {
        return false;
    }
    if (*conn).conn.is_null() || rust_pq_status((*conn).conn) != super::session::CONNECTION_OK {
        set_error(
            conn,
            "PostgreSQL handle session unavailable; session was not replaced",
        );
        return false;
    }
    let state = state_for(conn);
    let state = state.lock().unwrap_or_else(|poison| poison.into_inner());
    if state.active
        && (state.session != (*conn).conn as usize || rust_pq_transaction_status((*conn).conn) == 0)
    {
        set_error(
            conn,
            "PostgreSQL transaction session was lost; refusing execution",
        );
        return false;
    }
    true
}

pub(crate) fn session_ready(conn: *mut PgConnection) -> bool {
    if conn.is_null() {
        return false;
    }
    unsafe {
        let _guard = PthreadMutexGuard::lock(&mut (*conn).mutex);
        session_ready_locked(conn)
    }
}

pub(crate) unsafe fn result_stream_live_locked(conn: *mut PgConnection) -> bool {
    if (*conn)
        .streaming_active
        .load(std::sync::atomic::Ordering::Acquire)
        == 0
    {
        return false;
    }
    let status = rust_pq_transaction_status((*conn).conn);
    if matches!(status, 0 | 2 | 3) && rust_pq_is_busy((*conn).conn) == 0 {
        (*conn)
            .streaming_active
            .store(0, std::sync::atomic::Ordering::Release);
        return false;
    }
    true
}

pub(crate) fn ensure_handle_session(conn: *mut PgConnection) {
    if conn.is_null() {
        return;
    }
    unsafe {
        let _guard = PthreadMutexGuard::lock(&mut (*conn).mutex);
        if transaction_active(conn) {
            return;
        }
        let config = super::conn_config();
        if !(*conn).conn.is_null() {
            if rust_pq_status((*conn).conn) != super::session::CONNECTION_OK {
                rust_pq_reset((*conn).conn);
                if rust_pq_status((*conn).conn) == super::session::CONNECTION_OK {
                    super::session::pg_set_socket_timeout((*conn).conn);
                    super::session::apply_session_settings((*conn).conn, &config.schema, false);
                    super::rust_stmt_cache_clear(conn.cast());
                    (*conn)
                        .streaming_active
                        .store(0, std::sync::atomic::Ordering::Release);
                }
            }
            return;
        }
        let Ok(conninfo) = CString::new(super::connection_lifecycle::build_conninfo(config, true))
        else {
            set_error(conn, "Invalid PostgreSQL connection configuration");
            return;
        };
        let session = rust_pq_connectdb(conninfo.as_ptr());
        if session.is_null() || rust_pq_status(session) != super::session::CONNECTION_OK {
            set_error(conn, "PostgreSQL handle session unavailable");
            if !session.is_null() {
                rust_pq_finish(session);
            }
            return;
        }
        super::session::pg_set_socket_timeout(session);
        super::session::apply_session_settings(session, &config.schema, false);
        (*conn).conn = session;
    }
}

struct Token {
    text: String,
    quoted: bool,
}

impl Token {
    fn keyword(&self, keyword: &str) -> bool {
        !self.quoted && self.text.eq_ignore_ascii_case(keyword)
    }
}

fn tokenize(sql: &str) -> Result<Vec<Token>, &'static str> {
    let mut chars = sql.chars().peekable();
    let mut tokens = Vec::new();
    let mut ended = false;
    while let Some(character) = chars.next() {
        if character.is_whitespace() {
            continue;
        }
        if character == '-' && chars.peek() == Some(&'-') {
            chars.next();
            for comment in chars.by_ref() {
                if comment == '\n' {
                    break;
                }
            }
            continue;
        }
        if character == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut closed = false;
            while let Some(comment) = chars.next() {
                if comment == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    closed = true;
                    break;
                }
            }
            if !closed {
                return Err("Unterminated SQL comment");
            }
            continue;
        }
        if ended {
            return Err("Expected a single transaction control statement");
        }
        if character == ';' {
            ended = true;
            continue;
        }
        let quoted = matches!(character, '"' | '\'' | '`' | '[');
        let mut text = String::new();
        if quoted {
            let delimiter = if character == '[' { ']' } else { character };
            let mut closed = false;
            while let Some(part) = chars.next() {
                if part == delimiter {
                    if delimiter != ']' && chars.peek() == Some(&delimiter) {
                        chars.next();
                        text.push(delimiter);
                    } else {
                        closed = true;
                        break;
                    }
                } else {
                    text.push(part);
                }
            }
            if !closed {
                return Err("Unterminated savepoint name");
            }
        } else {
            if !(character.is_alphabetic() || character == '_') {
                return Err("Invalid transaction control syntax");
            }
            text.push(character);
            while let Some(part) = chars.peek() {
                if !(part.is_alphanumeric() || matches!(part, '_' | '$')) {
                    break;
                }
                text.push(chars.next().unwrap());
            }
        }
        tokens.push(Token { text, quoted });
    }
    Ok(tokens)
}

pub(crate) fn is_transaction_sql(sql: &str) -> bool {
    let sql = crate::pg_config::strip_leading_ws_and_sql_comments(sql);
    let keyword = sql
        .split(|character: char| !character.is_ascii_alphabetic())
        .next()
        .unwrap_or("");
    ["begin", "commit", "end", "rollback", "savepoint", "release"]
        .iter()
        .any(|candidate| keyword.eq_ignore_ascii_case(candidate))
}

enum Control {
    Begin,
    Commit,
    Rollback,
    Savepoint(String),
    Release(String),
    RollbackTo(String),
}

fn parse(sql: &str) -> Result<Control, &'static str> {
    let tokens = tokenize(sql)?;
    let Some(first) = tokens.first() else {
        return Err("Empty transaction control statement");
    };
    let mut position = 1;
    if first.keyword("begin")
        && tokens.get(position).is_some_and(|token| {
            token.keyword("deferred") || token.keyword("immediate") || token.keyword("exclusive")
        })
    {
        position += 1;
    }
    if (first.keyword("begin")
        || first.keyword("commit")
        || first.keyword("end")
        || first.keyword("rollback"))
        && tokens
            .get(position)
            .is_some_and(|token| token.keyword("transaction"))
    {
        position += 1;
    }
    let named = if first.keyword("savepoint") {
        Some(0)
    } else if first.keyword("release") {
        if tokens
            .get(position)
            .is_some_and(|token| token.keyword("savepoint"))
        {
            position += 1;
        }
        Some(1)
    } else if first.keyword("rollback")
        && tokens
            .get(position)
            .is_some_and(|token| token.keyword("to"))
    {
        position += 1;
        if tokens
            .get(position)
            .is_some_and(|token| token.keyword("savepoint"))
        {
            position += 1;
        }
        Some(2)
    } else {
        None
    };
    if let Some(kind) = named {
        let name = tokens
            .get(position)
            .ok_or("Missing savepoint name")?
            .text
            .clone();
        if position + 1 != tokens.len() {
            return Err("Unexpected transaction control tokens");
        }
        return Ok(match kind {
            0 => Control::Savepoint(name),
            1 => Control::Release(name),
            _ => Control::RollbackTo(name),
        });
    }
    if position != tokens.len() {
        return Err("Unexpected transaction control tokens");
    }
    if first.keyword("begin") {
        Ok(Control::Begin)
    } else if first.keyword("commit") || first.keyword("end") {
        Ok(Control::Commit)
    } else if first.keyword("rollback") {
        Ok(Control::Rollback)
    } else {
        Err("Invalid transaction control statement")
    }
}

pub(crate) unsafe fn set_error(conn: *mut PgConnection, message: &str) -> c_int {
    if !conn.is_null() {
        (*conn).last_error_code = 1;
        (*conn).last_error.fill(0);
        let capacity = (*conn).last_error.len().saturating_sub(1);
        for (destination, source) in (*conn)
            .last_error
            .iter_mut()
            .take(capacity)
            .zip(message.bytes())
        {
            *destination = source as c_char;
        }
    }
    1
}

unsafe fn command(conn: *mut PgConnection, sql: &str) -> Result<(), c_int> {
    let sql = CString::new(sql).map_err(|_| set_error(conn, "Invalid transaction SQL"))?;
    let result = rust_pq_exec((*conn).conn, sql.as_ptr());
    let success = !result.is_null() && rust_pq_result_status(result) == 1;
    if !success {
        crate::db_interpose_exec::pg_path::record_pg_result_error(conn, conn, result);
    }
    if !result.is_null() {
        rust_pq_clear(result);
    }
    if success {
        Ok(())
    } else {
        Err(1)
    }
}

/// Returns None for non-control SQL; SQLite OK/ERROR for a handled control.
/// The caller must provide a live, registered handle connection, not a pool slot.
pub(crate) fn execute_transaction(conn: *mut PgConnection, sql: &str) -> Option<c_int> {
    if !is_transaction_sql(sql) {
        return None;
    }
    if conn.is_null() {
        return Some(1);
    }
    Some(unsafe { execute(conn, sql) }.unwrap_or_else(|error| error))
}

unsafe fn execute(conn: *mut PgConnection, sql: &str) -> Result<c_int, c_int> {
    let _guard = PthreadMutexGuard::lock(&mut (*conn).mutex);
    let control = parse(sql).map_err(|message| set_error(conn, message))?;
    if (*conn).conn.is_null() || rust_pq_status((*conn).conn) != super::session::CONNECTION_OK {
        return Err(set_error(
            conn,
            "PostgreSQL transaction session unavailable; outcome may be unknown",
        ));
    }
    if result_stream_live_locked(conn) {
        return Err(set_error(
            conn,
            "Transaction control cannot interrupt an active PostgreSQL result",
        ));
    }
    let state = state_for(conn);
    let mut state = state.lock().unwrap_or_else(|poison| poison.into_inner());
    let status = rust_pq_transaction_status((*conn).conn);
    if state.active && (state.session != (*conn).conn as usize || status == 0) {
        return Err(set_error(
            conn,
            "PostgreSQL transaction session was lost; refusing transaction control",
        ));
    }
    match control {
        Control::Begin => {
            if state.active || status != 0 {
                return Err(set_error(
                    conn,
                    "cannot start a transaction within a transaction",
                ));
            }
            command(conn, "BEGIN")?;
            state.active = true;
            state.implicit = false;
            state.session = (*conn).conn as usize;
        }
        Control::Commit | Control::Rollback => {
            if !state.active {
                return Err(set_error(conn, "no transaction is active"));
            }
            if matches!(control, Control::Commit) && status == 3 {
                return Err(set_error(
                    conn,
                    "Cannot commit an aborted PostgreSQL transaction; rollback required",
                ));
            }
            command(
                conn,
                if matches!(control, Control::Commit) {
                    "COMMIT"
                } else {
                    "ROLLBACK"
                },
            )?;
            *state = TransactionState::default();
        }
        Control::Savepoint(name) => {
            if !state.active {
                if status != 0 {
                    return Err(set_error(conn, "Untracked PostgreSQL transaction"));
                }
                command(conn, "BEGIN")?;
                state.active = true;
                state.implicit = true;
                state.session = (*conn).conn as usize;
            }
            state.next_savepoint += 1;
            let pg_name = format!("plex_sqlite_savepoint_{}", state.next_savepoint);
            command(conn, &format!("SAVEPOINT {pg_name}"))?;
            state.savepoints.push((name, pg_name));
        }
        Control::Release(ref name) | Control::RollbackTo(ref name) => {
            let position = state
                .savepoints
                .iter()
                .rposition(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
                .ok_or_else(|| set_error(conn, "no such savepoint"))?;
            if matches!(control, Control::Release(_)) {
                if state.implicit && position == 0 {
                    if status == 3 {
                        return Err(set_error(
                            conn,
                            "Cannot release an aborted transaction; rollback required",
                        ));
                    }
                    command(conn, "COMMIT")?;
                    *state = TransactionState::default();
                } else {
                    command(
                        conn,
                        &format!("RELEASE SAVEPOINT {}", state.savepoints[position].1),
                    )?;
                    state.savepoints.truncate(position);
                }
            } else {
                command(
                    conn,
                    &format!("ROLLBACK TO SAVEPOINT {}", state.savepoints[position].1),
                )?;
                state.savepoints.truncate(position + 1);
            }
        }
    }
    (*conn).last_error_code = 0;
    (*conn).last_error.fill(0);
    Ok(0)
}
