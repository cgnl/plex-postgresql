# Shim Maintenance Checklist

Use this checklist when adding or changing step/interpose helpers.

## Safety rules

1. **No `unsafe` in business logic.** All `unsafe` must be at the FFI boundary (`#[no_mangle] pub extern "C" fn`). Internal functions take `&mut PgStmt` / `&mut PgConnection`, not raw pointers.
2. **No logging inside mutex scope.** Use `log_debug_lazy!` / `log_info_lazy!` outside the guard to avoid ABBA deadlock with the LOGGER mutex.
3. **No nested mutex acquisition.** Lock ordering: `stmt.mutex` (Rust Mutex) before `conn.mutex` (pthread). Never hold conn mutex while acquiring stmt mutex.
4. **Vec-based PgStmt arrays.** Call `ensure_param_capacity()` at prepare time and `ensure_column_capacity()` at first step. Never index without ensuring capacity first.
5. **No `Box::new([T; large])`.** Use `vec![T; n].into_boxed_slice()` for allocations >1KB to avoid stack overflow on Plex's 544K worker threads.

## Code conventions

6. Keep `db_interpose_step.rs` orchestration-only where possible.
7. Use `step_result_t` for helper return values (`ROW/DONE/ERROR/FALLBACK`).
8. If helper can fail at connection level, expose `conn_error_out` and set it explicitly.
9. Be explicit about state side effects (`write_executed`, `read_done`, cached stmt updates).
10. Centralize libpq cancel/drain behavior via `rust_step_conn_cancel_and_drain` (fast path skips PQcancel when not busy).
11. Prefer helper modules (`step_read_utils/`, `step_write_utils/`, `step_cached_read_utils`) over inline logic.
12. Keep logging taxonomy consistent (`READ`, `WRITE`, `CACHED_READ`, `CACHED_WRITE`, `LIFECYCLE`).
13. Update `docs/step-flow.md` when module boundaries or contracts change.

## Performance rules

14. Use `log_debug_lazy!` / `log_info_lazy!` instead of `log_debug(&format!(...))` — the macro checks LOG_LEVEL before `format!()`.
15. Transaction control must run on the same PostgreSQL session as every statement belonging to that SQLite handle. Never acknowledge `BEGIN`/`COMMIT`/`ROLLBACK`/savepoints as successful no-ops. Per-statement implicit transactions do not preserve rollback or atomicity. Pool cleanup must roll back abandoned work, never commit it. Verify changes through the actual shared shim and PostgreSQL, not a pool model.
16. `validate_type_consistency` only runs at DEBUG log level. Do not add unconditional validation in column accessors.
