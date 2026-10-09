//! Serialize replacement against all writers before taking its statement snapshot.
//! Callers retain their existing connection mutex throughout this scope.
use crate::ffi_types::PgConnection;
use crate::libpq_helpers::{self as pq, PGconn, PGresult};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_SAVEPOINT: AtomicU64 = AtomicU64::new(1);

trait Commands {
    type Result: Copy;
    fn transaction_status(&self) -> i32;
    fn command(&mut self, sql: &str) -> Self::Result;
    fn successful(&self, result: Self::Result) -> bool;
    fn read_committed(&self, result: Self::Result) -> bool;
    fn clear(&mut self, result: Self::Result);
}

struct Libpq(*mut PGconn);
impl Commands for Libpq {
    type Result = *mut PGresult;
    fn transaction_status(&self) -> i32 {
        pq::rust_pq_transaction_status(self.0)
    }
    fn command(&mut self, sql: &str) -> Self::Result {
        let command = CString::new(sql).expect("validated replacement commands contain no NUL");
        pq::rust_pq_exec(self.0, command.as_ptr())
    }
    fn successful(&self, result: Self::Result) -> bool {
        matches!(pq::rust_pq_result_status(result), 1 | 2)
    }
    fn read_committed(&self, result: Self::Result) -> bool {
        let mut value = [0 as c_char; 64];
        crate::db_interpose_helpers::rust_pg_result_text_copy(
            result.cast(),
            0,
            0,
            value.as_mut_ptr(),
            value.len(),
        ) >= 0
            && unsafe { CStr::from_ptr(value.as_ptr()).to_bytes() == b"read committed" }
    }
    fn clear(&mut self, result: Self::Result) {
        pq::rust_pq_clear(result);
    }
}

struct Scope<'a, C: Commands> {
    commands: &'a mut C,
    savepoint: Option<String>,
    active: bool,
}
impl<C: Commands> Scope<'_, C> {
    // Return cleanup failures to the caller instead of reporting a successful write.
    fn rollback(&mut self) -> Option<C::Result> {
        if !self.active {
            return None;
        }
        self.active = false;
        // COMMIT can fail after the server already ended the transaction. Never
        // issue rollback against some later transaction or replace the session.
        if self.commands.transaction_status() == 0 {
            return None;
        }
        let command = match &self.savepoint {
            Some(name) => format!("ROLLBACK TO SAVEPOINT {name}"),
            None => "ROLLBACK".to_owned(),
        };
        let result = self.commands.command(&command);
        if !self.commands.successful(result) {
            return Some(result);
        }
        self.commands.clear(result);
        if let Some(name) = &self.savepoint {
            let result = self.commands.command(&format!("RELEASE SAVEPOINT {name}"));
            if !self.commands.successful(result) {
                return Some(result);
            }
            self.commands.clear(result);
        }
        None
    }
    fn failed(&mut self, result: C::Result) -> C::Result {
        if let Some(cleanup_error) = self.rollback() {
            self.commands.clear(result);
            cleanup_error
        } else {
            result
        }
    }
}
impl<C: Commands> Drop for Scope<'_, C> {
    fn drop(&mut self) {
        // Also handles a Rust unwind while the mutex is still held. No reconnect,
        // error clearing, or rollback of work outside our owned scope occurs.
        if let Some(result) = self.rollback() {
            self.commands.clear(result);
        }
    }
}

