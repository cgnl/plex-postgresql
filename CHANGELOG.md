# Changelog

All notable changes to plex-postgresql will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).


## [Unreleased]

## [1.3.12] - 2026-08-10

### Changed
- Updated upstream base Docker images (linuxserver/plex:latest / plexinc/pms-docker:latest).

## [1.3.11] - 2026-08-04

### Changed
- Updated upstream base Docker images (linuxserver/plex:latest / plexinc/pms-docker:latest).

## [1.3.10] - 2026-08-03

### Changed
- Updated upstream base Docker images (linuxserver/plex:latest / plexinc/pms-docker:latest).

## [1.3.9] - 2026-07-27

### Changed
- Updated upstream base Docker images (linuxserver/plex:latest / plexinc/pms-docker:latest).

## [1.3.8] - 2026-07-15

### Changed
- Updated upstream base Docker images (linuxserver/plex:latest / plexinc/pms-docker:latest).

## [1.3.7] - 2026-07-14

### Changed
- Updated upstream base Docker images (linuxserver/plex:latest / plexinc/pms-docker:latest).

## [1.3.6] - 2026-07-13

### Changed
- Updated upstream base Docker images (linuxserver/plex:latest / plexinc/pms-docker:latest).

## [1.3.5] - 2026-07-06

### Changed
- Updated upstream base Docker images (linuxserver/plex:latest / plexinc/pms-docker:latest).

## [1.3.4] - 2026-06-29

### Changed
- Updated upstream base Docker images (linuxserver/plex:latest / plexinc/pms-docker:latest).

## [1.3.3] - 2026-06-22

### Changed
- Updated upstream base Docker images (linuxserver/plex:latest / plexinc/pms-docker:latest).

## [1.3.2] - 2026-06-22

### Changed
- Updated upstream base Docker images (linuxserver/plex:latest / plexinc/pms-docker:latest).

### Added
- Automated check for upstream Docker base image updates (`linuxserver/plex:latest` and `plexinc/pms-docker:latest`) that triggers automatic patch releases and builds.

### Fixed
- Derive `Flags.dat` path from `$SQLITE_DB` for non-`/config` mount layouts during data migration.

## [1.3.1] - 2026-04-01

### Fixed
- Prevent `BindAddrInUseException` crash loop in Docker via a `subreaper` wrapper.

## [1.3.0] - 2026-04-01

### Changed
- Added German, Esperanto, Irish, and Dutch README documentations with flag emoji links.

### Fixed
- Resolved `LD_PRELOAD` deadlock, DDL routing, and SQLite passthrough.
- Fixed `bad_cast` issues by handling `dt_integer(8)` mapping to `BIGINT` decltype.
- Corrected column type/decltype SQLite semantics per the SOCI database connector contract.
- Added automatic clearing of `Flags.dat` after data migration to prevent UUID-related crashes.
- Added `pg_trgm` extension checks and stderr logging to all migration paths.
- Allowed wildcard expansion in `GROUP BY` clause for joined tables.
- Mapped `TIMESTAMP`/`TIMESTAMPTZ` OIDs to `SQLITE_INTEGER`.
- Fixed SQL translator backtick parsing in `ON CONFLICT` and FTS column names.
- Fixed various compiler and linter warnings (Clippy `let_and_return`, `dead_code`).

## [1.2.0] - 2026-03-28

### Changed
- **92% unsafe reduction** — raw pointer dereferences 806→59. Internal functions use safe `&mut PgStmt` / `&mut PgConnection` references. Business logic is now Rust-safe; unsafe confined to FFI boundary.
- **PgStmt Vec-based allocation** — inline `[T; 1024]` arrays replaced with `Vec<T>` that grow on-demand. 0-param SELECT uses 540 bytes (was 88KB). Allocated via `Box::new()`, freed via Drop.
- **Rust Mutex on PgStmt** — `pthread_mutex_t` replaced with `std::sync::Mutex<()>`. Auto-initialized, no manual pthread calls.
- **138 functions de-FFI'd** — removed `#[no_mangle] extern "C"` from Rust-only functions, enabling inlining and eliminating C ABI overhead.
- **Safe orig_sqlite3 accessors** — 55 static mut function pointer reads encapsulated in safe accessor functions via macro-generated wrappers.

### Fixed
- **Memory leak (1.8GB→59MB)** — PGresult leak in re-execution path, transaction control routing mismatch (BEGIN/COMMIT sent to PG instead of skipped), cached_result ref_count not released on stmt free.
- **3 deadlocks eliminated** — connection mutex self-deadlock (PTHREAD_MUTEX_RECURSIVE), ABBA deadlock (stmt→LOGGER mutex), connection mutex convoy (reduced lock hold time).
- **Stack overflow** — 512KB thread-local buffers heap-allocated via `vec![].into_boxed_slice()` for Plex's 544K worker threads.
- **11 data races fixed** — atomic counters/flags, seqlock on crash buffers, OnceLock for lazy config, eager hook resolution, AtomicPtr for fishhook linked list.
- **All clippy warnings resolved** — zero warnings on Linux CI with `-D warnings`.
## [1.1.0] - 2026-03-26

### Changed
- **Full Rust shim runtime** — all interpose/runtime C code eliminated. The shim is now 100% Rust (compiled to a single `.dylib`/`.so` via static linking). No C object files remain. Rust's ownership model, borrow checker, and type system prevent entire classes of bugs that plagued the C implementation: use-after-free, buffer overflows, double-free, null pointer dereference, and data races are caught at compile time rather than discovered in production.
- **Module split** — large monolithic Rust files split into focused submodules: `db_interpose_column/`, `db_interpose_common/`, `db_interpose_step/`, `db_interpose_prepare/`, `pg_client/`, `pg_statement/`, and 15+ other module directories.
- **Thread-local text buffers heap-allocated** — fixes stack overflow on Plex worker threads (544K stacks) by using `Box::new()` for the 512KB column text buffer pool.

### Fixed
- **Connection mutex self-deadlock** — `PgConnection.mutex` changed from `PTHREAD_MUTEX_DEFAULT` to `PTHREAD_MUTEX_RECURSIVE`, matching `PgStmt.mutex`. Fixes self-deadlock when `ensure_pg_result_for_metadata` calls `resolve_column_tables_impl` on the same connection.
- **Connection mutex convoy** — `ensure_pg_result_for_metadata` now releases the connection mutex before calling `resolve_column_tables`, reducing lock hold time and preventing thread convoy effects under concurrent metadata access.
- **ABBA deadlock (stmt mutex / LOGGER mutex)** — logging calls removed from inside stmt/conn mutex scopes across all column accessor modules. Logger uses `try_lock` with stderr fallback when contended.
- **Double-lock on stmt mutex** — `validate_type_consistency` moved outside the stmt mutex scope in scalar accessors. `ensure_pg_result_for_metadata` and `allocate_fake_sqlite_value` moved outside stmt mutex in metadata/value accessors.
- **rusqlite 0.32 compatibility** — test files updated to use FFI `sqlite3_prepare_v2` instead of removed `Statement::as_raw()` method.

