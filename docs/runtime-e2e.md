# Rust shared-shim runtime E2E

Run `bash scripts/run-runtime-e2e.sh` from any directory. The script builds the
existing Makefile shared shim with `--features interpose`, builds the Rust
`runtime_e2e` binary, creates a disposable PostgreSQL cluster, and removes it on
exit. This is an executable integration runner, not an addition to the unit suite.
It needs Cargo, the existing native libpq/SQLite link prerequisites, and PostgreSQL
`initdb`, `pg_ctl`, and `psql`. On macOS, Homebrew PostgreSQL 15 is discovered;
override with `RUNTIME_E2E_PG_BIN=/path/to/postgresql/bin`.

## CLI contract

```sh
bash scripts/run-runtime-e2e.sh [--shim /absolute/path/to/db_interpose_pg.dylib] [--reconnect]
```

`--shim` tests an already-built artifact without rebuilding it (Linux uses `.so`).
`--reconnect` additionally terminates only this disposable database/user's shim
backends and requires read/write recovery. CI always requests reconnect. Every
missing prerequisite, missing export, unexpected return code, failed case, or
isolation check returns nonzero; nothing is silently skipped. Cases print
`PASS case_name` or `FAIL case_name: evidence`, followed by a success summary only
when all requested cases pass. Independent cases continue after failures to
collect evidence; no runtime fixes are applied by this runner.

The binary accepts `runtime_e2e --shim ABSOLUTE_PATH [--reconnect]` or
`runtime_e2e --shim-env [--reconnect]` with `RUNTIME_E2E_SHIM=ABSOLUTE_PATH`, but
invoke the script: it supplies validated configuration and copies the executable
under the name `Plex Media Server`, required by the Linux constructor's process
filter. The script uses `--shim-env` because the current Linux filter scans for
the last slash across the entire command line, including arguments. Keeping the
absolute artifact path in environment prevents it from being mistaken for the
executable name.
The selected shared artifact is loaded with `dlopen` and `RTLD_GLOBAL`, matching
the global lookup scope of Linux preload and letting musl resolve real SQLite
dependencies through the shim's `RTLD_NEXT`; public SQLite ABI functions
are obtained from that artifact with `dlsym`. Linux exports `sqlite3_*`; macOS
exports the `my_sqlite3_*` replacement functions used by fishhook, with the same
SQLite ABI. No translator, private Rust runtime helper, or independent behavioral
model is called. The open function's `dladdr` provenance must match the requested
artifact. PostgreSQL creates the fixture and independently checks persistence;
all operations under test go through the shim. A PostgreSQL-only table proves
library opens are redirected rather than accidentally using shadow SQLite.
Linux linking retains the existing system SQLite dependency with
`--no-as-needed` so the constructor's `RTLD_NEXT` lookup has real SQLite to find.

## Coverage

- Normal Plex library filename/open and PostgreSQL-only table visibility.
- Real SQLite RTree construction/root-node access, with engine-internal backing
  tables kept off PostgreSQL and schema-qualified internal reads preserved.
- Prepared writes, bind/reset/clear reuse and re-prepare cache paths.
- Real PostgreSQL UNIQUE, NOT NULL, and foreign-key errors, SQLite primary and
  extended codes, nonempty diagnostics, and subsequent connection usability.
- Two simultaneously open handles and both API/SQL last-insert-rowid isolation.
- BEGIN, multiple writes, cross-handle invisibility, ROLLBACK,
  SAVEPOINT, ROLLBACK TO, RELEASE, and COMMIT persistence.
- Aborted COMMIT rejection, and the exported `sqlite3_exec` ABI's transaction,
  constraint-diagnostic, error-buffer release, and persistence behavior.
- NULL storage class, large signed int64, binary BLOB including NUL/high bytes,
  and integer-bound PostgreSQL boolean roundtrips with independent PG verification.
- Repeated `sqlite3_column_int`, `sqlite3_column_int64` and `sqlite3_column_double`
  reads of numeric results through the actual shared-artifact ABI.
- Prepared and `sqlite3_exec` maintenance no-ops (VACUUM, REINDEX and PRAGMA
  optimize), with independent verification that PostgreSQL library data is unchanged.
- Optional real backend disconnect/reconnect, terminal errors after interrupted
  streaming reads without replay or false DONE, and fail-closed active transaction
  interruption without partial writes. Only fixture-owned backends are terminated.
- SELECT result exhaustion requires ROW followed by DONE; checked separately so
  an exhaustion defect cannot hide transaction/type coverage.

## Isolation and CI

Inherited `PLEX_PG_*`, preload variables, and PostgreSQL service/default variables
are cleared. The local cluster listens only on its private temporary Unix socket.
A unique `runtime_e2e_*` database and login role are created with a database comment
marker. The driver checks explicit host/port/database/user/password/schema,
connection identity, comment marker, and temporary directory marker **before**
loading the shim. It rejects production names/defaults and remote hosts. The
driver deliberately refuses to reuse an existing schema.
Shim logs live inside the fixture directory; on failure the script emits the
tail of shim and local PostgreSQL logs before cleanup.

For a disposable CI PostgreSQL service only, set `RUNTIME_E2E_EXTERNAL_FIXTURE=1`
and all five `RUNTIME_E2E_ADMIN_HOST`, `RUNTIME_E2E_ADMIN_PORT`,
`RUNTIME_E2E_ADMIN_USER`, `RUNTIME_E2E_ADMIN_PASSWORD`, and
`RUNTIME_E2E_ADMIN_DATABASE` values. Host must be `127.0.0.1`; this is an explicit
attestation that the service is disposable, not permission to use production.
The admin user and database must also use `runtime_e2e_` names.
The script creates and drops only its uniquely named database/role. The workflow
uses separate PostgreSQL 15 and 18 service jobs and treats all failures as failures.

No new dependencies or C implementation are introduced. The existing Makefile
compiler invocation links the Rust static library into the shared artifact.

On musl Linux, only the E2E executable is built with `-crt-static` disabled:
musl's statically linked executable cannot `dlopen` the shared shim. The runner
preserves existing Rust flags and scopes this adjustment to its binary build;
it does not change production shim build settings.