fn execute_scoped<C: Commands>(
    commands: &mut C,
    relation: &str,
    execute: impl FnOnce() -> C::Result,
) -> C::Result {
    let state = commands.transaction_status();
    // In an already failed caller transaction, run the original command so PG
    // returns 25P02. We must not silently recover or discard earlier caller work.
    if !matches!(state, 0 | 2) {
        return execute();
    }
    let savepoint = (state == 2).then(|| {
        format!(
            "plex_pg_replace_{}",
            NEXT_SAVEPOINT.fetch_add(1, Ordering::Relaxed)
        )
    });
    let begin = match &savepoint {
        Some(name) => format!("SAVEPOINT {name}"),
        None => "BEGIN".to_owned(),
    };
    let result = commands.command(&begin);
    if !commands.successful(result) {
        return result;
    }
    commands.clear(result);
    let mut scope = Scope {
        commands,
        savepoint,
        active: true,
    };
    let isolation = scope.commands.command("SHOW transaction_isolation");
    if !scope.commands.successful(isolation) {
        return scope.failed(isolation);
    }
    let read_committed = scope.commands.read_committed(isolation);
    scope.commands.clear(isolation);
    if !read_committed {
        let failure = scope.commands.command(
            "DO $$ BEGIN RAISE EXCEPTION 'SQLite replacement requires PostgreSQL READ COMMITTED isolation' USING ERRCODE = '0A000'; END $$",
        );
        return scope.failed(failure);
    }
    let lock = scope.commands.command(&format!(
        "LOCK TABLE {relation} IN SHARE ROW EXCLUSIVE MODE"
    ));
    if !scope.commands.successful(lock) {
        return scope.failed(lock);
    }
    scope.commands.clear(lock);
    // Separate LOCK and execution commands are essential: after a concurrent
    // writer commits, READ COMMITTED takes a fresh snapshot for the CTE.
    let result = execute();
    if !scope.commands.successful(result) {
        return scope.failed(result);
    }
    let finish = match &scope.savepoint {
        Some(name) => format!("RELEASE SAVEPOINT {name}"),
        None => "COMMIT".to_owned(),
    };
    let finished = scope.commands.command(&finish);
    if !scope.commands.successful(finished) {
        scope.commands.clear(result);
        return scope.failed(finished);
    }
    scope.commands.clear(finished);
    scope.active = false;
    result
}

pub(crate) unsafe fn is_replacement(sql: *const c_char) -> bool {
    !sql.is_null()
        && crate::upsert::replacement_lock_relation(&CStr::from_ptr(sql).to_string_lossy())
            .is_some()
}