## [1.0.0] - 2026-02-22

### Changed
- **SQL translator fully migrated to Rust (Phase 1)** — the entire SQLite-to-PostgreSQL SQL translation pipeline now runs on Rust's `sqlparser-rs` AST engine. The C translator (`sql_tr_*.c`, `sql_translator.c`) has been removed. 525 Rust tests cover all translation paths (318 lib + 54 batch1 + 51 batch2 + 42 batch3 + 60 batch4).
- **PG modules migrated to hybrid C/Rust (Phase 2)** — all 7 backend modules now have their core logic in Rust with thin C shims: `pg_config`, `pg_logging`, `pg_mem_telemetry`, `shim_alloc`, `pg_query_cache`, `pg_statement`, `pg_client`. ~550 C tests across 25 suites continue to pass.
- **`transform_expr` refactored to `&mut Expr`** — functions.rs, keywords.rs, and query.rs now use in-place mutation instead of by-value transform, reducing unnecessary AST cloning.
- **Log level demotion** — 5 informational `LOG_ERROR` messages demoted to `LOG_INFO` (pool init, fresh connection succeeded). These are filtered at the default `PLEX_PG_LOG_LEVEL=ERROR` setting, reducing log noise in production.
- **Docker and CI builds updated for Rust** — Dockerfiles, build_shim_musl.sh, and GitHub Actions workflows now install the Rust toolchain and build the sql-translator staticlib.

### Removed
- **C SQL translator** — `sql_tr_*.c`, `sql_translator.c`, `sql_translator_internal.h` and all `SQL_TR_OBJS` from Makefile. The Rust translator is now the sole implementation.

### Fixed
- **Git repository size** — removed accidentally committed `rust/sql-translator/target/` from all history via `git-filter-repo`. Repository size reduced from 243MB to 60MB.

## [0.9.40] - 2026-02-22

### Fixed
- **Duplicate prepared statement handling (SQLSTATE 42P05)** — after a Plex restart (while PG keeps running), the shim's empty cache would try to re-prepare statements that already exist on the PG backend. Now detects SQLSTATE `42P05` via `pg_is_duplicate_prepared_stmt()` (locale-independent), replaces fragile `strstr(err, "already exists")` checks. Fixed missing 42P05 handling in `db_interpose_exec.c` and `db_interpose_column.c` (METADATA_DESCRIBE path).
- **DEALLOCATE ALL at connection init** — new connections now run `DEALLOCATE ALL` to clean up orphaned prepared statements from previous shim instances. Eliminates PG log spam.
- **METADATA_DESCRIBE cache lookup** — the column metadata path now checks the local stmt cache before calling `PQprepare`, avoiding unnecessary round-trips and PG-side errors.

### Added
- **Prepared statement cache unit tests** — 21 new tests covering hash function, cache add/lookup/clear, SQLSTATE 42P05/26000 detection, edge cases, and cache eviction. Total: 299 tests.

## [0.9.39] - 2026-02-22

### Fixed
- **Stale prepared statement recovery after PG restart** — after a PostgreSQL restart, the shim's prepared statement cache could reference statements that no longer exist on the server. Now detects SQLSTATE `26000` (`invalid_sql_statement_name`), clears the local cache (no DEALLOCATE round-trips), and lets the retry wrapper re-prepare and re-execute. Covers all 6 error handlers in step and exec paths.

## [0.9.38] - 2026-02-22