/// Execute on the same PG session while the caller already owns its mutex.
/// The successful query result is published only after COMMIT/RELEASE succeeds.
pub(crate) unsafe fn execute_locked(
    connection: *mut PgConnection,
    sql: *const c_char,
    execute: impl FnOnce() -> *mut PGresult,
) -> *mut PGresult {
    let relation = if sql.is_null() {
        None
    } else {
        crate::upsert::replacement_lock_relation(&CStr::from_ptr(sql).to_string_lossy())
    };
    match relation {
        Some(relation) => execute_scoped(&mut Libpq((*connection).conn), &relation, execute),
        None => execute(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct State {
        status: i32,
        log: Vec<String>,
        fail: Option<&'static str>,
        repeatable_read: bool,
        prior_work: bool,
        concurrent_version: u32,
    }
    struct Fake(Rc<RefCell<State>>);
    impl Commands for Fake {
        type Result = u8;
        fn transaction_status(&self) -> i32 {
            self.0.borrow().status
        }
        fn command(&mut self, sql: &str) -> u8 {
            let mut state = self.0.borrow_mut();
            state.log.push(sql.to_owned());
            if state.fail.is_some_and(|fail| sql.starts_with(fail)) || sql.starts_with("DO ") {
                state.status = 3;
                return 2;
            }
            if sql == "BEGIN" {
                state.status = 2;
            }
            if sql == "COMMIT" || sql == "ROLLBACK" {
                state.status = 0;
                if sql == "ROLLBACK" {
                    state.prior_work = false;
                }
            }
            if sql.starts_with("ROLLBACK TO ") {
                state.status = 2;
            }
            if sql.starts_with("LOCK TABLE ") {
                // Model a prior writer completing while LOCK waits. Query must
                // be a subsequent command, otherwise it keeps the old snapshot.
                state.concurrent_version = 2;
            }
            if sql == "SHOW transaction_isolation" {
                return 3;
            }
            1
        }
        fn successful(&self, result: u8) -> bool {
            result != 2
        }
        fn read_committed(&self, _: u8) -> bool {
            !self.0.borrow().repeatable_read
        }
        fn clear(&mut self, _: u8) {}
    }
    fn query(state: &Rc<RefCell<State>>, fail: bool) -> u8 {
        let mut state = state.borrow_mut();
        state.log.push("QUERY".to_owned());
        if fail {
            state.status = 3;
            2
        } else {
            1
        }
    }
    #[test]
    fn idle_scope_locks_before_fresh_query_then_commits() {
        let state = Rc::new(RefCell::new(State::default()));
        let result = execute_scoped(&mut Fake(state.clone()), "\"tags\"", || {
            assert_eq!(state.borrow().concurrent_version, 2);
            query(&state, false)
        });
        assert_eq!(result, 1);
        assert_eq!(
            state.borrow().log,
            [
                "BEGIN",
                "SHOW transaction_isolation",
                "LOCK TABLE \"tags\" IN SHARE ROW EXCLUSIVE MODE",
                "QUERY",
                "COMMIT"
            ]
        );
        assert_eq!(state.borrow().status, 0);
    }
    #[test]
    fn failing_caller_write_rolls_back_only_savepoint_preserving_prior_work() {
        let state = Rc::new(RefCell::new(State {
            status: 2,
            prior_work: true,
            ..State::default()
        }));
        assert_eq!(
            execute_scoped(&mut Fake(state.clone()), "\"tags\"", || query(&state, true)),
            2
        );
        let state = state.borrow();
        let name = state.log[0].strip_prefix("SAVEPOINT ").unwrap();
        assert_eq!(state.log[4], format!("ROLLBACK TO SAVEPOINT {name}"));
        assert_eq!(state.log[5], format!("RELEASE SAVEPOINT {name}"));
        assert!(state.prior_work);
        assert_eq!(state.status, 2);
    }
    #[test]
    fn successful_caller_write_releases_without_committing_caller_work() {
        let state = Rc::new(RefCell::new(State {
            status: 2,
            prior_work: true,
            ..State::default()
        }));
        assert_eq!(
            execute_scoped(&mut Fake(state.clone()), "\"tags\"", || query(
                &state, false
            )),
            1
        );
        let state = state.borrow();
        assert!(state.log.last().unwrap().starts_with("RELEASE SAVEPOINT "));
        assert!(!state.log.iter().any(|sql| sql == "COMMIT"));
        assert_eq!(state.status, 2);
    }
    #[test]
    fn failed_caller_transaction_is_not_recovered() {
        let state = Rc::new(RefCell::new(State {
            status: 3,
            prior_work: true,
            ..State::default()
        }));
        assert_eq!(
            execute_scoped(&mut Fake(state.clone()), "\"tags\"", || query(&state, true)),
            2
        );
        assert_eq!(state.borrow().log, ["QUERY"]);
        assert!(state.borrow().prior_work);
        assert_eq!(state.borrow().status, 3);
    }
    #[test]
    fn lock_failure_prevents_execution_and_cleans_owned_transaction() {
        let state = Rc::new(RefCell::new(State {
            fail: Some("LOCK TABLE"),
            ..State::default()
        }));
        assert_eq!(
            execute_scoped(&mut Fake(state.clone()), "\"tags\"", || panic!(
                "query must not run"
            )),
            2
        );
        assert_eq!(state.borrow().log.last().unwrap(), "ROLLBACK");
        assert_eq!(state.borrow().status, 0);
    }
    #[test]
    fn commit_failure_is_returned_and_rolls_back() {
        let state = Rc::new(RefCell::new(State {
            fail: Some("COMMIT"),
            ..State::default()
        }));
        assert_eq!(
            execute_scoped(&mut Fake(state.clone()), "\"tags\"", || query(
                &state, false
            )),
            2
        );
        assert_eq!(state.borrow().log.last().unwrap(), "ROLLBACK");
        assert_eq!(state.borrow().status, 0);
    }
    #[test]
    fn cleanup_failure_is_not_reported_as_success_or_retried_forever() {
        let state = Rc::new(RefCell::new(State {
            fail: Some("ROLLBACK"),
            ..State::default()
        }));
        assert_eq!(
            execute_scoped(&mut Fake(state.clone()), "\"tags\"", || query(&state, true)),
            2
        );
        assert_eq!(
            state
                .borrow()
                .log
                .iter()
                .filter(|sql| *sql == "ROLLBACK")
                .count(),
            1
        );
        assert_eq!(state.borrow().status, 3);
    }
    #[test]
    fn unsupported_isolation_fails_before_lock_or_mutation() {
        let state = Rc::new(RefCell::new(State {
            status: 2,
            repeatable_read: true,
            prior_work: true,
            ..State::default()
        }));
        assert_eq!(
            execute_scoped(&mut Fake(state.clone()), "\"tags\"", || panic!(
                "query must not run"
            )),
            2
        );
        let state = state.borrow();
        assert!(!state.log.iter().any(|sql| sql.starts_with("LOCK TABLE")));
        assert!(state.prior_work);
        assert_eq!(state.status, 2);
    }
    #[test]
    fn unwind_rolls_back_owned_scope_before_mutex_release() {
        let state = Rc::new(RefCell::new(State::default()));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            execute_scoped(&mut Fake(state.clone()), "\"tags\"", || {
                panic!("write unwind")
            });
        }));
        assert!(result.is_err());
        assert_eq!(state.borrow().log.last().unwrap(), "ROLLBACK");
        assert_eq!(state.borrow().status, 0);
    }
}