### Fixed
- **`sqlite3_exec` retry + reconnect (Issue #8)** — `sqlite3_exec` now has the same pre-flight connection health check and retry wrapper that `sqlite3_step` already had. Dead connections are detected immediately via `PQstatus`, reconnected inline, and retried with `PLEX_PG_RETRY_DELAYS` backoff. Previously, `sqlite3_exec` would silently swallow connection errors.

## [0.9.37] - 2026-02-21

### Changed
- **Docs: add `PLEX_PG_IDLE_TIMEOUT` everywhere** — added idle timeout env var to Dockerfile.standalone, INSTALL.md (all platform sections), and Linux install wrapper script.

## [0.9.36] - 2026-02-21

### Added
- **Pool auto-grow (Issue #9)** — Connection pool now automatically grows from configured size up to `POOL_SIZE_MAX` (200) when Plex creates more threads than available slots. Prevents permanent thread lockout when `PLEX_PG_POOL_SIZE` is set below peak thread count. The existing reaper closes idle connections when demand drops.
- **`PLEX_PG_IDLE_TIMEOUT` environment variable** — configures how long idle connections are kept before the reaper closes them (default: 300s, minimum: 10s).
- **Stress test suite** — `test_pool_autogrow`, `test_stress_load`, `test_pool_exhaustion`, `test_pool_modes` for pool behavior validation under load.
- **Makefile targets** — `test-stress` and `test-pool-exhaustion` for running pool stress tests.

### Changed
- `configured_pool_size` is now atomic (`_Atomic int`) for lock-free auto-grow via CAS.

## [0.9.35] - 2026-02-21

### Fixed
- **Docker standalone: chown -R delay on large libraries** — removed two `chown -R plex:plex` calls from `standalone-entrypoint.sh` that caused massive delays on multi-TB libraries. Plex handles ownership itself via `40-plex-first-run`. Also fixes triple-chown issue when `PLEX_UID`/`PLEX_GID` are set. (PR #7 by @sjdaws)

## [0.9.34] - 2026-02-21

### Added
- **`PLEX_PG_RETRY_DELAYS` environment variable** — configures the retry backoff schedule for both pool-level and step-level PG reconnection. Comma-separated list of delays in milliseconds (default: `500,1000,2000,3000,4000`). Example: `PLEX_PG_RETRY_DELAYS=200,500,1000` for faster recovery on a local PG.

### Fixed
- **Plex doesn't recover after PostgreSQL restart (Issue #8)** — Two-layer fix ensures zero endpoint failures after PG restarts:
  1. **Pool-level retry** (`pg_client.c`): `pool_get_connection()` Phase 5 retries with exponential backoff (500ms→4s, 5 retries, ~10.5s total) instead of returning NULL when all slots are unavailable. This covers threads needing new connections.
  2. **Step-level retry** (`db_interpose_step.c`): New `my_sqlite3_step()` wrapper catches `SQLITE_ERROR` from any PG connection failure (flagged via thread-local `step_pg_conn_error`), resets statement state with `pg_stmt_clear_result()`, waits with exponential backoff, and retries. This covers threads whose existing connections died mid-query (PQsend failures, CONNECTION_BAD). All existing mutex/lock patterns are preserved — no deadlock risk.
- **Blobs UNIQUE index** — `blobs` table was missing `UNIQUE INDEX idx_blobs_linked_type_id_blob_type ON blobs(linked_type, linked_id, blob_type)`, causing ON CONFLICT upsert errors. Added to schema and doctor.sh.

## [0.9.33] - 2026-02-18

### Fixed
- **Docker fresh-install crash** — `blobs.db` was excluded from the PG dummy-shadow path by `!is_blobs_db_path()`, causing a "no such table: schema_migrations" crash on first start. Removed the exclusion so blobs.db uses the PG dummy shadow like all other databases.
- **Docker blobs schema_migrations rewrite** — `rewrite_blobs_schema_migrations()` renamed the table to `blobs_schema_migrations` which doesn't exist in PG. Disabled with `#if 0` since both databases share the same `schema_migrations` table.
- **Docker claim flow crash (linuxserver)** — `init-plex-claim` started Plex temporarily without `LD_PRELOAD`, causing immediate crash. Fixed by patching the claim script at build time to include the shim.
- **Docker migration: generated columns** — PG `subtype` on `metadata_items` is a generated column; COPY fails. Fixed: filter `is_generated = 'ALWAYS'` columns from COPY target list.
- **Docker migration: check constraints** — `chk_not_orphan` blocked COPY for orphaned records. Fixed: drop constraints before COPY, restore with NOT VALID after.
- **Docker migration: script path** — `$(dirname "$0")` resolved wrong in standalone container. Fixed: `SHIM_DIR` fallback.
- **Docker: missing python3 + migrate_table.py** in both Docker images. Added to Dockerfile and Dockerfile.standalone.
- **Docker: missing source files in Dockerfile.standalone** — gcc command was out of sync. Synced with Dockerfile.
- **SQL: GROUP BY / NULLS FIRST corrupted non-SELECT** — `add_nulls_first_ordering()` and `fix_group_by_strict_complete()` added ORDER BY/GROUP BY rewrites to DELETE/UPDATE statements. Both now skip non-SELECT statements.
- **`streaming_active` race condition** — Changed from `volatile int` to `_Atomic int` for correct cross-thread visibility.

### Added
- **Docker claim detection** — `check_plex_claim()` in docker-entrypoint.sh warns if PLEX_CLAIM is set but server is already claimed.
- **PLEX_CLAIM example** in docker-compose.yml.
- **Diagnostic logging** for real SQLite prepare failures (helps debug shadow issues).
- **17 connection isolation tests** updated for `_Atomic int` semantics.
- Total: 278 tests (220 SQL + 41 shadow elimination + 17 connection isolation).

### Changed
- Log level fixes: "Result from different connection" → DEBUG, "finalize: BUG" → INFO, "LOOP DETECTED" → INFO. Removed PREPARE INSERT debug dump.

## [0.9.32] - 2026-02-17

### Fixed
- **Aggregate decltype returns NULL** — `sqlite3_column_decltype()` for aggregate expressions (`count(*)`, `sum()`, `min()`, `max()`, `avg()`) now returns NULL instead of `"INTEGER"`, matching real SQLite behavior. This fixes `std::bad_cast` crashes in SyncCollections (`MetadataCollection.cpp:522`), "Saving activity history aborted", and "ViewStateSync exception".
- **camelCase alias quoting** — PostgreSQL lowercases unquoted identifiers; aliases like `blankKeyTaggingId`, `nonblankKeyId`, `grandparentsSettings` are now auto-quoted. Two-pass approach excludes SQL type keywords (INTEGER, TEXT, etc.) in CAST expressions.
- **Logging fd leak after fork** — child processes forked by Plex no longer close the parent's log file descriptor on exit.
- **Log noise** — "STREAM: zero rows returned", "RESOLVE_TABLES: alternate connection", "drained N results after cancel" downgraded from ERROR to DEBUG.

### Added
- **Dummy prepare with named parameters** — PG-routed queries build a dummy SQLite statement using actual named parameters from the SQL translator, removing more shadow SQLite dependency.
- **PQdescribePrepared for column metadata** — `column_count()` and `column_name()` use PostgreSQL's describe protocol instead of shadow SQLite.
- **Decltype cache from PG catalog** — column types resolved from `information_schema.columns` cache.
- **Transaction commit guards** in connection pool release.
- **blobs.db schema_migrations routing** to PostgreSQL.
- 11 new camelCase alias quoting tests. Total: 261 tests (220 SQL + 41 shadow elimination).

## [0.9.31] - 2026-02-16

### Fixed
- **Docker build: missing source files** — `Dockerfile` and `build_shim_musl.sh` were missing `db_interpose_value.c`, `db_interpose_common.c`, `platform_backtrace.c`, `pg_mem_telemetry.c`, and `shim_alloc.c`, causing symbol lookup errors at runtime in Docker containers.
- **Windows Docker: CRLF line endings** — added `.gitattributes` to enforce LF line endings for shell scripts, Dockerfiles, and source files. Windows git converts LF→CRLF by default, breaking shebangs inside containers (fixes #6).

## [0.9.30] - 2026-02-16

### Added
- **Shim memory tracker** — opt-in allocation tracking via `PLEX_PG_ALLOC_TRACK=1` (summary every 60s) and `PLEX_PG_ALLOC_TRACE=1` (top 15 unfreed allocation sites with file:line). Zero overhead when disabled.

## [0.9.29] - 2026-02-16

### Fixed
- **Connection isolation during streaming** — `resolve_column_tables()` and `preload_decltype_cache()` called `PQexec()` on the streaming connection, consuming pending results. Next `PQgetResult` returned NULL, making Plex think no migrations had run, triggering full re-migration and `std::bad_cast` crash. Both functions now acquire an alternate pool connection when the passed connection has `streaming_active=1`.
- **Pool connection acquisition skips streaming connections** — fast path (TLS cached slot) and PHASE 1 loop both check `streaming_active` flag before returning a connection.
- **`PQcancel` before drain loops** — 6 drain loops (4 in `db_interpose_step.c`, 2 in `pg_statement.c`) now cancel the server-side query before draining, preventing hangs on large result sets.

### Added
- **`streaming_active` flag** on `pg_connection_t` — `volatile int` set when streaming starts, cleared on all completion/error/reset/finalize paths.
- **Dummy shadow statement fallback** — when shadow SQLite prepare fails for READ queries, builds a dummy `SELECT 1 WHERE ? IS NOT NULL AND ...` with matching parameter count so `sqlite3_bind_*` calls succeed and the query runs purely on PostgreSQL.
- **DDL `IF NOT EXISTS` injection** — shadow SQLite `CREATE TABLE`/`CREATE INDEX` statements get `IF NOT EXISTS` added automatically.
- **17 connection isolation tests** (`test_connection_isolation.c`) — streaming_active lifecycle, pool isolation, resolve/decltype guards, regression, multi-threaded.
- **20 shadow fallback tests** (`test_shadow_fallback.c`) — parameter counting, dummy generation, edge cases, end-to-end.
- Total: 798 tests across 24 suites.

## [0.9.28] - 2026-02-15

### Added
- **Single-row streaming mode** — READ queries use `PQsetSingleRowMode` for row-by-row streaming instead of fetching entire result sets into memory. Reduces memory pressure for large queries.

### Fixed
- **SQL translation bugs** — placeholder counting missed parameters after string literals; upsert translation failed when column list was absent; string quoting edge cases.

## [0.9.27] - 2026-02-14

### Fixed
- **Removed SyncCollections COMPAT skips** — the blank-key cleanup and tag aggregation queries were being intercepted and replaced with empty results to avoid `std::bad_cast` crashes. The root causes (`dt_integer(8)` decltype mapping and column alias fixes) were already resolved in v0.9.23, making these skips unnecessary. Removing them restores collection data and eliminates all 223 "Failed to generate a query" LPE errors at startup.

## [0.9.26] - 2026-02-13

### Fixed
- **JSON `->>` operator rewritten correctly** — `col ->> '$.key'` now translates to `col::json->>'key'` (native PostgreSQL JSON extraction). Previous LIKE-based hack consumed bind parameters, causing "bind message supplies N parameters, but prepared statement requires M" errors on Plex v1.43.0.10492 voice-activity-detection queries.
- **`instr()` function now translated** — SQLite's `instr(haystack, needle)` is translated to PostgreSQL's `STRPOS(haystack, needle)`. Fixes "function instr(text, unknown) does not exist" error on Last.fm blacklist queries.
- **Migration CSV truncation bug** — the `sqlite3 -csv` export silently truncated TEXT fields larger than ~8KB containing embedded quotes. Replaced with Python bridge (`migrate_table.py`) using `COPY FROM STDIN` with tab-delimited data. Affected 133 rows in `media_parts.extra_data` on a typical library.
- **CI `leaks` tool on Linux** — `test-stmt-free` and `test-bind-mismatch` targets now skip macOS `leaks` tool on Linux instead of failing with "leaks: not found".

### Added
- **`scripts/migrate_table.py`** — Python bridge for lossless SQLite-to-PostgreSQL data transfer via COPY protocol.
- **Truncated JSON detection in `doctor.sh`** — checks `extra_data` columns across media_parts, media_items, metadata_items, metadata_item_settings for invalid JSON and auto-repairs by trimming the redundant `url` field.
- **Data integrity check in migration** — `migrate_lib.sh` verifies JSON integrity after migration completes.
- 3 new tests: `instr()` translation (2), real Plex VAD query with 3 bind params (1). Total: 738 tests.

### Changed
- All `TRACE_BADCAST` and `TRACE_PREPARE` diagnostic messages downgraded from `LOG_ERROR` to `LOG_DEBUG` — they are opt-in debug traces, not errors.

## [0.9.25] - 2026-02-12

### Added
- **Optional memory telemetry** — set `PLEX_PG_MEM_TELEMETRY=1` to log per-subsystem allocation stats every 60s. Tracks bind_text, bind_hex, bind_value_blob, column cached_blob, column decoded_blob, and statement sweep frees. Default off, zero overhead when disabled.
- Telemetry passed through `make run` via `PLEX_PG_MEM_TELEMETRY` env var.

### Confirmed
- 30-minute production measurement confirmed **shim allocates only 14KB total** under normal Plex load — memory growth is Plex-internal, not shim-caused.

## [0.9.24] - 2026-02-12

### Fixed
- **Statement cleanup leak window** in `pg_stmt_free` — captured bind values are now freed across all `MAX_PARAMS` slots, not only up to `param_count`.

### Added
- **`test_stmt_free_param_sweep`** regression test to verify full parameter slot cleanup at statement teardown.
- **`test_bind_index_mismatch_cleanup`** regression test to cover cleanup safety when bind index usage diverges from translated `param_count`.

### Changed
- Included both new regression tests in `unit-test` and `ci-test` targets.

## [0.9.23] - 2026-02-10

### Fixed
- **`LPE: only library URIs are allowed right now` errors on startup** (142-221 per startup)
  - Root cause: Plex's `extra_data` column stores JSON blobs containing `"pv:uri":"server://<machineId>/com.plexapp.plugins.library/library/..."` URIs. Plex's LPE parser only accepts `library://` scheme.
  - Solution: Rewrote `rewrite_server_library_uri()` to scan text values for embedded `server://` URIs and rewrite them to `library://` inline. Handles both standalone URIs and JSON-embedded URIs.
  - The data in PostgreSQL is correct (identical to original SQLite); the rewrite happens at read-time only.
- **Off-by-one in `needle_len` constant** — hardcoded 36 instead of actual 37. Found by new unit tests. Replaced all hardcoded string lengths with `sizeof() - 1`.
- **`/library/metadata/<id>` returning HTTP 500** with `std::bad_cast` — return `dt_integer(8)` for OID=20 timestamp columns.
- **`/library/metadata/<id>/related` returning HTTP 500** with `std::bad_cast` — return `SQLITE_NULL` for `metadata_type=18` (collection/folder) in related-items queries.
- **`DatabaseFixups/SyncCollections` throwing `std::bad_cast`** — skip two problematic query patterns with empty-result dummy statements.

### Added
- **13 unit tests** (`test_uri_rewrite.c`) — standalone URIs, JSON-embedded URIs, multiple URIs, edge cases (NULL, empty, small buffer, no match, partial match).
- Total: 777 tests across 24 suites.

### Changed
- Removed `LPE_URI_READ` diagnostic trace (was ERROR-level, spammed 884 lines per startup).
- Cleaned up dead `col_name_for_log` variable and redundant comments.

## [0.9.22] - 2026-02-09

### Changed
- **Extracted `db_interpose_value.c`** from `db_interpose_column.c` — all 7 `sqlite3_value_*` functions (type, text, int, int64, double, bytes, blob) now in separate module. Column.c: 2065 → 1769 lines.

## [0.9.21] - 2026-02-09

### Fixed
- **macOS `sqlite3_column_decltype` not intercepted** — was missing from fishhook rebindings, so Plex's SOCI type-mapping logic (`my_sqlite3_column_decltype`, 150+ lines) was never called on macOS.
- **Linux `sqlite3_column_decltype` wrapper bypassed SOCI type mapping** — routed to `orig_sqlite3_column_decltype` instead of `my_sqlite3_column_decltype`.
- **macOS fallback `load_sqlite_fallback()` only loaded ~11 of ~60 symbols** — replaced with shared `common_load_sqlite_symbols()` covering all functions.

### Changed
- **Extracted `common_load_sqlite_symbols()`** into `db_interpose_common.c` — single source of truth for all ~60 `dlsym` lookups, used by both macOS fallback and Linux `load_original_functions()`.
- **Extracted `platform_backtrace.c`** — unified backtrace module with `#ifdef __APPLE__` for platform-specific frame collection and symbol resolution. Removed ~300 lines of duplicated code.

### Added
- **66 new platform parity tests** (`test_platform_parity.c`) — symbol loading completeness, if-not-set pattern, idempotency, callable pointer verification, backtrace output format.
- Total: 764 tests across 23 suites (CI: 722 tests, 19 suites).

## [0.9.20] - 2026-02-09

### Added
- **GitHub Actions unit test pipeline**
  - New `unit-tests` job in `.github/workflows/ci.yml` runs 657 tests across 18 suites on every push and PR.
  - New `ci-test` Makefile target for CI-safe test subset (excludes LD_PRELOAD tests).
  - Fixes for Linux/GCC portability: `pthread_getattr_np` for stack tests, `stddef.h` for `ptrdiff_t`, graceful `__cxa_demangle` skip.

- **~160 new unit tests for SQL translator and upsert** (540 -> 698 total)
  - Upsert: 6 -> 59 tests covering all 28 conflict targets, schema prefix stripping, special column handling.
  - Case booleans: 2 -> 14 tests. Integer/text mismatch: 9 tests. DDL types: 12 tests. Keywords: 18 tests.
  - Forward reference joins: 5 tests. Null sorting: 6 tests. Plex pipeline: 3 tests.
  - `fix_group_by_strict_complete`: 15 direct tests. `add_nulls_first_ordering`: 4 tests.
  - typeof remapping, strftime, unixepoch, json_each, placeholder edge cases, operator spacing, COLLATE NOCASE.

### Fixed
- **sql_tr_upsert.c**: schema prefix not stripped before `metadata_item_settings` special case comparison.
- **sql_tr_query.c**: fast-path in `translate_case_booleans` missing `" 0)"` and `" 1)"` patterns.
- **sql_tr_types.c**: fast-path in `sql_translate_types` missing `" datetime"` check.

## [0.9.19] - 2026-02-09

### Fixed
- **Block junk metadata inserts** with both `library_section_id` and `metadata_type` NULL (orphan rows).
  - Added `chk_not_orphan` CHECK constraint to schema and `doctor.sh`.
- **schema_migrations conflict handling**: added `ON CONFLICT DO NOTHING` for INSERTs to prevent UNIQUE violation crash.
- **Placeholder translator**: only track single-quote strings, not double-quote identifiers.
- **Duplicate assignment dedup**: handle backtick-quoted columns, consume removed `$N` params with COALESCE.
- **NULLS FIRST ordering** added for GROUP BY queries (SOCI compatibility).
- Downgraded `COLUMN_TEXT_NO_STMT` from ERROR to DEBUG for non-PG databases.

## [0.9.18] - 2026-02-09

### Fixed
- **Auto-reconnect PostgreSQL after connection loss**
  - `step()` READ/WRITE retries once after 500ms if pool returns NULL connection.
  - If `PQreset` fails on `CONNECTION_BAD`, tries fresh `PQconnectdb` instead of giving up.
  - `pg_pool_check_connection_health` uses same fallback to fresh connection when reset fails.
  - Fixes HTTP 500 on all library endpoints after PostgreSQL restart or after Plex is killed with SIGKILL.

## [0.9.17] - 2026-02-09

### Changed
- **macOS: shim dylib installed into Plex.app bundle**
  - `install_wrappers.sh` copies dylib into `Plex.app/Contents/MacOS/` instead of referencing external paths.
  - Scanner uses `@loader_path` for `LC_LOAD_DYLIB` (portable, no absolute paths).
  - Server wrapper simplified (no more placeholder sed, no auto-build).
  - Uninstaller cleans up dylib from Plex.app.

## [0.9.16] - 2026-02-08

### Fixed
- **macOS wrapper portability and migration state correctness**
  - Removed hardcoded machine-specific paths from generated server wrapper.
  - Wrapper now uses install-time shim placeholders and user-home defaults.
  - SQLite shadow `schema_migrations` now syncs from PostgreSQL (instead of only inserting `pg_adapter_1.0.0`).

- **macOS scanner uninstall reliability**
  - Installer now keeps `Plex Media Scanner.original` backup before patching.
  - Uninstaller now restores scanner when backup exists and prints a clear warning when restore is impossible.
  - Prevents silent "uninstall succeeded" state while scanner stays patched.

## [0.9.15] - 2026-02-08

### Added
- **GitHub Actions Linux release pipeline**
  - Added `.github/workflows/release-linux-artifacts.yml` to build Linux release binaries on tag push.
  - Builds `db_interpose_pg-linux-x86_64.so` and `db_interpose_pg-linux-aarch64.so` and uploads them to the GitHub release.
  - Supports manual re-run via `workflow_dispatch` with a `tag` input.

### Fixed
- **CI tag checkout behavior for manual runs**
  - Workflow now checks out the requested tag ref instead of always building `main` during manual dispatch.

- **Architecture-specific PostgreSQL builder flags in Dockerfiles**
  - Made PostgreSQL build flags architecture-aware to improve release build stability in CI.
  - Files: `Dockerfile`, `Dockerfile.standalone`

## [0.9.14] - 2026-02-08

### Fixed
- **Linux: Plex crash loop with `Received unexpected async signal 17` (SIGCHLD)**
  - Added Linux `sigaction()` guard to keep `SIGCHLD` ignored in the main Plex Server/Scanner process.
  - Prevents child process exits (plugins/scanner helpers) from triggering the async signal crash path under `LD_PRELOAD`.
  - Files: `src/db_interpose_core_linux.c`

- **Linux child-process safety with `LD_PRELOAD` inheritance**
  - Added passthrough mode for non-target processes so plugin/helper processes keep original SQLite behavior.
  - Explicitly resolve and use original SQLite symbols in non-server/scanner processes.
  - File: `src/db_interpose_core_linux.c`, `src/pg_config.c`, `src/db_interpose.h`, `src/db_interpose_common.c`

- **Docker migration parity: prevent migration reruns from SQLite shadow DB**
  - Added `schema_migrations` sync from PostgreSQL to SQLite shadow databases during init.
  - Keeps SQLite and PostgreSQL migration state aligned, avoiding repeated migration attempts.
  - Files: `scripts/standalone-entrypoint.sh`, `scripts/docker-entrypoint.sh`

### Changed
- **Standalone compose defaults to production log level**
  - Switched `docker-compose.standalone.yml` to `PLEX_PG_LOG_LEVEL=ERROR` after stabilization.

- **Standalone Docker image hardening**
  - Updated `Dockerfile.standalone` to use build-time run-script injection and include schema/type metadata setup.
  - Replaced CrashUploader with a no-op binary in the standalone image to avoid unnecessary crash upload behavior in Docker.

## [0.9.13] - 2026-02-07

### Fixed
- **Makefile: `make install` on Linux tried to build `fishhook.o` (macOS-only)**
  - `$(OBJECTS)` always included `src/fishhook.o`, even on Linux where fishhook.c requires macOS headers (`mach/mach.h`)
  - `make install` depended on `$(TARGET)` which used the default `$(OBJECTS)` instead of `$(LINUX_OBJECTS)`
  - Solution: `$(OBJECTS)` is now platform-conditional — includes `fishhook.o` only on Darwin
  - Fixes [#5](https://github.com/cgnl/plex-postgresql/issues/5)

### Added
- **`Dockerfile.standalone` for `plexinc/pms-docker` users**
  - Multi-stage build: Alpine builder (musl) + plexinc/pms-docker runtime
  - Builds libpq from source without OpenSSL to avoid `ENGINE_*` symbol conflicts
  - Includes musl symlink setup and locale configuration
  - Closes [#5](https://github.com/cgnl/plex-postgresql/issues/5)

## [0.9.12] - 2026-02-06

### Fixed
- **CRITICAL: Excessive logging causing system freeze and kernel panic**
  - ROOT CAUSE: 18 debug/trace `LOG_ERROR` and `LOG_INFO` calls on hot paths fired on every database query
  - Even at `PLEX_PG_LOG_LEVEL=ERROR`, debug statements like `RACE_DEBUG`, `CACHED INSERT metadata_items`, `STEP metadata_items INSERT`, `play_queue_generators` params, `DEBUG_TRACE STEP_EXIT`, `PREPARED CHECK/PATH/STMT`, `EXEC_PREPARED`, `STEP_TRACE/DONE/ROW` were logged
  - Each log call: malloc(4KB) + mutex lock + unbuffered write syscall + mutex unlock + free
  - At thousands of queries/second this caused 34+ GB/day disk writes and severe mutex contention
  - Thread starvation led to 63 GB memory exhaustion, 29 swap files, WindowServer watchdog timeout, and kernel panic
  - Solution: Demoted all debug/trace statements to `LOG_DEBUG` so they are completely skipped at ERROR level (no malloc, no mutex, no syscall)
  - Files: `src/db_interpose_step.c`, `src/db_interpose_column.c`

## [0.9.11] - 2026-02-04

### Fixed
- **Stack overflow from circular parent_id references in metadata_items**
- **ORDER BY syntax error in GROUP BY query translation**

### Added
- Database triggers to prevent circular parent references
- Triggers to auto-fix orphan seasons on episode insert

## [0.9.10] - 2026-02-02

### Fixed
- **CRITICAL: Kernel panic caused by fflush(NULL) deadlock** - System crash prevention
  - ROOT CAUSE: `fflush(NULL)` in `db_interpose_step.c:653` flushed ALL stdio streams while holding log mutex
  - 14+ postgres processes blocked on `_fwalk → sflush_locked → flockfile`
  - Triggered WindowServer watchdog timeout (120s) and kernel panic
  - Solution: Removed `fflush(NULL)` call - logging already flushes per-line
  - File: `src/db_interpose_step.c`

- **CRITICAL: SOCI "Null value not allowed for this type" exceptions** - HTTP 500 errors
  - ROOT CAUSE: `column_type()` returned `SQLITE_NULL` for NULL column values
  - SOCI checks `column_type()` BEFORE calling `column_int()`, throws exception on NULL
  - Affected endpoints: `/library/all/top`, `/hubs/promoted`, `/library/metadata/*`
  - Specifically: MetadataCounterCache query with NULL `parent_id` values
  - Solution: Return declared column type (INTEGER, TEXT, etc) instead of `SQLITE_NULL`
  - `column_int()` already returns 0 for NULL (matching SQLite behavior)
  - File: `src/db_interpose_column.c` (both cached and non-cached paths)

### Technical Notes
- The NULL handling fix does NOT break existing behavior
- SQLite's `column_int()` returns 0 for NULL values - our shim does the same
- SOCI's strict type checking is the real issue, this is a workaround
- Similar to v0.8.12 aggregate function TEXT workaround

## [0.8.13] - 2026-01-13

### Changed
- Reduce shim log noise by downgrading high-frequency DECLTYPE/COLUMN_TYPE/COLUMN_TEXT_INTEGER diagnostics to DEBUG.

## [0.8.12] - 2026-01-13

### Fixed
- **CRITICAL: std::bad_cast exception in TV shows endpoint** - MetadataCounterCache rebuild crash
  - ROOT CAUSE: Plex's SOCI version has bug with BIGINT aggregate functions in row access
  - Sequence: count() returns BIGINT (OID 20) → declared as "BIGINT" → SOCI uses column_text() → parses as integer → row.get<int64_t>() → std::bad_cast
  - SOCI's SQLite backend in Plex uses column_text() for ALL integer types, not column_int64()
  - Type checking during text-to-int conversion fails for aggregate BIGINT values
  - Impact: TV shows endpoint returned HTTP 500, "Exception handled: std::bad_cast"
  - Solution: Force aggregate functions (count, sum, max, min, avg) to declare as TEXT type
  - SOCI accepts TEXT → integer conversion without strict type checking
  - Workaround in `column_decltype()` detects aggregate column names and returns "TEXT"
  - Files: `db_interpose_column.c` (line 1573), `pg_statement.c` (improved type mappings)
  - Related: SOCI Issue #1190 (identical bug, fixed in SOCI 4.1.0)
  
- **Improved PostgreSQL type mappings** - Correct BIGINT decltype declaration
  - INT2 (OID 21) → "INTEGER" (unchanged for SOCI compatibility)
  - INT4 (OID 23) → "INTEGER" (unchanged, correct)
  - INT8 (OID 20) → "BIGINT" (was "INTEGER", now correct)
  - File: `pg_statement.c` (pg_oid_to_sqlite_decltype function)
  - Note: SMALLINT not used due to SOCI compatibility issues

## [0.8.10] - 2026-01-12

### Fixed
- **CRITICAL: INSERT...RETURNING lastval() transaction boundary bug** - playQueues 500 errors (final fix)
  - ROOT CAUSE: `lastval()` only works within the same transaction, but libpq uses autocommit mode
  - Sequence: INSERT...RETURNING executes in transaction T1 → commits → lastval() queries in transaction T2 → fails
  - PostgreSQL closes transaction after each PQexec() in autocommit mode
  - `lastval()` error: "lastval is not yet defined in this session" or returns stale values
  - Solution: Capture RETURNING id immediately and store in `pg_connection_t->last_insert_rowid`
  - Modified `last_insert_rowid()` to return stored value instead of calling `lastval()`
  - Stores ID in all three paths: prepared statements, cached statements, and direct exec
  - Files: `db_interpose_step.c` (2 locations), `db_interpose_exec.c`, `db_interpose_metadata.c`

- **CRITICAL: Explicit transaction handling implementation** - Root cause fix for transaction data loss
  - ROOT CAUSE: BEGIN/COMMIT/ROLLBACK were skipped as no-ops, PostgreSQL never received them
  - Plex sends: BEGIN → INSERT → COMMIT, but shim executed: (skip) → INSERT → (skip)
  - PostgreSQL used implicit transaction mode, transactions never committed
  - Data appeared to succeed (lastval() worked) but was rolled back on connection reuse
  - Solution: Removed transaction commands from skip patterns, implemented explicit execution
  - Added `is_transaction_command()` to detect BEGIN/COMMIT/ROLLBACK
  - Execute transaction commands on PostgreSQL in `db_interpose_exec.c`
  - Track transaction state in `pg_connection_t.in_transaction` field
  - Files: `pg_config.c`, `pg_config.h`, `db_interpose_exec.c`, `db_interpose_prepare.c`

- **CRITICAL: Connection mismatch in lastval()** - Wrong sequence values returned from different connection
  - Root cause: INSERT uses `pg_get_thread_connection()` but `lastval()` used `pg_find_connection()`
  - Between INSERT and lastval(), pool state can change (thread steals slot, cache invalidated, etc.)
  - Result: INSERT on connection A succeeds, `lastval()` queries connection B, returns wrong ID
  - Solution: Use `pg_get_thread_connection()` for metadata functions on library.db
  - Modified `my_sqlite3_last_insert_rowid()`, `my_sqlite3_changes()`, `my_sqlite3_changes64()`
  - Guarantees same thread-local connection for INSERT and metadata retrieval

### Changed
- Transaction commands (BEGIN/COMMIT/ROLLBACK) now executed on PostgreSQL instead of skipped
- Transaction state tracking via `in_transaction` field in connection structure
- Metadata functions (`lastval()`, `changes()`) now use thread-local connection for library.db
- Ensures transaction consistency across all operations in a single thread

### Technical Notes
- v0.8.10 implements explicit transaction handling (ROOT CAUSE fix)
- Complements v0.8.9.6 (pool reuse) and v0.8.9.7 (connection mismatch) fixes
- All three fixes work together for complete transaction correctness

## [0.8.9.6] - 2026-01-12

### Fixed
- **CRITICAL: Uncommitted transactions lost on connection pool reuse** - playQueues 500 errors
  - Root cause: PQreset() in PHASE 2 of `pool_get_connection()` aborts uncommitted transactions
  - Sequence: Thread A INSERTs -> releases connection -> Thread B reuses -> PQreset() rolls back
  - lastval() succeeds (sequence persists) but actual INSERT data is rolled back
  - Result: 404 on subsequent GET requests despite successful lastval() return
  - Solution: Check PQtransactionStatus() and COMMIT before releasing connection (pg_close_pool_for_db)
  - Defense-in-depth: Also check and COMMIT before PQreset() in PHASE 2 reuse
  - PostgreSQL implicit transactions now properly committed before pool slot release

### Changed
- `pg_close_pool_for_db()` now commits pending transactions before marking slot as SLOT_FREE
- `pool_get_connection()` PHASE 2 now commits pending transactions before PQreset()

## [0.8.9.5] - 2026-01-12

### Fixed
- **Row index -1 out of bounds error** - libpq "row number -1 is out of range" error
  - Root cause: WRITE statements with RETURNING set `current_row = -1`
  - Column functions using fake values could access libpq with invalid row index
  - Added `row_idx >= 0` check to all fake value access points
  - Column functions now handle all PostgreSQL statements properly (not just those with results)

- **INSERT...RETURNING result storage causing issues**
  - Don't store RETURNING result for WRITE statements
  - SOCI uses `lastval()` via SQL translation, not the RETURNING columns
  - Prevents confusion from mixing WRITE and READ result handling

### Changed
- Column functions now use simpler `pg_stmt->is_pg` check instead of `is_pg == 2 || (is_pg == 1 && result)`
- This ensures proper fallback behavior for all PostgreSQL-intercepted statements

## [0.8.9.1] - 2026-01-12

### Fixed
- **Memory corruption when clearing metadata results** - Race condition in PQclear()
  - Root cause: v0.8.9's `clear_metadata_result_if_needed()` called `PQclear()` during bind operations
  - This caused race conditions when multiple threads accessed the same prepared statement result
  - Crash in libpq's `resetPQExpBuffer` with corrupted address `0x4d55545a00000000` (ASCII "MUTZ")
  - Solution: Don't call `PQclear()` in bind functions - set `metadata_only_result = 2` instead
  - Actual cleanup now handled safely in `step()` where proper locking is in place

### Changed
- `clear_metadata_result_if_needed()` now sets flag to 2 instead of calling PQclear()
- `step()` checks for `metadata_only_result == 2` to safely cleanup and re-execute

## [0.8.9] - 2026-01-11

### Fixed
- **Metadata-only results blocking step() re-execution** - "Step didn't return row" errors
  - Root cause: `ensure_pg_result_for_metadata()` executed queries BEFORE parameters were bound
  - This cached 0-row results, and `step()` saw the cached result instead of re-executing
  - Solution: Added `metadata_only_result` flag to track pre-step execution
  - Bind functions now clear this cached result via `clear_metadata_result_if_needed()`
  - `step()` properly re-executes with bound parameters

### Changed
- All 9 bind functions now call `clear_metadata_result_if_needed()` before binding
- `ensure_pg_result_for_metadata()` sets `metadata_only_result = 1` flag
- `step()` clears the flag after successful execution

## [0.8.8] - 2026-01-11

### Fixed
- **Bind functions not checking cached statement registry** - Race condition for cached statements
  - Root cause: `pg_find_stmt()` only checked primary registry, returning NULL for cached statements
  - This caused bind operations on cached statements to have no mutex protection
  - Solution: Use `pg_find_any_stmt()` which checks BOTH primary and cached registries
  - Applied to all 9 bind functions for consistent thread-safety

- **Auto-reset busy statements before binding**
  - Added `ensure_stmt_not_busy()` helper to auto-reset statements that are still in-use
  - Prevents SQLITE_MISUSE (21) "bind on busy prepared statement" errors
  - Called before every bind operation

## [0.8.7] - 2026-01-11

### Fixed
- **Deadlock when bind/reset trigger column functions** - std::exception crashes
  - Root cause: Non-recursive mutex caused deadlock when bind/reset internally triggered column operations
  - Solution: Use `PTHREAD_MUTEX_RECURSIVE` for statement mutex
  - Allows same thread to re-lock mutex without deadlock

## [0.8.6] - 2026-01-11

### Fixed
- **Thread-safety race condition in reset/clear_bindings** - Additional "bind on busy prepared statement" fix
  - Root cause: `sqlite3_reset()` and `sqlite3_clear_bindings()` released mutex BEFORE calling original SQLite
  - Solution: Hold mutex during entire `orig_sqlite3_reset()` and `orig_sqlite3_clear_bindings()` calls
  - Completes thread-safety fix started in v0.8.5

## [0.8.5] - 2026-01-11

### Fixed
- **Thread-safety race condition in bind operations** - "bind on busy prepared statement" errors
  - Root cause: Mutex was acquired AFTER calling SQLite, not before
  - Solution: Lock mutex BEFORE calling `orig_sqlite3_bind_*()` in all 9 bind functions
  - Prevents concurrent access when Thread A is stepping while Thread B is binding

- **lastval() error causing 500 on playQueues** - PostgreSQL error when no INSERT done yet
  - Root cause: `sqlite3_last_insert_rowid()` called `SELECT lastval()` which fails if no INSERT
  - Solution: Gracefully return 0 (like SQLite does) instead of propagating error

### Changed
- `make macos` now auto-cleans before building to prevent corrupt object files

## [0.8.1] - 2026-01-10

### Fixed
- **std::bad_cast exceptions** - SOCI ORM type conversion failures caused 500 errors
  - Root cause: `column_decltype()` returned NULL, causing SOCI type mismatch
  - Solution: Map PostgreSQL OIDs to SQLite-compatible type strings (INTEGER, REAL, TEXT, BLOB)
  - Types now match what `column_type()` returns, ensuring SOCI consistency

### Added
- **Robust C++ exception handler** (Linux only):
  - Per-exception-type tracking with stack traces for first occurrence of each type
  - Automatic source detection: "SHIM-RELATED" vs "external C++ code"
  - Library identification via `dladdr()` runtime linker
  - C++ symbol demangling via `__cxa_demangle`
  - Manual `/proc/self/maps` parsing (musl-compatible, no sscanf)
  - Throttling after 50 exceptions with type summary
- **musl build script** (`build_shim_musl.sh`) for Alpine/musl-based containers

### Changed
- Exception context tracking uses volatile globals instead of TLS (musl compatibility)
- Stack frame collection works on both ARM64 and x86_64

## [0.8.0] - 2026-01-10

### Added
- `sqlite3_column_decltype` interception for SOCI ORM compatibility
- `sqlite3_bind_parameter_index` for named parameter support
- Thread-local SQL translation cache with 512 entries per thread
- `ensure_pg_result_for_metadata()` for pre-step metadata access
- Comprehensive benchmark suite:
  - `tests/bench_cache.c` - Cache implementation comparison
  - `tests/bench_sqlite_vs_pg.py` - SQLite vs PostgreSQL latency
  - `tests/bench_translation.c` - SQL translation throughput
- Stack protection tests for macOS and Linux
- VERSION file for release tracking

### Changed
- SQL translation now uses lock-free thread-local cache (145x speedup)
- Updated README with detailed benchmark results
- Rewrote `docs/modules.md` with cache architecture documentation
- Reorganized debug documentation into `docs/debug/`

### Performance
- Cached SQL translation: 0.12 µs (was 17.5 µs uncached)
- Thread-local cache is 22x faster than mutex-protected cache
- Shim overhead is <1% of total query time
- Cache lookup: 22.6 ns per operation

### Fixed
- `sqlite3_column_value` now properly handles pre-step calls
- Column metadata functions work before `sqlite3_step()` is called

## [0.7.0] - 2026-01-08

### Added
- SQL normalization for parameterized query caching
- Prepared statement cache with O(1) hash table lookup
- Unix socket support for PostgreSQL connections
- `sqlite3_expanded_sql` implementation
- Boolean value conversions for PostgreSQL 't'/'f' values

### Fixed
- Double-free crash in connection cleanup
- Fork safety with pthread_atfork handlers

## [0.6.0] - 2026-01-06

### Added
- Stack overflow protection (multi-layer defense)
- Recursion guards with depth limiting
- OnDeck query special handling for low-stack conditions
- Loop detection for rapid repeated queries

### Fixed
- Stack overflow crash with 218 recursive frames
- Integer overflow in counter variables

## [0.5.0] - 2026-01-04

### Added
- COLLATE NOCASE translation to ILIKE/LOWER()
- FTS4 boolean search operators (AND, OR, NOT, phrases)
- Window functions support (ROW_NUMBER, RANK, DENSE_RANK)
- WHERE 0/1 to WHERE FALSE/TRUE translation

### Changed
- Improved GROUP BY expression rewriting

## [0.4.0] - 2026-01-02

### Added
- Connection pooling (50 connections default, max 100)
- Query result caching with TTL-based eviction
- Thread-local connection caching

### Fixed
- Connection exhaustion under heavy load

## [0.3.0] - 2025-12-30

### Added
- Full SQL translation pipeline
- Placeholder translation (? to $1, :name to $N)
- Function translations (iif, strftime, IFNULL, etc.)
- UPSERT translation (INSERT OR REPLACE to ON CONFLICT)

## [0.2.0] - 2025-12-28

### Added
- Linux support via LD_PRELOAD
- Docker support with docker-compose
- Schema auto-initialization

## [0.1.0] - 2025-12-25

### Added
- Initial release
- macOS support via DYLD_INTERPOSE + fishhook
- Basic SQLite to PostgreSQL interception
- Shadow database for SQLite-only queries
